//! `.mvn/` directory honoring.
//!
//! Maven projects may include a `.mvn/` directory at the project root
//! with a small set of files that affect the build:
//!
//! - `.mvn/maven.config` — CLI flags / system properties prepended to
//!   every `mvn` invocation in this project. One arg per line; lines
//!   starting with `#` are comments; blank lines are skipped. Common
//!   contents: `-Dmaven.repo.local=.mvn/.local-repository`,
//!   `-Dproject.version=1.2.3`, `-T4`.
//!
//! - `.mvn/jvm.config` — JVM args passed to the Maven (or barback)
//!   process. Same one-arg-per-line format. Common contents:
//!   `-Xmx2g`, `--add-opens=java.base/java.util=ALL-UNNAMED`.
//!
//! - `.mvn/extensions.xml` — declares build extensions loaded BEFORE
//!   POM parsing. Examples: `maven-build-cache-extension`,
//!   `os-maven-plugin`, CI-friendly-versions extension. Extension
//!   support is OUT OF SCOPE for v0.1; see
//!   `docs/compat/dot-mvn-extensions-survey.md` for the
//!   corpus-impact accounting that informs v0.2 scoping.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Parsed contents of a project's `.mvn/` directory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DotMvnConfig {
    /// One entry per token from non-empty, non-comment lines in
    /// `.mvn/maven.config`. Lines are token-split on shell whitespace.
    pub maven_args: Vec<String>,

    /// One entry per token from non-empty, non-comment lines in
    /// `.mvn/jvm.config`.
    pub jvm_args: Vec<String>,

    /// True if `.mvn/extensions.xml` exists. v0.1 logs a warning and
    /// otherwise ignores; v0.2 (or later) will parse and apply.
    pub has_extensions_xml: bool,

    /// Resolved path to the `.mvn/` directory. Set even when the
    /// directory does not exist (in which case it points at where
    /// the loader looked).
    pub dot_mvn_dir: PathBuf,
}

/// Errors raised by [`load_dot_mvn`].
#[derive(Debug, thiserror::Error)]
pub enum DotMvnError {
    /// Filesystem I/O failure reading one of the `.mvn/` files. The
    /// `.mvn/` directory's *absence* is not an error — it's the
    /// common case.
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Load `.mvn/` contents from a project root.
///
/// `project_root` is the directory containing `pom.xml`. If the
/// `.mvn/` subdirectory doesn't exist, returns a default (empty)
/// [`DotMvnConfig`] without error.
///
/// Caller decides what to do with the result — typically the CLI
/// prepends [`DotMvnConfig::maven_args`] to argv (gated on
/// `config.maven.honor_mvn_config`), and forwards
/// [`DotMvnConfig::jvm_args`] to barback's JVM (gated on
/// `config.maven.honor_jvm_config`).
pub fn load_dot_mvn(project_root: &Path) -> Result<DotMvnConfig, DotMvnError> {
    let dot_mvn_dir = project_root.join(".mvn");
    let mut out = DotMvnConfig {
        dot_mvn_dir: dot_mvn_dir.clone(),
        ..Default::default()
    };

    if !dot_mvn_dir.is_dir() {
        return Ok(out);
    }

    let maven_config = dot_mvn_dir.join("maven.config");
    if maven_config.is_file() {
        out.maven_args = read_arg_file(&maven_config)?;
    }

    let jvm_config = dot_mvn_dir.join("jvm.config");
    if jvm_config.is_file() {
        out.jvm_args = read_arg_file(&jvm_config)?;
    }

    out.has_extensions_xml = dot_mvn_dir.join("extensions.xml").is_file();

    Ok(out)
}

/// Read a Maven-style arg file: one logical "argument source" per
/// line, blank lines and `#` comments skipped, each surviving line
/// token-split on whitespace.
///
/// Maven's own behavior is "the file's content is split on whitespace
/// like a shell would" — a single shell-quote-aware split. For v0.1
/// `split_whitespace` is sufficient; quoted-arg-with-spaces is a rare
/// pattern in `.mvn/maven.config` and `.mvn/jvm.config` in practice
/// (system property values that contain spaces are usually written as
/// `-Dkey=value-no-spaces` or escaped at the shell level).
fn read_arg_file(path: &Path) -> Result<Vec<String>, DotMvnError> {
    let raw = std::fs::read_to_string(path).map_err(|source| DotMvnError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut args = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        for tok in trimmed.split_whitespace() {
            args.push(tok.to_string());
        }
    }
    Ok(args)
}

/// Render the user-facing warning string emitted when a project has
/// `.mvn/extensions.xml`.
///
/// Callers decide how to surface the message (stderr, structured log,
/// diagnostic stream). Returning the string keeps the module
/// dependency-free.
pub fn warn_extensions_unsupported(project_root: &Path) -> String {
    format!(
        "warning: {}/.mvn/extensions.xml is present. Barista does not yet \
         apply Maven build extensions; the build may differ from `mvn`. \
         (Extension support is planned for a subsequent release; see \
         docs/compat/dot-mvn-extensions-survey.md for current status.)",
        project_root.display()
    )
}

/// Summary statistics produced by [`survey_extensions`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExtensionSurvey {
    /// Number of corpus entries surveyed.
    pub total_projects: usize,
    /// Sorted list of corpus IDs that contain `.mvn/extensions.xml`.
    pub projects_with_extensions: Vec<String>,
    /// Per-extension counts, keyed by `groupId:artifactId` (or
    /// `artifactId` alone if the groupId could not be parsed).
    pub extension_counts: BTreeMap<String, usize>,
}

