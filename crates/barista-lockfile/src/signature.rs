// SPDX-License-Identifier: MIT OR Apache-2.0

//! Project signature.
//!
//! The lockfile's `project_signature` field is a SHA-256 hex digest
//! computed over the canonicalized effective POMs of every module in
//! the reactor. It serves the `--frozen` validation mode: if the
//! signature in the on-disk lockfile doesn't match the signature
//! computed from the current source tree, the lockfile is stale and
//! resolution must be re-run.
//!
//! Canonicalization rules — applied in order:
//!
//! 1. Sort reactor modules by `groupId:artifactId`.
//! 2. Per module: serialize the [`RawPom`] via a stable bincode
//!    encoding (NOT TOML; TOML map key order is unstable across
//!    serializer versions).
//! 3. Each module's encoded bytes are prefixed with the length (u32
//!    little-endian) before being fed into the rolling hash. The
//!    length prefix prevents boundary collisions: two different
//!    partitions of the same byte stream into modules would otherwise
//!    hash identically.
//! 4. The aggregate hash is the SHA-256 of the concatenated
//!    length-prefixed module bytes.
//!
//! The signature is stable iff the inputs are stable. [`RawPom`] uses
//! `IndexMap` for properties (M1.2), so insertion order is preserved.
//! bincode 2 emits a deterministic byte sequence for `serde`
//! structures.

use barista_pom::raw::RawPom;
use sha2::{Digest, Sha256};

/// Errors produced while computing a project signature.
#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    /// The bincode encoder rejected a [`RawPom`]. This is not expected
    /// for any well-formed POM the parser would produce, but is
    /// surfaced rather than panicked on for defence in depth.
    #[error("bincode encode error: {detail}")]
    Encode { detail: String },
}

/// One module of the reactor. The `group_id` / `artifact_id` pair is
/// used to sort modules into a canonical order before hashing; the
/// `pom` itself is what actually contributes to the hash.
///
/// `version` is intentionally excluded from the sort key: the
/// signature is computed pre-resolution, before any `${revision}`-
/// style version property has been pinned, so versions may not yet be
/// stable. The `groupId:artifactId` pair is unique within a reactor.
#[derive(Debug, Clone)]
pub struct ReactorModule {
    /// The module's `groupId` (possibly inherited from its parent in
    /// the effective POM).
    pub group_id: String,
    /// The module's `artifactId`.
    pub artifact_id: String,
    /// The effective `RawPom` for the module. Callers are expected to
    /// pass an already-resolved POM (parent merge + interpolation +
    /// depMgt + profile activation applied) so the hash reflects the
    /// build inputs as seen by the resolver.
    pub pom: RawPom,
}

impl ReactorModule {
    /// Canonical sort key: `groupId:artifactId`.
    fn sort_key(&self) -> String {
        format!("{}:{}", self.group_id, self.artifact_id)
    }
}

/// Compute the SHA-256 project signature over a reactor's effective
/// POMs. Returns a 64-character lowercase hex string.
pub fn compute_signature(modules: &[ReactorModule]) -> Result<String, SignatureError> {
    // 1. Sort by groupId:artifactId so two reactors that list the
    //    same modules in different orders hash identically.
    let mut sorted: Vec<&ReactorModule> = modules.iter().collect();
    sorted.sort_by_key(|m| m.sort_key());

    // 2. Stream each module into the SHA-256 hasher.
    let mut hasher = Sha256::new();
    let cfg = bincode::config::standard();
    for module in sorted {
        let bytes = bincode::serde::encode_to_vec(&module.pom, cfg).map_err(|e| {
            SignatureError::Encode {
                detail: format!("{e}"),
            }
        })?;
        // 3. Length-prefix so module boundaries are unambiguous.
        let len = bytes.len() as u32;
        hasher.update(len.to_le_bytes());
        hasher.update(&bytes);
    }

    Ok(hex(&hasher.finalize()))
}

