// SPDX-License-Identifier: MIT OR Apache-2.0

//! Read-only parser for Maven's `settings.xml` (and `settings-security.xml`).
//!
//! Maven's user-level configuration file at `~/.m2/settings.xml` declares:
//!
//! - `<servers>` — credentials for remote repositories (releases,
//!   snapshots, deploy).
//! - `<mirrors>` — global URL substitutions applied before resolution.
//! - `<profiles>` — additional config (properties, repositories, plugin
//!   repos) gated on activation conditions.
//! - `<activeProfiles>` — explicit profile activations.
//! - `<proxies>` — HTTP proxy declarations.
//! - `<pluginGroups>` — additional groupIds searched when invoking a
//!   plugin by alias.
//!
//! This parser is **read-only**: it ingests an XML file and produces
//! typed Rust structs. Application of mirrors/servers to specific
//! resolution requests is a downstream concern (the resolver and the
//! network layer consume these structs as auxiliary data).
//!
//! ## Encrypted passwords (limitation)
//!
//! Maven 2.1+ supports encrypting server passwords with a master key
//! stored in `~/.m2/settings-security.xml`. The on-disk format wraps
//! the ciphertext in `{...}` braces. The encryption scheme is one of:
//!
//! - PBE/DES (very old Maven releases).
//! - AES-128/CBC/PKCS5Padding with PBKDF2 (Maven 4 / newer
//!   `plexus-cipher`).
//!
//! Full decryption is **not yet implemented**. The parser still
//! reads the raw `{...}` blob into [`Server::password`]; calling
//! [`decrypt_password`] on such a value returns a
//! [`SettingsError::Decryption`] error whose message points users at
//! a workaround (use unencrypted credentials or supply them via
//! environment variables). Plaintext passwords pass through
//! unchanged.
//!
//! See the upstream Maven implementation for reference:
//!
//! - `org.sonatype.plexus.components.cipher.DefaultPlexusCipher`
//! - `org.sonatype.plexus.components.sec.dispatcher.DefaultSecDispatcher`

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::{BytesText, Event};
use serde::{Deserialize, Serialize};

// ===========================================================================
// Data model
// ===========================================================================

/// Parsed Maven `settings.xml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsXml {
    pub servers: Vec<Server>,
    pub mirrors: Vec<Mirror>,
    pub profiles: Vec<XmlProfile>,
    pub active_profile_ids: Vec<String>,
    pub proxies: Vec<Proxy>,
    pub plugin_groups: Vec<String>,
    pub local_repository: Option<String>,
    /// Maven's `<interactiveMode>`. Default `true`.
    pub interactive_mode: bool,
    /// Maven's `<offline>`. Default `false`.
    pub offline: bool,
}