/// Survey a list of project paths for `.mvn/extensions.xml` usage.
///
/// Input is `(corpus_id, project_root_path)` pairs. Output is a
/// deterministic [`ExtensionSurvey`]: identical inputs always produce
/// identical structs (the `projects_with_extensions` list is sorted;
/// the counts map iterates in key order).
///
/// Parse errors on a malformed `extensions.xml` are best-effort —
/// what we can extract is counted, the rest is silently dropped. The
/// survey is informational; it must not crash a corpus scan.
pub fn survey_extensions(corpus_paths: &[(String, PathBuf)]) -> ExtensionSurvey {
    let mut survey = ExtensionSurvey {
        total_projects: corpus_paths.len(),
        ..Default::default()
    };

    for (id, root) in corpus_paths {
        let path = root.join(".mvn").join("extensions.xml");
        if !path.is_file() {
            continue;
        }
        survey.projects_with_extensions.push(id.clone());
        if let Ok(raw) = std::fs::read_to_string(&path) {
            for key in parse_extension_keys_best_effort(&raw) {
                *survey.extension_counts.entry(key).or_insert(0) += 1;
            }
        }
    }

    survey.projects_with_extensions.sort();
    survey
}

/// Best-effort parser for `<extension><groupId>…</groupId><artifactId>
/// …</artifactId></extension>` blocks. Returns one
/// `groupId:artifactId` key per extension element found. Malformed
/// input yields a shorter list rather than an error.
fn parse_extension_keys_best_effort(xml: &str) -> Vec<String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut keys = Vec::new();
    let mut depth_in_extension = 0u32;
    let mut current_text_target: Option<&'static str> = None;
    let mut current_group: Option<String> = None;
    let mut current_artifact: Option<String> = None;

    loop {
        match reader.read_event() {
            Err(_) => break,
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = e.name();
                let name = name.as_ref();
                match name {
                    b"extension" => {
                        depth_in_extension = 1;
                        current_group = None;
                        current_artifact = None;
                    }
                    b"groupId" if depth_in_extension > 0 => {
                        current_text_target = Some("groupId");
                    }
                    b"artifactId" if depth_in_extension > 0 => {
                        current_text_target = Some("artifactId");
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(target) = current_text_target {
                    if let Ok(s) = std::str::from_utf8(t.as_ref()) {
                        let v = s.trim().to_string();
                        if !v.is_empty() {
                            match target {
                                "groupId" => current_group = Some(v),
                                "artifactId" => current_artifact = Some(v),
                                _ => {}
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                match name.as_ref() {
                    b"groupId" | b"artifactId" => current_text_target = None,
                    b"extension" => {
                        depth_in_extension = 0;
                        let key = match (&current_group, &current_artifact) {
                            (Some(g), Some(a)) => Some(format!("{g}:{a}")),
                            (None, Some(a)) => Some(a.clone()),
                            _ => None,
                        };
                        if let Some(k) = key {
                            keys.push(k);
                        }
                        current_group = None;
                        current_artifact = None;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn no_dot_mvn_returns_default() {
        let dir = tempdir().unwrap();
        let cfg = load_dot_mvn(dir.path()).unwrap();
        assert!(cfg.maven_args.is_empty());
        assert!(cfg.jvm_args.is_empty());
        assert!(!cfg.has_extensions_xml);
        assert_eq!(cfg.dot_mvn_dir, dir.path().join(".mvn"));
    }

    #[test]
    fn maven_config_three_lines_parses_to_three_args() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(".mvn/maven.config"),
            "-Dfoo=bar\n-Dbaz=qux\n-T4\n",
        );
        let cfg = load_dot_mvn(dir.path()).unwrap();
        assert_eq!(cfg.maven_args, vec!["-Dfoo=bar", "-Dbaz=qux", "-T4"]);
    }

    #[test]
    fn comments_and_blank_lines_stripped() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(".mvn/maven.config"),
            "# leading comment\n\n-Dfoo=bar\n   \n# trailing\n-T4\n",
        );
        let cfg = load_dot_mvn(dir.path()).unwrap();
        assert_eq!(cfg.maven_args, vec!["-Dfoo=bar", "-T4"]);
    }

    #[test]
    fn multi_token_line_splits_to_multiple_args() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(".mvn/maven.config"), "-T 4\n");
        let cfg = load_dot_mvn(dir.path()).unwrap();
        assert_eq!(cfg.maven_args, vec!["-T", "4"]);
    }

    #[test]
    fn jvm_config_parses() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(".mvn/jvm.config"),
            "-Xmx2g\n--add-opens=java.base/java.util=ALL-UNNAMED\n",
        );
        let cfg = load_dot_mvn(dir.path()).unwrap();
        assert_eq!(
            cfg.jvm_args,
            vec!["-Xmx2g", "--add-opens=java.base/java.util=ALL-UNNAMED"]
        );
    }

    #[test]
    fn both_config_files_present() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(".mvn/maven.config"), "-Dfoo=bar\n");
        write(&dir.path().join(".mvn/jvm.config"), "-Xmx1g\n");
        let cfg = load_dot_mvn(dir.path()).unwrap();
        assert_eq!(cfg.maven_args, vec!["-Dfoo=bar"]);
        assert_eq!(cfg.jvm_args, vec!["-Xmx1g"]);
        assert!(!cfg.has_extensions_xml);
    }

    #[test]
    fn extensions_xml_sets_flag() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(".mvn/extensions.xml"), "<extensions/>\n");
        let cfg = load_dot_mvn(dir.path()).unwrap();
        assert!(cfg.has_extensions_xml);
    }

    #[test]
    fn warn_message_mentions_project_path() {
        let msg = warn_extensions_unsupported(Path::new("/tmp/myproj"));
        assert!(!msg.is_empty());
        assert!(msg.contains("/tmp/myproj"));
        assert!(msg.contains("extensions.xml"));
    }

    #[test]
    fn survey_on_empty_corpus() {
        let survey = survey_extensions(&[]);
        assert_eq!(survey.total_projects, 0);
        assert!(survey.projects_with_extensions.is_empty());
        assert!(survey.extension_counts.is_empty());
    }

    #[test]
    fn survey_counts_extensions() {
        let dir = tempdir().unwrap();
        let proj_a = dir.path().join("a");
        let proj_b = dir.path().join("b");
        fs::create_dir_all(&proj_a).unwrap();
        fs::create_dir_all(&proj_b).unwrap();
        write(
            &proj_a.join(".mvn/extensions.xml"),
            r#"<?xml version="1.0"?>
<extensions>
  <extension>
    <groupId>kr.motd.maven</groupId>
    <artifactId>os-maven-plugin</artifactId>
    <version>1.7.1</version>
  </extension>
  <extension>
    <groupId>com.gradle</groupId>
    <artifactId>maven-build-cache-extension</artifactId>
    <version>1.0</version>
  </extension>
</extensions>
"#,
        );
        // proj_b has no .mvn/
        let survey = survey_extensions(&[
            ("a".to_string(), proj_a.clone()),
            ("b".to_string(), proj_b.clone()),
        ]);
        assert_eq!(survey.total_projects, 2);
        assert_eq!(survey.projects_with_extensions, vec!["a".to_string()]);
        assert_eq!(
            survey.extension_counts.get("kr.motd.maven:os-maven-plugin"),
            Some(&1)
        );
        assert_eq!(
            survey
                .extension_counts
                .get("com.gradle:maven-build-cache-extension"),
            Some(&1)
        );
    }

    #[test]
    fn survey_tolerates_malformed_xml() {
        let dir = tempdir().unwrap();
        let proj = dir.path().join("p");
        fs::create_dir_all(&proj).unwrap();
        write(
            &proj.join(".mvn/extensions.xml"),
            "<extensions><extension><groupId>g</groupId><artifactId>a</a", // truncated
        );
        let survey = survey_extensions(&[("p".to_string(), proj)]);
        // Should not panic; the project IS counted as having
        // extensions.xml even if parsing yields no clean entries.
        assert_eq!(survey.total_projects, 1);
        assert_eq!(survey.projects_with_extensions, vec!["p".to_string()]);
        // extension_counts may be empty — best-effort.
    }

    #[test]
    fn survey_is_deterministic() {
        let dir = tempdir().unwrap();
        let proj_a = dir.path().join("a");
        let proj_b = dir.path().join("b");
        fs::create_dir_all(&proj_a).unwrap();
        fs::create_dir_all(&proj_b).unwrap();
        let xml = r#"<extensions>
  <extension><groupId>g</groupId><artifactId>x</artifactId></extension>
</extensions>"#;
        write(&proj_a.join(".mvn/extensions.xml"), xml);
        write(&proj_b.join(".mvn/extensions.xml"), xml);

        // Reverse the input order; the output must still be sorted.
        let s1 = survey_extensions(&[
            ("a".to_string(), proj_a.clone()),
            ("b".to_string(), proj_b.clone()),
        ]);
        let s2 = survey_extensions(&[("b".to_string(), proj_b), ("a".to_string(), proj_a)]);
        assert_eq!(s1, s2);
        assert_eq!(s1.projects_with_extensions, vec!["a", "b"]);
        assert_eq!(s1.extension_counts.get("g:x"), Some(&2));
    }
}
