//! Raw POM parser — XML → typed struct.
//!
//! This parser is intentionally syntactic: it deserializes the
//! pom.xml content into a typed struct without interpreting any
//! semantics (no parent-chain merge, no property interpolation, no
//! `dependencyManagement` application, no profile activation). Those
//! passes live in sibling modules.
//!
//! Accepts both `<modelVersion>4.0.0</modelVersion>` (Maven 3
//! default) and `<modelVersion>4.1.0</modelVersion>` (Maven 4) input.
//! When `<modelVersion>` is absent, defaults to `4.0.0` (matching
//! Maven's own behaviour for legacy POMs).
//!
//! ## Design
//!
//! The parser is event-driven on top of [`quick_xml::Reader`] rather
//! than serde-derive, because real-world POMs contain free-form
//! `<configuration>` blocks (plugin configuration) whose schema
//! varies per plugin. These are captured as a recursive [`XmlValue`]
//! tree so downstream interpreters can read them without losing
//! information.
//!
//! ## Edge cases
//!
//! - XML namespaces (`xmlns="http://maven.apache.org/POM/4.0.0"`):
//!   ignored. Element names are matched on local name only.
//! - Comments and processing instructions: ignored.
//! - CDATA sections: their content is concatenated with surrounding
//!   text.
//! - Property references (`${foo.bar}`): preserved verbatim.
//!   Interpolation happens in the effective-POM pass.
//! - Unknown elements: skipped silently (the schema is large and we
//!   only model what the resolver and build pipeline consume).

use std::io::BufRead;

use indexmap::IndexMap;
use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::{BytesStart, BytesText, Event};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Public data model
// ---------------------------------------------------------------------------

/// Top-level parsed pom.xml content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawPom {
    pub model_version: String,
    pub parent: Option<RawParent>,

    pub group_id: Option<String>,
    pub artifact_id: String,
    pub version: Option<String>,
    pub packaging: String,

    pub name: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
    pub inception_year: Option<String>,

    pub properties: Properties,

    pub dependency_management: Option<DependencyManagement>,
    pub dependencies: Vec<RawDependency>,

    pub modules: Vec<String>,

    pub build: Option<RawBuild>,

    pub profiles: Vec<RawProfile>,

    pub repositories: Vec<RawRepository>,
    pub plugin_repositories: Vec<RawRepository>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawParent {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    pub relative_path: Option<String>,
}