impl Default for SettingsXml {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            mirrors: Vec::new(),
            profiles: Vec::new(),
            active_profile_ids: Vec::new(),
            proxies: Vec::new(),
            plugin_groups: Vec::new(),
            local_repository: None,
            interactive_mode: true,
            offline: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub id: String,
    pub username: Option<String>,
    /// Raw password as it appears in the file. May be plaintext or
    /// `{encrypted}`-wrapped. Use [`decrypt_password`] to resolve.
    pub password: Option<String>,
    pub private_key: Option<String>,
    pub passphrase: Option<String>,
    pub file_permissions: Option<String>,
    pub directory_permissions: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mirror {
    pub id: String,
    pub name: Option<String>,
    pub url: String,
    /// `<mirrorOf>` — typically a repo id, `*`, or a comma-separated
    /// list with optional `!`-negation (e.g. `!internal,*`).
    pub mirror_of: String,
    pub blocked: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XmlProfile {
    pub id: String,
    pub properties: BTreeMap<String, String>,
    pub repositories: Vec<Repository>,
    pub plugin_repositories: Vec<Repository>,
    pub activation: Option<Activation>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    pub name: Option<String>,
    pub url: String,
    pub releases: RepositoryPolicy,
    pub snapshots: RepositoryPolicy,
    pub layout: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryPolicy {
    pub enabled: bool,
    pub update_policy: Option<String>,
    pub checksum_policy: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activation {
    pub active_by_default: bool,
    pub jdk: Option<String>,
    pub os_name: Option<String>,
    pub os_family: Option<String>,
    pub os_arch: Option<String>,
    pub os_version: Option<String>,
    pub property_name: Option<String>,
    pub property_value: Option<String>,
    pub file_exists: Option<String>,
    pub file_missing: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proxy {
    pub id: String,
    pub active: bool,
    pub protocol: String,
    pub host: String,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub non_proxy_hosts: Option<String>,
}

// ===========================================================================
// Errors
// ===========================================================================

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("settings.xml read error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("settings.xml parse error at {path:?}: {detail}")]
    XmlParse { path: PathBuf, detail: String },
    #[error("password decryption error: {detail}")]
    Decryption { detail: String },
    #[error("master password not found at {path:?}")]
    MissingMasterPassword { path: PathBuf },
}

// ===========================================================================
// Public API
// ===========================================================================

/// Parse a `settings.xml` file at `path`.
pub fn parse_settings_xml(path: &Path) -> Result<SettingsXml, SettingsError> {
    let raw = std::fs::read_to_string(path).map_err(|e| SettingsError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    parse_settings_str(&raw).map_err(|detail| SettingsError::XmlParse {
        path: path.to_path_buf(),
        detail,
    })
}

/// Parse `settings.xml` content from a string. Returns a string detail
/// on failure (callers wrap it into [`SettingsError::XmlParse`]).
pub fn parse_settings_str(xml: &str) -> Result<SettingsXml, String> {
    let mut reader = Reader::from_str(xml);
    {
        let cfg = reader.config_mut();
        cfg.trim_text(true);
        cfg.expand_empty_elements = true;
    }
    parse_inner(&mut reader)
}

/// Decrypt a server password.
///
/// - If `raw` is not `{...}`-wrapped, returns it unchanged (the common
///   plaintext case).
/// - If `raw` is `{...}`-wrapped, returns a [`SettingsError::Decryption`]
///   error with a documented workaround. Full decryption is not yet
///   implemented — see the module-level docs.
///
/// The `master_security_path` argument is reserved for the future
/// implementation that will read `settings-security.xml`. It is
/// currently unused.
//
// TODO: implement the Maven decryption pipeline.
//
// Reference: `org.sonatype.plexus.components.cipher.DefaultPlexusCipher`
// and `org.sonatype.plexus.components.sec.dispatcher.DefaultSecDispatcher`.
//
// Outline:
// 1. If master_security_path is None, default to ~/.m2/settings-security.xml.
// 2. Parse <settingsSecurity><master>{...}</master></settingsSecurity>.
// 3. Decrypt the master using the well-known "settings.security" key
//    (PBE-based; Maven literally hardcodes the passphrase
//    "settings.security" unless a relocation file is set).
// 4. Use the decrypted master as the key to decrypt the server password.
// 5. Both layers use the same plexus-cipher format: the {...} blob is
//    a base64 string whose first bytes are a 1-byte salt length, the
//    salt itself, a 1-byte pad length, and the ciphertext.
// 6. Newer plexus-cipher uses AES-128/CBC/PKCS5Padding with PBKDF2;
//    older versions used PBE/DES. Detect via leading byte tag.
pub fn decrypt_password(
    raw: &str,
    _master_security_path: Option<&Path>,
) -> Result<String, SettingsError> {
    let trimmed = raw.trim();
    if !is_encrypted_blob(trimmed) {
        return Ok(raw.to_string());
    }
    Err(SettingsError::Decryption {
        detail: "encrypted passwords not yet supported; configure credentials via \
             environment variables or use unencrypted settings.xml until a \
             future milestone adds master-password decryption"
            .to_string(),
    })
}

/// Recognise a Maven password blob wrapped in `{...}`.
///
/// Maven's plexus-cipher stores ciphertext as `{<base64>}`. A literal
/// brace inside the value is escaped as `\\{` / `\\}` upstream, so a
/// trailing `}` is reliable.
fn is_encrypted_blob(s: &str) -> bool {
    s.starts_with('{') && s.ends_with('}') && s.len() >= 2
}

// ===========================================================================
// Parser (quick-xml event loop, same shape as barista-pom::raw)
// ===========================================================================

fn parse_inner<R: BufRead>(reader: &mut Reader<R>) -> Result<SettingsXml, String> {
    let mut buf = Vec::new();
    // Find the top-level <settings> start tag.
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name != "settings" {
                    // Tolerant: skip non-settings root and try to find
                    // the inner <settings>.
                    skip_element(reader, &name)?;
                    continue;
                }
                return parse_settings(reader);
            }
            Event::Eof => {
                // Empty document is treated as defaults.
                return Ok(SettingsXml::default());
            }
            _ => {}
        }
        buf.clear();
    }
}

fn parse_settings<R: BufRead>(reader: &mut Reader<R>) -> Result<SettingsXml, String> {
    let mut s = SettingsXml::default();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "localRepository" => s.local_repository = Some(read_text(reader)?),
                    "interactiveMode" => s.interactive_mode = read_bool(reader, true)?,
                    "offline" => s.offline = read_bool(reader, false)?,
                    "servers" => s.servers = parse_list(reader, "server", parse_server)?,
                    "mirrors" => s.mirrors = parse_list(reader, "mirror", parse_mirror)?,
                    "profiles" => s.profiles = parse_list(reader, "profile", parse_profile)?,
                    "activeProfiles" => {
                        s.active_profile_ids = parse_string_list(reader, "activeProfile")?;
                    }
                    "proxies" => s.proxies = parse_list(reader, "proxy", parse_proxy)?,
                    "pluginGroups" => {
                        s.plugin_groups = parse_string_list(reader, "pluginGroup")?;
                    }
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(s);
            }
            Event::Eof => return Err("unexpected EOF in <settings>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_list<R, T, F>(
    reader: &mut Reader<R>,
    item: &str,
    mut parse_item: F,
) -> Result<Vec<T>, String>
where
    R: BufRead,
    F: FnMut(&mut Reader<R>) -> Result<T, String>,
{
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == item {
                    out.push(parse_item(reader)?);
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => return Err(format!("unexpected EOF inside <{item}> list")),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_server<R: BufRead>(reader: &mut Reader<R>) -> Result<Server, String> {
    let mut s = Server::default();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "id" => s.id = read_text(reader)?,
                    "username" => s.username = Some(read_text(reader)?),
                    "password" => s.password = Some(read_text(reader)?),
                    "privateKey" => s.private_key = Some(read_text(reader)?),
                    "passphrase" => s.passphrase = Some(read_text(reader)?),
                    "filePermissions" => s.file_permissions = Some(read_text(reader)?),
                    "directoryPermissions" => s.directory_permissions = Some(read_text(reader)?),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(s);
            }
            Event::Eof => return Err("unexpected EOF in <server>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_mirror<R: BufRead>(reader: &mut Reader<R>) -> Result<Mirror, String> {
    let mut m = Mirror::default();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "id" => m.id = read_text(reader)?,
                    "name" => m.name = Some(read_text(reader)?),
                    "url" => m.url = read_text(reader)?,
                    "mirrorOf" => m.mirror_of = read_text(reader)?,
                    "blocked" => m.blocked = parse_bool_str(&read_text(reader)?, false),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(m);
            }
            Event::Eof => return Err("unexpected EOF in <mirror>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_profile<R: BufRead>(reader: &mut Reader<R>) -> Result<XmlProfile, String> {
    let mut p = XmlProfile::default();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "id" => p.id = read_text(reader)?,
                    "properties" => p.properties = parse_properties(reader)?,
                    "activation" => p.activation = Some(parse_activation(reader)?),
                    "repositories" => {
                        p.repositories = parse_list(reader, "repository", parse_repository)?;
                    }
                    "pluginRepositories" => {
                        p.plugin_repositories =
                            parse_list(reader, "pluginRepository", parse_repository)?;
                    }
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(p);
            }
            Event::Eof => return Err("unexpected EOF in <profile>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_properties<R: BufRead>(
    reader: &mut Reader<R>,
) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                let text = read_text(reader)?;
                out.insert(name, text);
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => return Err("unexpected EOF in <properties>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_activation<R: BufRead>(reader: &mut Reader<R>) -> Result<Activation, String> {
    let mut a = Activation::default();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "activeByDefault" => {
                        a.active_by_default = parse_bool_str(&read_text(reader)?, false);
                    }
                    "jdk" => a.jdk = Some(read_text(reader)?),
                    "os" => parse_activation_os(reader, &mut a)?,
                    "property" => parse_activation_property(reader, &mut a)?,
                    "file" => parse_activation_file(reader, &mut a)?,
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(a);
            }
            Event::Eof => return Err("unexpected EOF in <activation>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_activation_os<R: BufRead>(
    reader: &mut Reader<R>,
    a: &mut Activation,
) -> Result<(), String> {
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "name" => a.os_name = Some(read_text(reader)?),
                    "family" => a.os_family = Some(read_text(reader)?),
                    "arch" => a.os_arch = Some(read_text(reader)?),
                    "version" => a.os_version = Some(read_text(reader)?),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(());
            }
            Event::Eof => return Err("unexpected EOF in <os>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_activation_property<R: BufRead>(
    reader: &mut Reader<R>,
    a: &mut Activation,
) -> Result<(), String> {
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "name" => a.property_name = Some(read_text(reader)?),
                    "value" => a.property_value = Some(read_text(reader)?),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(());
            }
            Event::Eof => return Err("unexpected EOF in <property>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_activation_file<R: BufRead>(
    reader: &mut Reader<R>,
    a: &mut Activation,
) -> Result<(), String> {
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "exists" => a.file_exists = Some(read_text(reader)?),
                    "missing" => a.file_missing = Some(read_text(reader)?),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(());
            }
            Event::Eof => return Err("unexpected EOF in <file>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_repository<R: BufRead>(reader: &mut Reader<R>) -> Result<Repository, String> {
    let mut r = Repository::default();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "id" => r.id = read_text(reader)?,
                    "name" => r.name = Some(read_text(reader)?),
                    "url" => r.url = read_text(reader)?,
                    "layout" => r.layout = Some(read_text(reader)?),
                    "releases" => r.releases = parse_repo_policy(reader)?,
                    "snapshots" => r.snapshots = parse_repo_policy(reader)?,
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(r);
            }
            Event::Eof => return Err("unexpected EOF in <repository>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_repo_policy<R: BufRead>(reader: &mut Reader<R>) -> Result<RepositoryPolicy, String> {
    // Maven default for <enabled> when the <releases> / <snapshots>
    // block is present but doesn't specify it is `true`.
    let mut p = RepositoryPolicy {
        enabled: true,
        ..RepositoryPolicy::default()
    };
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "enabled" => p.enabled = parse_bool_str(&read_text(reader)?, true),
                    "updatePolicy" => p.update_policy = Some(read_text(reader)?),
                    "checksumPolicy" => p.checksum_policy = Some(read_text(reader)?),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(p);
            }
            Event::Eof => return Err("unexpected EOF in repo policy".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_proxy<R: BufRead>(reader: &mut Reader<R>) -> Result<Proxy, String> {
    let mut p = Proxy::default();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                match name.as_str() {
                    "id" => p.id = read_text(reader)?,
                    "active" => p.active = parse_bool_str(&read_text(reader)?, false),
                    "protocol" => p.protocol = read_text(reader)?,
                    "host" => p.host = read_text(reader)?,
                    "port" => {
                        let t = read_text(reader)?;
                        p.port = t.trim().parse::<u16>().ok();
                    }
                    "username" => p.username = Some(read_text(reader)?),
                    "password" => p.password = Some(read_text(reader)?),
                    "nonProxyHosts" => p.non_proxy_hosts = Some(read_text(reader)?),
                    _ => skip_element(reader, &name)?,
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(p);
            }
            Event::Eof => return Err("unexpected EOF in <proxy>".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn parse_string_list<R: BufRead>(
    reader: &mut Reader<R>,
    item: &str,
) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let name = local_name(e.name().as_ref());
                buf.clear();
                if name == item {
                    out.push(read_text(reader)?);
                } else {
                    skip_element(reader, &name)?;
                }
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => return Err(format!("unexpected EOF in <{item}> list")),
            _ => {}
        }
        buf.clear();
    }
}

// ===========================================================================
// Low-level helpers
// ===========================================================================

fn read_text<R: BufRead>(reader: &mut Reader<R>) -> Result<String, String> {
    let mut out = String::new();
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Text(t) => out.push_str(&decode_text(&t)?),
            Event::CData(c) => {
                let s = std::str::from_utf8(c.as_ref()).map_err(|e| e.to_string())?;
                out.push_str(s);
            }
            Event::Start(e) => {
                // Nested element inside a text-only field; skip for
                // robustness.
                let n = local_name(e.name().as_ref());
                buf.clear();
                skip_element(reader, &n)?;
            }
            Event::End(_) => {
                buf.clear();
                return Ok(out);
            }
            Event::Eof => return Err("unexpected EOF reading text".to_string()),
            _ => {}
        }
        buf.clear();
    }
}

fn read_bool<R: BufRead>(reader: &mut Reader<R>, default: bool) -> Result<bool, String> {
    let t = read_text(reader)?;
    Ok(parse_bool_str(&t, default))
}

fn parse_bool_str(raw: &str, default: bool) -> bool {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => true,
        "false" | "0" | "no" => false,
        _ => default,
    }
}

fn skip_element<R: BufRead>(reader: &mut Reader<R>, _name: &str) -> Result<(), String> {
    let mut depth: usize = 1;
    let mut buf = Vec::new();
    while depth > 0 {
        let ev = reader.read_event_into(&mut buf).map_err(xml_err)?;
        match ev {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            Event::Eof => return Err("unexpected EOF while skipping element".to_string()),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

fn decode_text(t: &BytesText<'_>) -> Result<String, String> {
    let raw = t.decode().map_err(|e| e.to_string())?;
    let unescaped = unescape(&raw).map_err(|e| e.to_string())?;
    Ok(unescaped.into_owned())
}

fn local_name(name: &[u8]) -> String {
    let s = std::str::from_utf8(name).unwrap_or("");
    s.rsplit_once(':').map_or(s, |(_, l)| l).to_string()
}

fn xml_err(e: quick_xml::Error) -> String {
    format!("xml error: {e}")
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(xml: &str) -> SettingsXml {
        parse_settings_str(xml).expect("parses")
    }

    // 1. Empty <settings/> parses to defaults.
    #[test]
    fn t01_empty_settings_is_default() {
        let s = parse("<settings/>");
        assert_eq!(s, SettingsXml::default());
        assert!(s.interactive_mode);
        assert!(!s.offline);
    }

    // 2. Single server with plaintext password.
    #[test]
    fn t02_single_server_plaintext() {
        let xml = r#"<settings>
          <servers>
            <server>
              <id>central</id>
              <username>alice</username>
              <password>hunter2</password>
            </server>
          </servers>
        </settings>"#;
        let s = parse(xml);
        assert_eq!(s.servers.len(), 1);
        let srv = &s.servers[0];
        assert_eq!(srv.id, "central");
        assert_eq!(srv.username.as_deref(), Some("alice"));
        assert_eq!(srv.password.as_deref(), Some("hunter2"));
    }

    // 3. Server with encrypted password — parses; decrypt errors.
    #[test]
    fn t03_encrypted_password_parses_decrypt_errors() {
        let xml = r#"<settings>
          <servers>
            <server>
              <id>nexus</id>
              <username>bob</username>
              <password>{COQLCE6DU6GtcS5P=}</password>
            </server>
          </servers>
        </settings>"#;
        let s = parse(xml);
        let pw = s.servers[0].password.as_deref().unwrap();
        assert_eq!(pw, "{COQLCE6DU6GtcS5P=}");
        let err = decrypt_password(pw, None).unwrap_err();
        match err {
            SettingsError::Decryption { detail } => {
                assert!(detail.contains("not yet supported"));
                assert!(detail.contains("environment variables"));
            }
            other => panic!("expected Decryption, got {other:?}"),
        }
    }

    // 4. Mirror with <mirrorOf>central</mirrorOf>.
    #[test]
    fn t04_mirror_specific_repo() {
        let xml = r#"<settings>
          <mirrors>
            <mirror>
              <id>internal</id>
              <name>Internal Mirror</name>
              <url>https://repo.example.com/</url>
              <mirrorOf>central</mirrorOf>
            </mirror>
          </mirrors>
        </settings>"#;
        let s = parse(xml);
        assert_eq!(s.mirrors.len(), 1);
        assert_eq!(s.mirrors[0].id, "internal");
        assert_eq!(s.mirrors[0].mirror_of, "central");
        assert_eq!(s.mirrors[0].url, "https://repo.example.com/");
        assert!(!s.mirrors[0].blocked);
    }

    // 5. Mirror with <mirrorOf>*</mirrorOf>.
    #[test]
    fn t05_mirror_wildcard() {
        let xml = r#"<settings>
          <mirrors>
            <mirror><id>m</id><url>https://m/</url><mirrorOf>*</mirrorOf></mirror>
          </mirrors>
        </settings>"#;
        let s = parse(xml);
        assert_eq!(s.mirrors[0].mirror_of, "*");
    }

    // 6. Mirror with negation.
    #[test]
    fn t06_mirror_negation() {
        let xml = r#"<settings>
          <mirrors>
            <mirror><id>m</id><url>https://m/</url><mirrorOf>!internal,*</mirrorOf></mirror>
          </mirrors>
        </settings>"#;
        let s = parse(xml);
        assert_eq!(s.mirrors[0].mirror_of, "!internal,*");
    }

    // 7. Profile with <properties>.
    #[test]
    fn t07_profile_properties() {
        let xml = r#"<settings>
          <profiles>
            <profile>
              <id>p1</id>
              <properties>
                <foo>bar</foo>
                <baz>qux</baz>
              </properties>
            </profile>
          </profiles>
        </settings>"#;
        let s = parse(xml);
        assert_eq!(s.profiles.len(), 1);
        let p = &s.profiles[0];
        assert_eq!(p.id, "p1");
        assert_eq!(p.properties.get("foo").map(String::as_str), Some("bar"));
        assert_eq!(p.properties.get("baz").map(String::as_str), Some("qux"));
    }

    // 8. Profile with activeByDefault.
    #[test]
    fn t08_profile_active_by_default() {
        let xml = r#"<settings>
          <profiles>
            <profile>
              <id>auto</id>
              <activation><activeByDefault>true</activeByDefault></activation>
            </profile>
          </profiles>
        </settings>"#;
        let s = parse(xml);
        let act = s.profiles[0].activation.as_ref().expect("activation");
        assert!(act.active_by_default);
    }

    // 9. Profile with repositories.
    #[test]
    fn t09_profile_repositories() {
        let xml = r#"<settings>
          <profiles>
            <profile>
              <id>p</id>
              <repositories>
                <repository>
                  <id>r1</id>
                  <url>https://r1/</url>
                  <releases><enabled>true</enabled></releases>
                  <snapshots><enabled>false</enabled></snapshots>
                </repository>
              </repositories>
            </profile>
          </profiles>
        </settings>"#;
        let s = parse(xml);
        let p = &s.profiles[0];
        assert_eq!(p.repositories.len(), 1);
        let r = &p.repositories[0];
        assert_eq!(r.id, "r1");
        assert_eq!(r.url, "https://r1/");
        assert!(r.releases.enabled);
        assert!(!r.snapshots.enabled);
    }

    // 10. <activeProfiles>.
    #[test]
    fn t10_active_profiles() {
        let xml = r#"<settings>
          <activeProfiles>
            <activeProfile>p1</activeProfile>
            <activeProfile>p2</activeProfile>
          </activeProfiles>
        </settings>"#;
        let s = parse(xml);
        assert_eq!(s.active_profile_ids, vec!["p1", "p2"]);
    }

    // 11. Multiple servers/mirrors/profiles together.
    #[test]
    fn t11_multiple_everything() {
        let xml = r#"<settings>
          <servers>
            <server><id>s1</id></server>
            <server><id>s2</id></server>
          </servers>
          <mirrors>
            <mirror><id>m1</id><url>https://m1/</url><mirrorOf>*</mirrorOf></mirror>
            <mirror><id>m2</id><url>https://m2/</url><mirrorOf>central</mirrorOf></mirror>
          </mirrors>
          <profiles>
            <profile><id>p1</id></profile>
            <profile><id>p2</id></profile>
          </profiles>
        </settings>"#;
        let s = parse(xml);
        assert_eq!(s.servers.len(), 2);
        assert_eq!(s.mirrors.len(), 2);
        assert_eq!(s.profiles.len(), 2);
    }

    // 12. <offline>true</offline>.
    #[test]
    fn t12_offline_true() {
        let s = parse("<settings><offline>true</offline></settings>");
        assert!(s.offline);
    }

    // 13. <localRepository>/path</localRepository>.
    #[test]
    fn t13_local_repository() {
        let s = parse("<settings><localRepository>/opt/m2</localRepository></settings>");
        assert_eq!(s.local_repository.as_deref(), Some("/opt/m2"));
    }

    // 14. <proxies>.
    #[test]
    fn t14_proxies() {
        let xml = r#"<settings>
          <proxies>
            <proxy>
              <id>corp</id>
              <active>true</active>
              <protocol>http</protocol>
              <host>proxy.corp</host>
              <port>8080</port>
              <username>u</username>
              <password>p</password>
              <nonProxyHosts>localhost|*.local</nonProxyHosts>
            </proxy>
          </proxies>
        </settings>"#;
        let s = parse(xml);
        assert_eq!(s.proxies.len(), 1);
        let p = &s.proxies[0];
        assert_eq!(p.id, "corp");
        assert!(p.active);
        assert_eq!(p.protocol, "http");
        assert_eq!(p.host, "proxy.corp");
        assert_eq!(p.port, Some(8080));
        assert_eq!(p.username.as_deref(), Some("u"));
        assert_eq!(p.non_proxy_hosts.as_deref(), Some("localhost|*.local"));
    }

    // 15. Unknown top-level element is skipped without error.
    #[test]
    fn t15_unknown_top_level_skipped() {
        let xml = r#"<settings>
          <futureField>some new thing</futureField>
          <offline>true</offline>
        </settings>"#;
        let s = parse(xml);
        assert!(s.offline);
    }

    // 16. Malformed XML returns parse error.
    #[test]
    fn t16_malformed_xml() {
        let err = parse_settings_str("<settings><offline>true</offlin").expect_err("should error");
        assert!(err.contains("xml error") || err.contains("EOF"));
    }

    // 17. Missing file returns Io error.
    #[test]
    fn t17_missing_file_io_error() {
        let err = parse_settings_xml(Path::new("/nonexistent/__no__settings.xml")).unwrap_err();
        match err {
            SettingsError::Io { .. } => {}
            other => panic!("expected Io, got {other:?}"),
        }
    }

    // 18. decrypt_password("plaintext", None) → "plaintext".
    #[test]
    fn t18_decrypt_plaintext_passthrough() {
        assert_eq!(decrypt_password("hunter2", None).unwrap(), "hunter2");
        assert_eq!(decrypt_password("", None).unwrap(), "");
    }

    // 19. decrypt_password("{enc}", None) → Decryption error.
    #[test]
    fn t19_decrypt_encrypted_errors() {
        let err = decrypt_password("{abcd}", None).unwrap_err();
        match err {
            SettingsError::Decryption { detail } => {
                assert!(detail.contains("encrypted passwords"));
            }
            other => panic!("expected Decryption, got {other:?}"),
        }
    }

    // 20. Fixture round-trip (a moderately full settings.xml).
    #[test]
    fn t20_fixture_round_trip() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <settings xmlns="http://maven.apache.org/SETTINGS/1.2.0">
          <localRepository>/var/m2</localRepository>
          <interactiveMode>false</interactiveMode>
          <offline>false</offline>
          <pluginGroups>
            <pluginGroup>org.example.plugins</pluginGroup>
          </pluginGroups>
          <servers>
            <server>
              <id>my-repo</id>
              <username>deploy</username>
              <password>secret</password>
            </server>
          </servers>
          <mirrors>
            <mirror>
              <id>nexus</id>
              <url>https://nexus.example.com/repository/maven-public/</url>
              <mirrorOf>external:*</mirrorOf>
            </mirror>
          </mirrors>
          <profiles>
            <profile>
              <id>release</id>
              <properties>
                <gpg.keyname>ABC123</gpg.keyname>
              </properties>
              <repositories>
                <repository>
                  <id>staging</id>
                  <url>https://nexus.example.com/repository/maven-staging/</url>
                </repository>
              </repositories>
            </profile>
          </profiles>
          <activeProfiles>
            <activeProfile>release</activeProfile>
          </activeProfiles>
        </settings>"#;
        let s = parse(xml);
        assert_eq!(s.local_repository.as_deref(), Some("/var/m2"));
        assert!(!s.interactive_mode);
        assert!(!s.offline);
        assert_eq!(s.plugin_groups, vec!["org.example.plugins"]);
        assert_eq!(s.servers.len(), 1);
        assert_eq!(s.servers[0].id, "my-repo");
        assert_eq!(s.mirrors[0].mirror_of, "external:*");
        assert_eq!(s.profiles[0].id, "release");
        assert_eq!(
            s.profiles[0]
                .properties
                .get("gpg.keyname")
                .map(String::as_str),
            Some("ABC123")
        );
        assert_eq!(s.profiles[0].repositories.len(), 1);
        assert_eq!(s.active_profile_ids, vec!["release"]);
    }

    // 21. Empty profile <properties/> ok.
    #[test]
    fn t21_empty_properties() {
        let xml = r#"<settings>
          <profiles><profile><id>p</id><properties></properties></profile></profiles>
        </settings>"#;
        let s = parse(xml);
        assert!(s.profiles[0].properties.is_empty());
    }

    // 22. Repo with both releases and snapshots policies.
    #[test]
    fn t22_repo_policies_both() {
        let xml = r#"<settings>
          <profiles>
            <profile>
              <id>p</id>
              <repositories>
                <repository>
                  <id>r</id>
                  <url>https://r/</url>
                  <releases>
                    <enabled>true</enabled>
                    <updatePolicy>daily</updatePolicy>
                    <checksumPolicy>warn</checksumPolicy>
                  </releases>
                  <snapshots>
                    <enabled>false</enabled>
                    <updatePolicy>never</updatePolicy>
                  </snapshots>
                </repository>
              </repositories>
            </profile>
          </profiles>
        </settings>"#;
        let s = parse(xml);
        let r = &s.profiles[0].repositories[0];
        assert!(r.releases.enabled);
        assert_eq!(r.releases.update_policy.as_deref(), Some("daily"));
        assert_eq!(r.releases.checksum_policy.as_deref(), Some("warn"));
        assert!(!r.snapshots.enabled);
        assert_eq!(r.snapshots.update_policy.as_deref(), Some("never"));
    }

    // 23. Server with privateKey instead of password.
    #[test]
    fn t23_server_private_key() {
        let xml = r#"<settings>
          <servers>
            <server>
              <id>scp</id>
              <username>deploy</username>
              <privateKey>/home/u/.ssh/id_rsa</privateKey>
              <passphrase>pp</passphrase>
              <filePermissions>664</filePermissions>
              <directoryPermissions>775</directoryPermissions>
            </server>
          </servers>
        </settings>"#;
        let s = parse(xml);
        let srv = &s.servers[0];
        assert!(srv.password.is_none());
        assert_eq!(srv.private_key.as_deref(), Some("/home/u/.ssh/id_rsa"));
        assert_eq!(srv.passphrase.as_deref(), Some("pp"));
        assert_eq!(srv.file_permissions.as_deref(), Some("664"));
        assert_eq!(srv.directory_permissions.as_deref(), Some("775"));
    }

    // 24. Namespaced root element parses (xmlns ignored).
    #[test]
    fn t24_namespaced_root_ok() {
        let xml = r#"<settings xmlns="http://maven.apache.org/SETTINGS/1.2.0"
                              xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
          <offline>true</offline>
        </settings>"#;
        let s = parse(xml);
        assert!(s.offline);
    }

    // 25. Plugin repositories on a profile.
    #[test]
    fn t25_plugin_repositories() {
        let xml = r#"<settings>
          <profiles>
            <profile>
              <id>p</id>
              <pluginRepositories>
                <pluginRepository>
                  <id>plug</id>
                  <url>https://plug/</url>
                </pluginRepository>
              </pluginRepositories>
            </profile>
          </profiles>
        </settings>"#;
        let s = parse(xml);
        let p = &s.profiles[0];
        assert_eq!(p.plugin_repositories.len(), 1);
        assert_eq!(p.plugin_repositories[0].id, "plug");
    }

    // 26. Activation with property + file conditions.
    #[test]
    fn t26_activation_property_and_file() {
        let xml = r#"<settings>
          <profiles>
            <profile>
              <id>p</id>
              <activation>
                <jdk>17</jdk>
                <property><name>env</name><value>ci</value></property>
                <file><exists>pom.xml</exists><missing>.skip</missing></file>
                <os><name>linux</name><family>unix</family><arch>amd64</arch><version>5</version></os>
              </activation>
            </profile>
          </profiles>
        </settings>"#;
        let s = parse(xml);
        let act = s.profiles[0].activation.as_ref().unwrap();
        assert_eq!(act.jdk.as_deref(), Some("17"));
        assert_eq!(act.property_name.as_deref(), Some("env"));
        assert_eq!(act.property_value.as_deref(), Some("ci"));
        assert_eq!(act.file_exists.as_deref(), Some("pom.xml"));
        assert_eq!(act.file_missing.as_deref(), Some(".skip"));
        assert_eq!(act.os_name.as_deref(), Some("linux"));
        assert_eq!(act.os_family.as_deref(), Some("unix"));
        assert_eq!(act.os_arch.as_deref(), Some("amd64"));
        assert_eq!(act.os_version.as_deref(), Some("5"));
    }

    // 27. <interactiveMode>false</interactiveMode> overrides default.
    #[test]
    fn t27_interactive_mode_off() {
        let s = parse("<settings><interactiveMode>false</interactiveMode></settings>");
        assert!(!s.interactive_mode);
    }

    // 28. parse_settings_xml end-to-end via a tempfile.
    #[test]
    fn t28_parse_from_tempfile() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.xml");
        std::fs::write(&path, "<settings><offline>true</offline></settings>").unwrap();
        let s = parse_settings_xml(&path).unwrap();
        assert!(s.offline);
    }

    // 29. Encrypted-blob detector edge cases.
    #[test]
    fn t29_encrypted_blob_detector() {
        assert!(is_encrypted_blob("{abc}"));
        assert!(!is_encrypted_blob("abc"));
        assert!(!is_encrypted_blob("{abc"));
        assert!(!is_encrypted_blob("abc}"));
        // Plaintext passthrough preserves leading/trailing whitespace.
        assert_eq!(decrypt_password("  plain  ", None).unwrap(), "  plain  ");
    }

    // 30. Blocked mirror.
    #[test]
    fn t30_blocked_mirror() {
        let xml = r#"<settings>
          <mirrors>
            <mirror>
              <id>blk</id>
              <url>https://blk/</url>
              <mirrorOf>*</mirrorOf>
              <blocked>true</blocked>
            </mirror>
          </mirrors>
        </settings>"#;
        let s = parse(xml);
        assert!(s.mirrors[0].blocked);
    }
}