/// Render a byte slice as a lowercase hex string. Avoids pulling in a
/// dedicated `hex` crate for a single use.
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(*b >> 4) as usize] as char);
        s.push(HEX[(*b & 0xf) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use barista_pom::raw::{Properties, RawDependency, RawPom};

    /// Build a minimal module with the given coordinates.
    fn module(group: &str, artifact: &str, version: &str) -> ReactorModule {
        let mut pom = RawPom {
            model_version: "4.0.0".to_string(),
            group_id: Some(group.to_string()),
            artifact_id: artifact.to_string(),
            version: Some(version.to_string()),
            packaging: "jar".to_string(),
            ..RawPom::default()
        };
        // Default `Properties` is fine; set explicitly for clarity.
        pom.properties = Properties::default();
        ReactorModule {
            group_id: group.to_string(),
            artifact_id: artifact.to_string(),
            pom,
        }
    }

    /// SHA-256 of the empty byte string.
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn empty_reactor_is_sha256_of_empty_input() {
        let sig = compute_signature(&[]).unwrap();
        assert_eq!(sig, EMPTY_SHA256);
    }

    #[test]
    fn single_module_produces_64_char_hex() {
        let sig = compute_signature(&[module("com.example", "foo", "1.0.0")]).unwrap();
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(sig, EMPTY_SHA256);
    }

    #[test]
    fn determinism_same_input_same_signature() {
        let modules = [
            module("com.example", "foo", "1.0.0"),
            module("com.example", "bar", "2.0.0"),
        ];
        let a = compute_signature(&modules).unwrap();
        let b = compute_signature(&modules).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn adding_a_module_changes_the_signature() {
        let one = [module("com.example", "foo", "1.0.0")];
        let two = [
            module("com.example", "foo", "1.0.0"),
            module("com.example", "bar", "2.0.0"),
        ];
        assert_ne!(
            compute_signature(&one).unwrap(),
            compute_signature(&two).unwrap()
        );
    }

    #[test]
    fn module_order_does_not_matter() {
        let a = [
            module("com.example", "foo", "1.0.0"),
            module("com.example", "bar", "2.0.0"),
        ];
        let b = [
            module("com.example", "bar", "2.0.0"),
            module("com.example", "foo", "1.0.0"),
        ];
        assert_eq!(
            compute_signature(&a).unwrap(),
            compute_signature(&b).unwrap()
        );
    }

    #[test]
    fn changing_group_id_changes_the_signature() {
        let a = [module("com.example", "foo", "1.0.0")];
        let b = [module("org.example", "foo", "1.0.0")];
        assert_ne!(
            compute_signature(&a).unwrap(),
            compute_signature(&b).unwrap()
        );
    }

    #[test]
    fn changing_artifact_id_changes_the_signature() {
        let a = [module("com.example", "foo", "1.0.0")];
        let b = [module("com.example", "bar", "1.0.0")];
        assert_ne!(
            compute_signature(&a).unwrap(),
            compute_signature(&b).unwrap()
        );
    }

    #[test]
    fn changing_version_changes_the_signature() {
        let a = [module("com.example", "foo", "1.0.0")];
        let b = [module("com.example", "foo", "1.0.1")];
        assert_ne!(
            compute_signature(&a).unwrap(),
            compute_signature(&b).unwrap()
        );
    }

    #[test]
    fn changing_a_dependency_coordinate_changes_the_signature() {
        let mut a = module("com.example", "foo", "1.0.0");
        a.pom.dependencies.push(RawDependency {
            group_id: "org.lib".to_string(),
            artifact_id: "lib-core".to_string(),
            version: Some("3.0.0".to_string()),
            ..RawDependency::default()
        });

        let mut b = module("com.example", "foo", "1.0.0");
        b.pom.dependencies.push(RawDependency {
            group_id: "org.lib".to_string(),
            artifact_id: "lib-core".to_string(),
            version: Some("3.0.1".to_string()),
            ..RawDependency::default()
        });

        assert_ne!(
            compute_signature(&[a]).unwrap(),
            compute_signature(&[b]).unwrap()
        );
    }

    #[test]
    fn adding_a_property_changes_the_signature() {
        let baseline = module("com.example", "foo", "1.0.0");
        let mut with_prop = module("com.example", "foo", "1.0.0");
        with_prop
            .pom
            .properties
            .entries
            .insert("java.version".to_string(), "21".to_string());

        assert_ne!(
            compute_signature(&[baseline]).unwrap(),
            compute_signature(&[with_prop]).unwrap()
        );
    }

    #[test]
    fn length_prefixing_prevents_boundary_collisions() {
        // Two different two-module reactors where the concatenation of
        // their per-module bincode payloads — without the length
        // prefix — could plausibly look the same to a naive hasher.
        // With the length prefix, the boundary is preserved.
        //
        // We don't try to construct a true boundary-collision pair
        // (that would require crafting bincode payloads); we just
        // assert that two reactors that share total payload mass but
        // partition it differently still hash differently. The check
        // is: distribute the "differentiating" content into the
        // artifactId of module A vs. module B and confirm the digest
        // moves.
        let left = [
            module("com.example", "aa", "1.0.0"),
            module("com.example", "bbbb", "1.0.0"),
        ];
        let right = [
            module("com.example", "aaaa", "1.0.0"),
            module("com.example", "bb", "1.0.0"),
        ];
        assert_ne!(
            compute_signature(&left).unwrap(),
            compute_signature(&right).unwrap()
        );
    }

    #[test]
    fn signature_is_valid_64_char_lowercase_hex() {
        let sig = compute_signature(&[module("com.example", "foo", "1.0.0")]).unwrap();
        assert_eq!(sig.len(), 64);
        assert!(
            sig.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "expected lowercase hex, got {sig:?}"
        );
    }
}