/// Ordered map of `<properties>` entries. Order matters for
/// interpolation, so an `IndexMap` is used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Properties {
    pub entries: IndexMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DependencyManagement {
    pub dependencies: Vec<RawDependency>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawDependency {
    pub group_id: String,
    pub artifact_id: String,
    pub version: Option<String>,
    pub scope: Option<String>,
    pub classifier: Option<String>,
    /// Maven's `<type>`. Often `jar` (default), `pom`, `war`, `bundle`, etc.
    pub r#type: Option<String>,
    pub system_path: Option<String>,
    pub optional: Option<String>,
    pub exclusions: Vec<RawExclusion>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawExclusion {
    pub group_id: String,
    pub artifact_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawBuild {
    pub source_directory: Option<String>,
    pub script_source_directory: Option<String>,
    pub test_source_directory: Option<String>,
    pub output_directory: Option<String>,
    pub test_output_directory: Option<String>,
    pub final_name: Option<String>,
    pub default_goal: Option<String>,
    pub directory: Option<String>,
    pub filters: Vec<String>,
    pub resources: Vec<RawResource>,
    pub test_resources: Vec<RawResource>,
    pub plugins: Vec<RawPlugin>,
    pub plugin_management: Option<RawPluginManagement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawPluginManagement {
    pub plugins: Vec<RawPlugin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawResource {
    pub directory: Option<String>,
    pub target_path: Option<String>,
    pub filtering: Option<String>,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawPlugin {
    /// Defaults to `org.apache.maven.plugins` when omitted.
    pub group_id: String,
    pub artifact_id: String,
    pub version: Option<String>,
    pub inherited: Option<String>,
    pub extensions: Option<String>,
    /// Free-form `<configuration>` block. Preserved as a generic
    /// XML tree because the schema is plugin-specific.
    pub configuration: Option<XmlValue>,
    pub dependencies: Vec<RawDependency>,
    pub executions: Vec<RawPluginExecution>,
}

impl Default for RawPlugin {
    fn default() -> Self {
        Self {
            group_id: "org.apache.maven.plugins".to_string(),
            artifact_id: String::new(),
            version: None,
            inherited: None,
            extensions: None,
            configuration: None,
            dependencies: Vec::new(),
            executions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawPluginExecution {
    pub id: Option<String>,
    pub phase: Option<String>,
    pub inherited: Option<String>,
    pub goals: Vec<String>,
    pub configuration: Option<XmlValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawProfile {
    pub id: Option<String>,
    pub activation: Option<RawActivation>,
    pub properties: Properties,
    pub dependencies: Vec<RawDependency>,
    pub dependency_management: Option<DependencyManagement>,
    pub modules: Vec<String>,
    pub build: Option<RawBuild>,
    pub repositories: Vec<RawRepository>,
    pub plugin_repositories: Vec<RawRepository>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawActivation {
    pub active_by_default: Option<String>,
    pub jdk: Option<String>,
    pub os: Option<XmlValue>,
    pub property: Option<RawActivationProperty>,
    pub file: Option<RawActivationFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawActivationProperty {
    pub name: Option<String>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawActivationFile {
    pub exists: Option<String>,
    pub missing: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawRepository {
    pub id: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub layout: Option<String>,
    pub releases: Option<RawRepositoryPolicy>,
    pub snapshots: Option<RawRepositoryPolicy>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RawRepositoryPolicy {
    pub enabled: Option<String>,
    pub update_policy: Option<String>,
    pub checksum_policy: Option<String>,
}

/// Recursive XML tree, used to preserve free-form plugin
/// `<configuration>` blocks (and similar) without interpretation.
///
/// An `Element` carries optional attributes (rarely used in POMs but
/// preserved for fidelity), child elements (which may repeat), and
/// optional text content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct XmlValue {
    /// Attributes on this element, in document order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<(String, String)>,
    /// Direct text content (excluding text inside child elements).
    /// `None` when the element has no text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Child elements, grouped by local name, preserving document
    /// order within each group.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub children: IndexMap<String, Vec<XmlValue>>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors produced by the raw POM parser.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Underlying XML reader error.
    #[error("XML parse error: {0}")]
    Xml(#[from] quick_xml::Error),
    /// XML entity unescape error.
    #[error("XML escape error: {0}")]
    Escape(#[from] quick_xml::escape::EscapeError),
    /// XML byte-encoding error.
    #[error("XML encoding error: {0}")]
    Encoding(#[from] quick_xml::encoding::EncodingError),
    /// I/O error reading from the input.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Required element missing.
    #[error("missing required element: {element:?}")]
    MissingElement { element: &'static str },
    /// Root element is not `<project>`.
    #[error("unexpected root element: expected `project`, found {found:?}")]
    UnexpectedRoot { found: String },
    /// Unsupported `<modelVersion>`.
    #[error("unsupported modelVersion: {found:?} (expected 4.0.0 or 4.1.0)")]
    UnsupportedModelVersion { found: String },
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse a pom.xml document from a string.
pub fn parse_pom(xml: &str) -> Result<RawPom, ParseError> {
    let mut reader = Reader::from_str(xml);
    configure(&mut reader);
    parse_inner(&mut reader)
}

/// Parse a pom.xml document from an `impl BufRead`.
pub fn parse_pom_reader<R: BufRead>(r: R) -> Result<RawPom, ParseError> {
    let mut reader = Reader::from_reader(r);
    configure(&mut reader);
    parse_inner(&mut reader)
}

fn configure<R>(reader: &mut Reader<R>) {
    let cfg = reader.config_mut();
    cfg.trim_text(true);
    cfg.expand_empty_elements = true;
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

trait XmlRead {
    fn read_event_buf<'a>(&mut self, buf: &'a mut Vec<u8>) -> Result<Event<'a>, quick_xml::Error>;
}

impl<R: BufRead> XmlRead for Reader<R> {
    fn read_event_buf<'a>(&mut self, buf: &'a mut Vec<u8>) -> Result<Event<'a>, quick_xml::Error> {
        self.read_event_into(buf)
    }
}

fn parse_inner<R: XmlRead>(reader: &mut R) -> Result<RawPom, ParseError> {
    let mut buf = Vec::new();
    loop {
        let owned = {
            let ev = reader.read_event_buf(&mut buf)?;
            match ev {
                Event::Decl(_) | Event::PI(_) | Event::Comment(_) | Event::Text(_) => None,
                Event::Start(e) => Some(local_name(e.name().as_ref())),
                Event::Empty(_) => return Err(ParseError::MissingElement { element: "project" }),
                Event::Eof => return Err(ParseError::MissingElement { element: "project" }),
                _ => None,
            }
        };
        buf.clear();
        if let Some(name) = owned {
            if name != "project" {
                return Err(ParseError::UnexpectedRoot { found: name });
            }
            let pom = parse_project(reader)?;
            validate_model_version(&pom.model_version)?;
            return Ok(pom);
        }
    }
}

fn validate_model_version(mv: &str) -> Result<(), ParseError> {
    match mv {
        "4.0.0" | "4.1.0" => Ok(()),
        other => Err(ParseError::UnsupportedModelVersion {
            found: other.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Per-element parsers. Each `parse_*` consumes through the matching
// end tag of the element whose Start event was just observed.
// ---------------------------------------------------------------------------

fn parse_project<R: XmlRead>(reader: &mut R) -> Result<RawPom, ParseError> {
    let mut pom = RawPom {
        packaging: "jar".to_string(),
        model_version: "4.0.0".to_string(),
        ..RawPom::default()
    };
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                drop(e);
                buf.clear();
                match name.as_str() {
                    "modelVersion" => pom.model_version = read_text(reader)?,
                    "parent" => pom.parent = Some(parse_parent(reader)?),
                    "groupId" => pom.group_id = Some(read_text(reader)?),
                    "artifactId" => pom.artifact_id = read_text(reader)?,
                    "version" => pom.version = Some(read_text(reader)?),
                    "packaging" => pom.packaging = read_text(reader)?,
                    "name" => pom.name = Some(read_text(reader)?),
                    "description" => pom.description = Some(read_text(reader)?),
                    "url" => pom.url = Some(read_text(reader)?),
                    "inceptionYear" => pom.inception_year = Some(read_text(reader)?),
                    "properties" => pom.properties = parse_properties(reader)?,
                    "dependencies" => pom.dependencies = parse_dependencies(reader)?,
                    "dependencyManagement" => {
                        pom.dependency_management = Some(parse_dependency_management(reader)?);
                    }
                    "modules" => pom.modules = parse_string_list(reader, "module")?,
                    "build" => pom.build = Some(parse_build(reader)?),
                    "profiles" => pom.profiles = parse_profiles(reader)?,
                    "repositories" => pom.repositories = parse_repositories(reader, "repository")?,
                    "pluginRepositories" => {
                        pom.plugin_repositories = parse_repositories(reader, "pluginRepository")?;
                    }
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                if pom.artifact_id.is_empty() {
                    return Err(ParseError::MissingElement {
                        element: "artifactId",
                    });
                }
                return Ok(pom);
            }
            Event::Eof => return Err(ParseError::MissingElement { element: "project" }),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_parent<R: XmlRead>(reader: &mut R) -> Result<RawParent, ParseError> {
    let mut p = RawParent::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "groupId" => p.group_id = read_text(reader)?,
                    "artifactId" => p.artifact_id = read_text(reader)?,
                    "version" => p.version = read_text(reader)?,
                    "relativePath" => p.relative_path = Some(read_text(reader)?),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(p);
            }
            Event::Eof => return Err(ParseError::MissingElement { element: "parent" }),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_properties<R: XmlRead>(reader: &mut R) -> Result<Properties, ParseError> {
    let mut entries: IndexMap<String, String> = IndexMap::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                let text = read_text(reader)?;
                entries.insert(name, text);
            }
            Event::End(_) => {
                buf.clear();
                return Ok(Properties { entries });
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "properties",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_dependencies<R: XmlRead>(reader: &mut R) -> Result<Vec<RawDependency>, ParseError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == "dependency" {
                    out.push(parse_dependency(reader)?);
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "dependencies",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_dependency<R: XmlRead>(reader: &mut R) -> Result<RawDependency, ParseError> {
    let mut d = RawDependency::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "groupId" => d.group_id = read_text(reader)?,
                    "artifactId" => d.artifact_id = read_text(reader)?,
                    "version" => d.version = Some(read_text(reader)?),
                    "scope" => d.scope = Some(read_text(reader)?),
                    "classifier" => d.classifier = Some(read_text(reader)?),
                    "type" => d.r#type = Some(read_text(reader)?),
                    "systemPath" => d.system_path = Some(read_text(reader)?),
                    "optional" => d.optional = Some(read_text(reader)?),
                    "exclusions" => d.exclusions = parse_exclusions(reader)?,
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(d);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "dependency",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_exclusions<R: XmlRead>(reader: &mut R) -> Result<Vec<RawExclusion>, ParseError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == "exclusion" {
                    out.push(parse_exclusion(reader)?);
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "exclusions",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_exclusion<R: XmlRead>(reader: &mut R) -> Result<RawExclusion, ParseError> {
    let mut x = RawExclusion::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "groupId" => x.group_id = read_text(reader)?,
                    "artifactId" => x.artifact_id = read_text(reader)?,
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(x);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "exclusion",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_dependency_management<R: XmlRead>(
    reader: &mut R,
) -> Result<DependencyManagement, ParseError> {
    let mut dm = DependencyManagement::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == "dependencies" {
                    dm.dependencies = parse_dependencies(reader)?;
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(dm);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "dependencyManagement",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_string_list<R: XmlRead>(
    reader: &mut R,
    item_name: &'static str,
) -> Result<Vec<String>, ParseError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == item_name {
                    out.push(read_text(reader)?);
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement { element: item_name });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_build<R: XmlRead>(reader: &mut R) -> Result<RawBuild, ParseError> {
    let mut b = RawBuild::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "sourceDirectory" => b.source_directory = Some(read_text(reader)?),
                    "scriptSourceDirectory" => {
                        b.script_source_directory = Some(read_text(reader)?);
                    }
                    "testSourceDirectory" => b.test_source_directory = Some(read_text(reader)?),
                    "outputDirectory" => b.output_directory = Some(read_text(reader)?),
                    "testOutputDirectory" => b.test_output_directory = Some(read_text(reader)?),
                    "finalName" => b.final_name = Some(read_text(reader)?),
                    "defaultGoal" => b.default_goal = Some(read_text(reader)?),
                    "directory" => b.directory = Some(read_text(reader)?),
                    "filters" => b.filters = parse_string_list(reader, "filter")?,
                    "resources" => b.resources = parse_resources(reader, "resource")?,
                    "testResources" => b.test_resources = parse_resources(reader, "testResource")?,
                    "plugins" => b.plugins = parse_plugins(reader)?,
                    "pluginManagement" => {
                        b.plugin_management = Some(parse_plugin_management(reader)?);
                    }
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(b);
            }
            Event::Eof => return Err(ParseError::MissingElement { element: "build" }),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_plugin_management<R: XmlRead>(reader: &mut R) -> Result<RawPluginManagement, ParseError> {
    let mut pm = RawPluginManagement::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == "plugins" {
                    pm.plugins = parse_plugins(reader)?;
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(pm);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "pluginManagement",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_resources<R: XmlRead>(
    reader: &mut R,
    item: &'static str,
) -> Result<Vec<RawResource>, ParseError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == item {
                    out.push(parse_resource(reader)?);
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => return Err(ParseError::MissingElement { element: item }),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_resource<R: XmlRead>(reader: &mut R) -> Result<RawResource, ParseError> {
    let mut r = RawResource::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "directory" => r.directory = Some(read_text(reader)?),
                    "targetPath" => r.target_path = Some(read_text(reader)?),
                    "filtering" => r.filtering = Some(read_text(reader)?),
                    "includes" => r.includes = parse_string_list(reader, "include")?,
                    "excludes" => r.excludes = parse_string_list(reader, "exclude")?,
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(r);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "resource",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_plugins<R: XmlRead>(reader: &mut R) -> Result<Vec<RawPlugin>, ParseError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == "plugin" {
                    out.push(parse_plugin(reader)?);
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => return Err(ParseError::MissingElement { element: "plugins" }),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_plugin<R: XmlRead>(reader: &mut R) -> Result<RawPlugin, ParseError> {
    let mut p = RawPlugin::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "groupId" => p.group_id = read_text(reader)?,
                    "artifactId" => p.artifact_id = read_text(reader)?,
                    "version" => p.version = Some(read_text(reader)?),
                    "inherited" => p.inherited = Some(read_text(reader)?),
                    "extensions" => p.extensions = Some(read_text(reader)?),
                    "configuration" => p.configuration = Some(parse_xml_value(reader, &[])?),
                    "dependencies" => p.dependencies = parse_dependencies(reader)?,
                    "executions" => p.executions = parse_plugin_executions(reader)?,
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(p);
            }
            Event::Eof => return Err(ParseError::MissingElement { element: "plugin" }),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_plugin_executions<R: XmlRead>(
    reader: &mut R,
) -> Result<Vec<RawPluginExecution>, ParseError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == "execution" {
                    out.push(parse_plugin_execution(reader)?);
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "executions",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_plugin_execution<R: XmlRead>(reader: &mut R) -> Result<RawPluginExecution, ParseError> {
    let mut e = RawPluginExecution::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "id" => e.id = Some(read_text(reader)?),
                    "phase" => e.phase = Some(read_text(reader)?),
                    "inherited" => e.inherited = Some(read_text(reader)?),
                    "goals" => e.goals = parse_string_list(reader, "goal")?,
                    "configuration" => e.configuration = Some(parse_xml_value(reader, &[])?),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(e);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "execution",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_profiles<R: XmlRead>(reader: &mut R) -> Result<Vec<RawProfile>, ParseError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == "profile" {
                    out.push(parse_profile(reader)?);
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "profiles",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_profile<R: XmlRead>(reader: &mut R) -> Result<RawProfile, ParseError> {
    let mut p = RawProfile::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "id" => p.id = Some(read_text(reader)?),
                    "activation" => p.activation = Some(parse_activation(reader)?),
                    "properties" => p.properties = parse_properties(reader)?,
                    "dependencies" => p.dependencies = parse_dependencies(reader)?,
                    "dependencyManagement" => {
                        p.dependency_management = Some(parse_dependency_management(reader)?);
                    }
                    "modules" => p.modules = parse_string_list(reader, "module")?,
                    "build" => p.build = Some(parse_build(reader)?),
                    "repositories" => p.repositories = parse_repositories(reader, "repository")?,
                    "pluginRepositories" => {
                        p.plugin_repositories = parse_repositories(reader, "pluginRepository")?;
                    }
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(p);
            }
            Event::Eof => return Err(ParseError::MissingElement { element: "profile" }),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_activation<R: XmlRead>(reader: &mut R) -> Result<RawActivation, ParseError> {
    let mut a = RawActivation::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "activeByDefault" => a.active_by_default = Some(read_text(reader)?),
                    "jdk" => a.jdk = Some(read_text(reader)?),
                    "os" => a.os = Some(parse_xml_value(reader, &[])?),
                    "property" => {
                        let mut prop = RawActivationProperty::default();
                        let mut inner = Vec::new();
                        loop {
                            let ev = reader.read_event_buf(&mut inner)?;
                            match ev {
                                Event::Start(se) => {
                                    let n = local_name(se.name().as_ref());
                                    inner.clear();
                                    match n.as_str() {
                                        "name" => prop.name = Some(read_text(reader)?),
                                        "value" => prop.value = Some(read_text(reader)?),
                                        _ => skip_element(reader, &n)?,
                                    }
                                }
                                Event::End(_) => {
                                    inner.clear();
                                    break;
                                }
                                Event::Eof => {
                                    return Err(ParseError::MissingElement {
                                        element: "property",
                                    });
                                }
                                _ => {}
                            }
                            inner.clear();
                        }
                        a.property = Some(prop);
                    }
                    "file" => {
                        let mut f = RawActivationFile::default();
                        let mut inner = Vec::new();
                        loop {
                            let ev = reader.read_event_buf(&mut inner)?;
                            match ev {
                                Event::Start(se) => {
                                    let n = local_name(se.name().as_ref());
                                    inner.clear();
                                    match n.as_str() {
                                        "exists" => f.exists = Some(read_text(reader)?),
                                        "missing" => f.missing = Some(read_text(reader)?),
                                        _ => skip_element(reader, &n)?,
                                    }
                                }
                                Event::End(_) => {
                                    inner.clear();
                                    break;
                                }
                                Event::Eof => {
                                    return Err(ParseError::MissingElement { element: "file" });
                                }
                                _ => {}
                            }
                            inner.clear();
                        }
                        a.file = Some(f);
                    }
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(a);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "activation",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_repositories<R: XmlRead>(
    reader: &mut R,
    item: &'static str,
) -> Result<Vec<RawRepository>, ParseError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == item {
                    out.push(parse_repository(reader)?);
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => return Err(ParseError::MissingElement { element: item }),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_repository<R: XmlRead>(reader: &mut R) -> Result<RawRepository, ParseError> {
    let mut r = RawRepository::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "id" => r.id = Some(read_text(reader)?),
                    "name" => r.name = Some(read_text(reader)?),
                    "url" => r.url = Some(read_text(reader)?),
                    "layout" => r.layout = Some(read_text(reader)?),
                    "releases" => r.releases = Some(parse_repo_policy(reader)?),
                    "snapshots" => r.snapshots = Some(parse_repo_policy(reader)?),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(r);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "repository",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_repo_policy<R: XmlRead>(reader: &mut R) -> Result<RawRepositoryPolicy, ParseError> {
    let mut p = RawRepositoryPolicy::default();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "enabled" => p.enabled = Some(read_text(reader)?),
                    "updatePolicy" => p.update_policy = Some(read_text(reader)?),
                    "checksumPolicy" => p.checksum_policy = Some(read_text(reader)?),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(p);
            }
            Event::Eof => {
                return Err(ParseError::MissingElement {
                    element: "repositoryPolicy",
                });
            }
            _ => {}
        }
        buf.clear();
    }
}

// ---------------------------------------------------------------------------
// Free-form XML value (plugin `<configuration>` and similar).
// ---------------------------------------------------------------------------

fn parse_xml_value<R: XmlRead>(
    reader: &mut R,
    attrs: &[(String, String)],
) -> Result<XmlValue, ParseError> {
    let mut value = XmlValue {
        attributes: attrs.to_vec(),
        ..XmlValue::default()
    };
    let mut text_acc = String::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                let inner_attrs = read_attributes(&e);
                buf.clear();
                let child = parse_xml_value(reader, &inner_attrs)?;
                value.children.entry(name).or_default().push(child);
            }
            Event::Text(t) => {
                let s = decode_text(&t)?;
                let s = s.trim();
                if !s.is_empty() {
                    if !text_acc.is_empty() {
                        text_acc.push(' ');
                    }
                    text_acc.push_str(s);
                }
                buf.clear();
            }
            Event::CData(c) => {
                let s = std::str::from_utf8(c.as_ref())
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                text_acc.push_str(s);
                buf.clear();
            }
            Event::End(_) => {
                buf.clear();
                if !text_acc.is_empty() {
                    value.text = Some(text_acc);
                }
                return Ok(value);
            }
            Event::Eof => return Err(ParseError::MissingElement { element: "element" }),
            _ => {
                buf.clear();
            }
        }
    }
}

fn read_attributes(e: &BytesStart<'_>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for attr in e.attributes().with_checks(false).flatten() {
        let key = std::str::from_utf8(attr.key.as_ref())
            .unwrap_or("")
            .to_string();
        let local = key.rsplit_once(':').map_or(key.as_str(), |(_, l)| l);
        let raw = std::str::from_utf8(attr.value.as_ref()).unwrap_or("");
        let value = unescape(raw)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| raw.to_string());
        out.push((local.to_string(), value));
    }
    out
}

/// Decode a `Text` event: byte-level encoding decode then XML entity
/// unescaping.
fn decode_text(t: &BytesText<'_>) -> Result<String, ParseError> {
    let raw = t.decode()?;
    let unescaped = unescape(&raw)?;
    Ok(unescaped.into_owned())
}

// ---------------------------------------------------------------------------
// Low-level helpers.
// ---------------------------------------------------------------------------

/// Read the text content of the currently-open element through its
/// closing tag. Handles text, CDATA, and nested whitespace; errors if
/// a nested element is encountered.
fn read_text<R: XmlRead>(reader: &mut R) -> Result<String, ParseError> {
    let mut out = String::new();
    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Text(t) => {
                let s = decode_text(&t)?;
                out.push_str(&s);
            }
            Event::CData(c) => {
                let s = std::str::from_utf8(c.as_ref())
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                out.push_str(s);
            }
            Event::Start(e) => {
                // Nested element inside what we expected to be a
                // text-only field; skip it for robustness.
                let n = local_name(e.name().as_ref());
                buf.clear();
                skip_element(reader, &n)?;
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => return Err(ParseError::MissingElement { element: "text" }),
            _ => {}
        }
        buf.clear();
    }
}

/// Consume events until the closing tag of the named element (which
/// has already had its Start event read). Tracks nested elements of
/// the same name so we close the right one.
fn skip_element<R: XmlRead>(reader: &mut R, _name: &str) -> Result<(), ParseError> {
    let mut depth: usize = 1;
    let mut buf = Vec::new();
    while depth > 0 {
        let event = reader.read_event_buf(&mut buf)?;
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            Event::Eof => return Err(ParseError::MissingElement { element: "end" }),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// Strip namespace prefix from an XML name and return owned `String`.
fn local_name(name: &[u8]) -> String {
    let s = std::str::from_utf8(name).unwrap_or("");
    s.rsplit_once(':').map_or(s, |(_, l)| l).to_string()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"<?xml version="1.0"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>thing</artifactId>
  <version>1.0</version>
</project>"#;

    #[test]
    fn parses_minimal_pom() {
        let pom = parse_pom(MINIMAL).expect("parses");
        assert_eq!(pom.model_version, "4.0.0");
        assert_eq!(pom.group_id.as_deref(), Some("com.example"));
        assert_eq!(pom.artifact_id, "thing");
        assert_eq!(pom.version.as_deref(), Some("1.0"));
        assert_eq!(pom.packaging, "jar");
    }

    #[test]
    fn accepts_maven_4_model_version() {
        let xml = MINIMAL.replace("4.0.0", "4.1.0");
        let pom = parse_pom(&xml).expect("parses");
        assert_eq!(pom.model_version, "4.1.0");
    }

    #[test]
    fn rejects_unsupported_model_version() {
        let xml = MINIMAL.replace("4.0.0", "5.0.0");
        let err = parse_pom(&xml).unwrap_err();
        assert!(matches!(err, ParseError::UnsupportedModelVersion { .. }));
    }

    #[test]
    fn defaults_model_version_when_missing() {
        let xml = r#"<project xmlns="http://maven.apache.org/POM/4.0.0">
          <groupId>g</groupId><artifactId>a</artifactId><version>1</version>
        </project>"#;
        let pom = parse_pom(xml).expect("parses");
        assert_eq!(pom.model_version, "4.0.0");
    }

    #[test]
    fn parses_parent() {
        let xml = r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <parent>
            <groupId>p.g</groupId>
            <artifactId>p.a</artifactId>
            <version>2.0</version>
            <relativePath>../pom.xml</relativePath>
          </parent>
          <artifactId>child</artifactId>
        </project>"#;
        let pom = parse_pom(xml).expect("parses");
        let p = pom.parent.expect("has parent");
        assert_eq!(p.group_id, "p.g");
        assert_eq!(p.artifact_id, "p.a");
        assert_eq!(p.version, "2.0");
        assert_eq!(p.relative_path.as_deref(), Some("../pom.xml"));
    }

    #[test]
    fn parses_dependencies_and_exclusions() {
        let xml = r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <artifactId>a</artifactId>
          <dependencies>
            <dependency>
              <groupId>g1</groupId>
              <artifactId>a1</artifactId>
              <version>1.0</version>
              <scope>test</scope>
              <exclusions>
                <exclusion><groupId>x</groupId><artifactId>y</artifactId></exclusion>
              </exclusions>
            </dependency>
          </dependencies>
        </project>"#;
        let pom = parse_pom(xml).expect("parses");
        assert_eq!(pom.dependencies.len(), 1);
        let d = &pom.dependencies[0];
        assert_eq!(d.group_id, "g1");
        assert_eq!(d.scope.as_deref(), Some("test"));
        assert_eq!(d.exclusions.len(), 1);
        assert_eq!(d.exclusions[0].artifact_id, "y");
    }

    #[test]
    fn parses_properties_in_order() {
        let xml = r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <artifactId>a</artifactId>
          <properties>
            <java.version>17</java.version>
            <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>
          </properties>
        </project>"#;
        let pom = parse_pom(xml).expect("parses");
        let keys: Vec<&str> = pom.properties.entries.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["java.version", "project.build.sourceEncoding"]);
    }

    #[test]
    fn preserves_property_placeholders() {
        let xml = r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <artifactId>a</artifactId>
          <url>${project.organization.url}</url>
        </project>"#;
        let pom = parse_pom(xml).expect("parses");
        assert_eq!(pom.url.as_deref(), Some("${project.organization.url}"));
    }

    #[test]
    fn preserves_plugin_configuration() {
        let xml = r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <artifactId>a</artifactId>
          <build>
            <plugins>
              <plugin>
                <artifactId>maven-compiler-plugin</artifactId>
                <configuration>
                  <source>17</source>
                  <target>17</target>
                  <args>
                    <arg>-Xlint:all</arg>
                    <arg>-Werror</arg>
                  </args>
                </configuration>
              </plugin>
            </plugins>
          </build>
        </project>"#;
        let pom = parse_pom(xml).expect("parses");
        let build = pom.build.expect("build");
        assert_eq!(build.plugins.len(), 1);
        let cfg = build.plugins[0].configuration.as_ref().expect("config");
        assert!(cfg.children.contains_key("source"));
        let args = cfg.children.get("args").expect("args").first().unwrap();
        assert_eq!(args.children.get("arg").map(Vec::len), Some(2));
    }

    #[test]
    fn accepts_namespaced_input() {
        let xml = r#"<project xmlns="http://maven.apache.org/POM/4.0.0"
                              xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
          <modelVersion>4.0.0</modelVersion>
          <artifactId>a</artifactId>
        </project>"#;
        let pom = parse_pom(xml).expect("parses");
        assert_eq!(pom.artifact_id, "a");
    }

    #[test]
    fn accepts_cdata_in_text() {
        let xml = r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <artifactId>a</artifactId>
          <description><![CDATA[Hello & welcome <here>]]></description>
        </project>"#;
        let pom = parse_pom(xml).expect("parses");
        assert_eq!(pom.description.as_deref(), Some("Hello & welcome <here>"));
    }

    #[test]
    fn parses_modules() {
        let xml = r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <artifactId>parent</artifactId>
          <packaging>pom</packaging>
          <modules>
            <module>core</module>
            <module>api</module>
          </modules>
        </project>"#;
        let pom = parse_pom(xml).expect("parses");
        assert_eq!(pom.modules, vec!["core", "api"]);
        assert_eq!(pom.packaging, "pom");
    }

    #[test]
    fn parses_profile_with_activation() {
        let xml = r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <artifactId>a</artifactId>
          <profiles>
            <profile>
              <id>java21</id>
              <activation>
                <jdk>[21,)</jdk>
                <property><name>release</name><value>21</value></property>
              </activation>
              <properties><maven.compiler.release>21</maven.compiler.release></properties>
            </profile>
          </profiles>
        </project>"#;
        let pom = parse_pom(xml).expect("parses");
        assert_eq!(pom.profiles.len(), 1);
        let pr = &pom.profiles[0];
        assert_eq!(pr.id.as_deref(), Some("java21"));
        let act = pr.activation.as_ref().expect("activation");
        assert_eq!(act.jdk.as_deref(), Some("[21,)"));
        assert_eq!(
            act.property.as_ref().and_then(|p| p.name.as_deref()),
            Some("release")
        );
    }

    #[test]
    fn missing_artifact_id_errors() {
        let xml = r#"<project>
          <modelVersion>4.0.0</modelVersion>
          <groupId>g</groupId>
        </project>"#;
        let err = parse_pom(xml).unwrap_err();
        assert!(matches!(
            err,
            ParseError::MissingElement {
                element: "artifactId"
            }
        ));
    }
}
