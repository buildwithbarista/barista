//! CBOR serialization for the effective POM blob.
//!
//! The CLI parses + resolves a project's POM, then ships the resulting
//! [`ResolvedPom`](crate::profile::ResolvedPom) to the barback worker
//! daemon over the IPC socket. CBOR is chosen for the blob format
//! because it is compact, schema-evolvable, and well-supported in both
//! Rust (via `ciborium`) and Java (via `jackson-dataformat-cbor`,
//! which barback already depends on through its Jackson stack).
//!
//! ## Wire layout
//!
//! ```text
//! +----------+----------------------+
//! | 4 bytes  | variable-length CBOR |
//! | "BPOM"   | encoding of PomBlob  |
//! +----------+----------------------+
//! ```
//!
//! The 4-byte magic lets a consumer distinguish "this isn't a blob at
//! all" from "this is the right shape, just the wrong version".
//!
//! ## Schema evolution
//!
//! Bump [`BLOB_SCHEMA_VERSION`] whenever the wire format changes
//! incompatibly. Consumers refuse to deserialize a blob whose version
//! does not match what they were built against. New optional fields
//! can be added without bumping the version provided they carry
//! `#[serde(default)]` and producers omit them when empty.

use crate::profile::ResolvedPom;
use crate::raw::RawPom;
use serde::{Deserialize, Serialize};

/// The schema version for the CBOR blob. Bumped when the wire format
/// changes incompatibly.
pub const BLOB_SCHEMA_VERSION: u32 = 1;

/// Magic byte sequence at the start of every blob. ASCII `"BPOM"` —
/// "Barista POM".
pub const BLOB_MAGIC: &[u8; 4] = b"BPOM";

/// On-the-wire representation of a fully-resolved POM, ready to ship
/// to the barback worker daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PomBlob {
    /// Schema version. Increment when the wire format changes.
    pub schema_version: u32,
    /// The resolved POM payload — parent-merged, interpolated,
    /// BOM-imported, profile-applied, depMgt-applied.
    pub pom: RawPom,
    /// The parent chain used during resolution, ordered from nearest
    /// ancestor to root. Empty for parentless POMs. Useful for
    /// debugging on the daemon side.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parent_chain: Vec<RawPom>,
    /// Ids of profiles that fired during resolution, in document
    /// order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_profile_ids: Vec<String>,
    /// Coordinates (`"group:artifact:version"`) of every BOM whose
    /// `<dependencyManagement>` was spliced in during resolution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imported_boms: Vec<String>,
}

impl PomBlob {
    /// Build a [`PomBlob`] from a [`ResolvedPom`] produced by
    /// [`crate::profile::resolve_pom`].
    pub fn from_resolved(rp: ResolvedPom) -> Self {
        Self {
            schema_version: BLOB_SCHEMA_VERSION,
            pom: rp.pom,
            parent_chain: rp.effective.parent_chain,
            active_profile_ids: rp.active_profile_ids,
            imported_boms: rp.imported_boms,
        }
    }
}

/// Errors produced by [`write_blob`] / [`read_blob`].
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// The byte slice was shorter than the 4-byte magic prefix plus
    /// at least one byte of payload.
    #[error("blob too short (expected at least 4 bytes of magic + 1 byte payload)")]
    TooShort,
    /// The 4-byte magic prefix did not match [`BLOB_MAGIC`].
    #[error("blob magic mismatch (expected {expected:?}, got {got:?})")]
    BadMagic {
        /// The magic the consumer was expecting.
        expected: [u8; 4],
        /// The first four bytes actually seen.
        got: [u8; 4],
    },
    /// The blob deserialized, but its `schema_version` did not match
    /// the consumer's [`BLOB_SCHEMA_VERSION`].
    #[error(
        "blob schema version mismatch (this build understands version {expected}, blob is version {got})"
    )]
    SchemaVersionMismatch {
        /// The version this build was compiled against.
        expected: u32,
        /// The version found in the blob.
        got: u32,
    },
    /// Underlying CBOR encoder / decoder error.
    #[error("CBOR error: {0}")]
    Cbor(String),
    /// I/O error while reading or writing the blob.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Serialize a [`PomBlob`] to its on-the-wire byte form.
///
/// Layout: [`BLOB_MAGIC`] (4 bytes) followed by the CBOR encoding of
/// `blob`.
pub fn write_blob(blob: &PomBlob) -> Result<Vec<u8>, BlobError> {
    let mut out = Vec::with_capacity(2048);
    out.extend_from_slice(BLOB_MAGIC);
    ciborium::ser::into_writer(blob, &mut out).map_err(|e| BlobError::Cbor(format!("{e}")))?;
    Ok(out)
}

/// Deserialize a [`PomBlob`] from its on-the-wire byte form.
///
/// Verifies the magic prefix and that the blob's `schema_version`
/// matches the consumer's [`BLOB_SCHEMA_VERSION`].
pub fn read_blob(bytes: &[u8]) -> Result<PomBlob, BlobError> {
    if bytes.len() < BLOB_MAGIC.len() + 1 {
        return Err(BlobError::TooShort);
    }
    let (magic, payload) = bytes.split_at(BLOB_MAGIC.len());
    // `split_at` guarantees the slice is exactly BLOB_MAGIC.len() long,
    // so the conversion is infallible.
    let got_magic: [u8; 4] = magic.try_into().expect("split_at gives exact 4 bytes");
    if &got_magic != BLOB_MAGIC {
        return Err(BlobError::BadMagic {
            expected: *BLOB_MAGIC,
            got: got_magic,
        });
    }
    let blob: PomBlob =
        ciborium::de::from_reader(payload).map_err(|e| BlobError::Cbor(format!("{e}")))?;
    if blob.schema_version != BLOB_SCHEMA_VERSION {
        return Err(BlobError::SchemaVersionMismatch {
            expected: BLOB_SCHEMA_VERSION,
            got: blob.schema_version,
        });
    }
    Ok(blob)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effective::ParentResolver;
    use crate::profile::{ActivationContext, resolve_pom};
    use crate::raw::{Properties, RawDependency, RawParent, RawPlugin, RawPom, XmlValue};
    use indexmap::IndexMap;

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    /// A resolver that knows about no parents — every `<parent>` lookup
    /// errors out. Suitable for parentless POMs.
    #[derive(Default)]
    struct NoParentResolver;

    impl ParentResolver for NoParentResolver {
        fn resolve(&mut self, parent: &RawParent) -> Result<RawPom, String> {
            Err(format!(
                "no parent resolver configured for {}:{}:{}",
                parent.group_id, parent.artifact_id, parent.version
            ))
        }
    }

    fn dep(group: &str, artifact: &str, version: &str) -> RawDependency {
        RawDependency {
            group_id: group.to_string(),
            artifact_id: artifact.to_string(),
            version: Some(version.to_string()),
            ..RawDependency::default()
        }
    }

    fn small_pom() -> RawPom {
        let mut properties = Properties::default();
        properties
            .entries
            .insert("junit.version".to_string(), "5.11.0".to_string());
        properties
            .entries
            .insert("project.encoding".to_string(), "UTF-8".to_string());

        RawPom {
            model_version: "4.0.0".to_string(),
            group_id: Some("com.example".to_string()),
            artifact_id: "demo".to_string(),
            version: Some("1.2.3".to_string()),
            packaging: "jar".to_string(),
            name: Some("Demo".to_string()),
            properties,
            dependencies: vec![
                dep("org.junit.jupiter", "junit-jupiter", "5.11.0"),
                dep("com.fasterxml.jackson.core", "jackson-databind", "2.18.0"),
                dep("org.slf4j", "slf4j-api", "2.0.13"),
                dep("com.google.guava", "guava", "33.3.0-jre"),
                dep("org.apache.commons", "commons-lang3", "3.17.0"),
            ],
            ..RawPom::default()
        }
    }

    // -----------------------------------------------------------------
    // 1. Round-trip identity on a realistic small POM.
    // -----------------------------------------------------------------

    #[test]
    fn round_trip_identity() {
        let original = PomBlob {
            schema_version: BLOB_SCHEMA_VERSION,
            pom: small_pom(),
            parent_chain: vec![],
            active_profile_ids: vec!["dev".to_string(), "ci".to_string()],
            imported_boms: vec!["io.netty:netty-bom:4.1.115.Final".to_string()],
        };

        let bytes = write_blob(&original).expect("write");
        let decoded = read_blob(&bytes).expect("read");
        assert_eq!(original, decoded);
    }

    // -----------------------------------------------------------------
    // 2. Empty/minimal POM round-trip.
    // -----------------------------------------------------------------

    #[test]
    fn empty_pom_round_trip() {
        let blob = PomBlob {
            schema_version: BLOB_SCHEMA_VERSION,
            pom: RawPom {
                model_version: "4.0.0".to_string(),
                group_id: Some("g".to_string()),
                artifact_id: "a".to_string(),
                version: Some("1".to_string()),
                packaging: "jar".to_string(),
                ..RawPom::default()
            },
            parent_chain: vec![],
            active_profile_ids: vec![],
            imported_boms: vec![],
        };

        let bytes = write_blob(&blob).unwrap();
        let decoded = read_blob(&bytes).unwrap();
        assert_eq!(blob, decoded);
    }

    // -----------------------------------------------------------------
    // 3. Magic check.
    // -----------------------------------------------------------------

    #[test]
    fn bad_magic_rejected() {
        // 10 bytes, none of which are "BPOM".
        let bytes = b"not-a-blob";
        match read_blob(bytes) {
            Err(BlobError::BadMagic { expected, got }) => {
                assert_eq!(&expected, BLOB_MAGIC);
                assert_eq!(&got, b"not-");
            }
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 4. Schema-version mismatch.
    // -----------------------------------------------------------------

    #[test]
    fn schema_version_mismatch_rejected() {
        // Hand-build a blob with a forged schema_version.
        let forged = PomBlob {
            schema_version: 999,
            pom: RawPom {
                model_version: "4.0.0".to_string(),
                artifact_id: "x".to_string(),
                packaging: "jar".to_string(),
                ..RawPom::default()
            },
            parent_chain: vec![],
            active_profile_ids: vec![],
            imported_boms: vec![],
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(BLOB_MAGIC);
        ciborium::ser::into_writer(&forged, &mut bytes).unwrap();

        match read_blob(&bytes) {
            Err(BlobError::SchemaVersionMismatch { expected, got }) => {
                assert_eq!(expected, BLOB_SCHEMA_VERSION);
                assert_eq!(got, 999);
            }
            other => panic!("expected SchemaVersionMismatch, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // 5. Too-short blob.
    // -----------------------------------------------------------------

    #[test]
    fn too_short_rejected() {
        assert!(matches!(read_blob(&[]), Err(BlobError::TooShort)));
        assert!(matches!(read_blob(b"BP"), Err(BlobError::TooShort)));
        assert!(matches!(read_blob(b"BPOM"), Err(BlobError::TooShort)));
    }

    // -----------------------------------------------------------------
    // 6. Free-form <configuration> XmlValue trees round-trip.
    // -----------------------------------------------------------------

    #[test]
    fn xml_value_configuration_round_trip() {
        // Build a nested XmlValue tree, the kind that lives in a
        // plugin's <configuration> block.
        let mut grandchildren: IndexMap<String, Vec<XmlValue>> = IndexMap::new();
        grandchildren.insert(
            "argLine".to_string(),
            vec![XmlValue {
                text: Some("-Xmx2g".to_string()),
                ..XmlValue::default()
            }],
        );
        grandchildren.insert(
            "systemPropertyVariables".to_string(),
            vec![XmlValue {
                children: {
                    let mut m: IndexMap<String, Vec<XmlValue>> = IndexMap::new();
                    m.insert(
                        "user.timezone".to_string(),
                        vec![XmlValue {
                            text: Some("UTC".to_string()),
                            ..XmlValue::default()
                        }],
                    );
                    m
                },
                ..XmlValue::default()
            }],
        );

        let config = XmlValue {
            attributes: vec![("combine.children".to_string(), "append".to_string())],
            text: None,
            children: grandchildren,
        };

        let plugin = RawPlugin {
            group_id: "org.apache.maven.plugins".to_string(),
            artifact_id: "maven-surefire-plugin".to_string(),
            version: Some("3.5.0".to_string()),
            configuration: Some(config),
            ..RawPlugin::default()
        };

        let mut pom = small_pom();
        pom.build = Some(crate::raw::RawBuild {
            plugins: vec![plugin],
            ..crate::raw::RawBuild::default()
        });

        let blob = PomBlob {
            schema_version: BLOB_SCHEMA_VERSION,
            pom,
            parent_chain: vec![],
            active_profile_ids: vec![],
            imported_boms: vec![],
        };

        let bytes = write_blob(&blob).unwrap();
        let decoded = read_blob(&bytes).unwrap();
        assert_eq!(blob, decoded);
        // And a second round-trip is byte-stable.
        let bytes2 = write_blob(&decoded).unwrap();
        assert_eq!(bytes, bytes2);
    }

    // -----------------------------------------------------------------
    // 7. Compactness sanity — a realistic small POM should fit well
    //    under 50 KB.
    // -----------------------------------------------------------------

    #[test]
    fn small_pom_serialized_size_is_reasonable() {
        // Use a parentless POM derived from small_pom so we don't need
        // to wire up a parent resolver for the sample fixture (which
        // declares a <parent>).
        let pom = small_pom();
        let mut resolver = NoParentResolver;
        let ctx = ActivationContext::default();
        let resolved = resolve_pom(pom, &mut resolver, &ctx).expect("resolve");

        let blob = PomBlob::from_resolved(resolved);
        let bytes = write_blob(&blob).expect("write");
        eprintln!("small_pom blob size: {} bytes", bytes.len());

        // Generous ceiling — the fixture is ~4 KB of XML; CBOR should
        // be similar or smaller. If a real POM ever blows this budget
        // we want to know.
        assert!(
            bytes.len() < 50_000,
            "blob is {} bytes, expected < 50000",
            bytes.len()
        );

        // And it should still round-trip.
        let decoded = read_blob(&bytes).expect("read");
        assert_eq!(blob, decoded);
    }

    // -----------------------------------------------------------------
    // 8. Schema version constant locked.
    // -----------------------------------------------------------------

    #[test]
    fn schema_version_is_one() {
        assert_eq!(BLOB_SCHEMA_VERSION, 1);
    }

    // -----------------------------------------------------------------
    // 9. Magic constant length.
    // -----------------------------------------------------------------

    #[test]
    fn magic_constant_length() {
        assert_eq!(BLOB_MAGIC.len(), 4);
        assert_eq!(BLOB_MAGIC, b"BPOM");
    }

    // -----------------------------------------------------------------
    // 10. from_resolved constructor populates fields correctly.
    // -----------------------------------------------------------------

    #[test]
    fn from_resolved_populates_fields() {
        let pom = small_pom();
        let mut resolver = NoParentResolver;
        let ctx = ActivationContext::default();
        let resolved = resolve_pom(pom.clone(), &mut resolver, &ctx).expect("resolve");

        let blob = PomBlob::from_resolved(resolved);

        assert_eq!(blob.schema_version, BLOB_SCHEMA_VERSION);
        assert_eq!(blob.pom.artifact_id, "demo");
        assert_eq!(blob.pom.dependencies.len(), 5);
        assert!(blob.parent_chain.is_empty());
        assert!(blob.active_profile_ids.is_empty());
        assert!(blob.imported_boms.is_empty());
    }

    // -----------------------------------------------------------------
    // 11. Properties (IndexMap) preserves insertion order across
    //     round-trip — important because Maven property interpolation
    //     is order-sensitive.
    // -----------------------------------------------------------------

    #[test]
    fn properties_order_preserved() {
        let mut props = Properties::default();
        // Insert in a specific, non-alphabetical order.
        props.entries.insert("z.last".to_string(), "1".to_string());
        props.entries.insert("a.first".to_string(), "2".to_string());
        props
            .entries
            .insert("m.middle".to_string(), "3".to_string());

        let blob = PomBlob {
            schema_version: BLOB_SCHEMA_VERSION,
            pom: RawPom {
                model_version: "4.0.0".to_string(),
                artifact_id: "ordered".to_string(),
                packaging: "jar".to_string(),
                properties: props.clone(),
                ..RawPom::default()
            },
            parent_chain: vec![],
            active_profile_ids: vec![],
            imported_boms: vec![],
        };

        let bytes = write_blob(&blob).unwrap();
        let decoded = read_blob(&bytes).unwrap();

        let decoded_keys: Vec<&String> = decoded.pom.properties.entries.keys().collect();
        let original_keys: Vec<&String> = props.entries.keys().collect();
        assert_eq!(decoded_keys, original_keys);
    }

    // -----------------------------------------------------------------
    // 12. Magic prefix appears literally at the start of the wire form.
    // -----------------------------------------------------------------

    #[test]
    fn wire_form_starts_with_magic() {
        let blob = PomBlob {
            schema_version: BLOB_SCHEMA_VERSION,
            pom: RawPom {
                model_version: "4.0.0".to_string(),
                artifact_id: "x".to_string(),
                packaging: "jar".to_string(),
                ..RawPom::default()
            },
            parent_chain: vec![],
            active_profile_ids: vec![],
            imported_boms: vec![],
        };
        let bytes = write_blob(&blob).unwrap();
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], BLOB_MAGIC);
    }
}
