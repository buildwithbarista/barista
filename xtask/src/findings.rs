//! `cargo xtask findings` — efficiency-findings catalog tooling.
//!
//! Two subcommands today:
//!
//! - `findings list` prints a table of every catalog entry under
//!   `docs/efficiency/findings/EFF-2026-*.md` so a reviewer can see
//!   the current state at a glance.
//! - `findings promote <path>` is the **promotion ceremony**: it
//!   moves a draft out of `docs/efficiency/findings/auto-generated/`
//!   into the parent catalog directory, allocating the next free
//!   `EFF-2026-NNN` and rewriting the draft's `id:` frontmatter line.
//!   Refuses to overwrite an existing id.
//!
//! The catalog format is documented in
//! `docs/efficiency/findings/README.md` and in PRD §18.10. The
//! frontmatter validator here enforces the *required* subset of that
//! shape — fields that downstream tooling depends on. Optional
//! fields (the `impact` mapping is treated as optional for catalog
//! validation; the seeds and the analyzer-emitted drafts both
//! include it, but the schema is intentionally permissive so
//! hand-authored entries can extend it).
//!
//! ## Why no YAML crate
//!
//! The frontmatter is a tiny fixed shape (six required scalar
//! fields). A targeted parser that knows the schema is ~120 LOC and
//! avoids dragging a YAML dependency into the workspace. If the
//! schema ever grows complex (nested mappings, anchors, multiline
//! scalars) this is the place to introduce one.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Args as ClapArgs, Subcommand};

// ---------------------------------------------------------------------------
// CLI surface
// ---------------------------------------------------------------------------

/// Top-level args for `cargo xtask findings`.
#[derive(ClapArgs, Debug, Clone)]
pub struct FindingsArgs {
    #[command(subcommand)]
    pub command: FindingsCommand,
}

/// Re-export under the conventional `Args` name so `main.rs`'s
/// `Command::Findings(findings::Args)` reads cleanly.
pub type Args = FindingsArgs;

/// Subcommands under `findings`.
#[derive(Subcommand, Debug, Clone)]
pub enum FindingsCommand {
    /// Print every catalog entry under
    /// `docs/efficiency/findings/EFF-2026-*.md` as a table.
    List(ListArgs),

    /// Promote a draft out of
    /// `docs/efficiency/findings/auto-generated/` into the catalog,
    /// allocating the next free `EFF-2026-NNN` id.
    Promote(PromoteArgs),
}

/// Flags for `findings list`.
#[derive(ClapArgs, Debug, Clone)]
pub struct ListArgs {
    /// Catalog root. Defaults to `docs/efficiency/findings/` relative
    /// to the current working directory. Tests override this.
    #[arg(long, value_name = "DIR")]
    pub catalog_dir: Option<PathBuf>,
}

/// Flags for `findings promote`.
#[derive(ClapArgs, Debug, Clone)]
pub struct PromoteArgs {
    /// Path to the draft to promote. Must be inside the
    /// `auto-generated/` directory under the catalog root.
    pub draft: PathBuf,

    /// Catalog root (parent of `auto-generated/`). Defaults to
    /// `docs/efficiency/findings/`. Tests override this so the
    /// promotion ceremony can be exercised against a tempdir.
    #[arg(long, value_name = "DIR")]
    pub catalog_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Binary entry point. Returns a process exit code.
#[must_use]
pub fn run(args: Args) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot read current directory: {e}");
            return 1;
        }
    };
    match args.command {
        FindingsCommand::List(list) => {
            let catalog = list
                .catalog_dir
                .unwrap_or_else(|| cwd.join("docs/efficiency/findings"));
            match list_catalog(&catalog) {
                Ok(table) => {
                    print!("{table}");
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        FindingsCommand::Promote(promote) => {
            let catalog = promote
                .catalog_dir
                .unwrap_or_else(|| cwd.join("docs/efficiency/findings"));
            match promote_draft(&promote.draft, &catalog) {
                Ok(promoted) => {
                    println!(
                        "promoted {src} -> {dst} (id: {id})",
                        src = promote.draft.display(),
                        dst = promoted.dest.display(),
                        id = promoted.allocated_id,
                    );
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Catalog listing
// ---------------------------------------------------------------------------

/// One row in the catalog table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRow {
    pub id: String,
    pub title: String,
    pub severity: String,
    pub category: String,
    pub status: String,
    pub discovered_by: String,
}

/// Scan `catalog_dir` for `EFF-2026-NNN.md` files, parse each, and
/// return them sorted by id.
pub fn collect_rows(catalog_dir: &Path) -> Result<Vec<CatalogRow>, FindingsError> {
    if !catalog_dir.exists() {
        return Err(FindingsError::CatalogMissing(catalog_dir.to_path_buf()));
    }
    let mut rows = Vec::new();
    for entry in fs::read_dir(catalog_dir).map_err(|source| FindingsError::Io {
        path: catalog_dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| FindingsError::Io {
            path: catalog_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !is_catalog_file(&path) {
            continue;
        }
        let body = fs::read_to_string(&path).map_err(|source| FindingsError::Io {
            path: path.clone(),
            source,
        })?;
        let fm = parse_frontmatter(&body).map_err(|reason| FindingsError::InvalidDraft {
            path: path.clone(),
            reason,
        })?;
        rows.push(CatalogRow {
            id: fm.id,
            title: fm.title,
            severity: fm.severity,
            category: fm.category,
            status: fm.status,
            discovered_by: fm.discovered_by,
        });
    }
    rows.sort_by(|a, b| compare_ids(&a.id, &b.id));
    Ok(rows)
}

/// Build the table string for `findings list`. Public so tests can
/// assert on the exact rendering.
pub fn list_catalog(catalog_dir: &Path) -> Result<String, FindingsError> {
    let rows = collect_rows(catalog_dir)?;
    Ok(render_table(&rows))
}

fn render_table(rows: &[CatalogRow]) -> String {
    let headers = [
        "ID",
        "Title",
        "Severity",
        "Category",
        "Status",
        "Discovered-by",
    ];
    // Column widths: max of the header and any cell. Cap title at 60
    // so terminal output doesn't wrap horribly; the cell text is not
    // truncated, just the column-width calculation is bounded.
    let mut widths = headers.map(str::len);
    for r in rows {
        let cells = [
            r.id.as_str(),
            r.title.as_str(),
            r.severity.as_str(),
            r.category.as_str(),
            r.status.as_str(),
            r.discovered_by.as_str(),
        ];
        for (i, cell) in cells.iter().enumerate() {
            let w = cell.chars().count();
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }

    let mut out = String::new();
    write_row(&mut out, &headers, &widths);
    let sep_cells: [String; 6] = std::array::from_fn(|i| "-".repeat(widths[i]));
    let sep_refs: [&str; 6] = std::array::from_fn(|i| sep_cells[i].as_str());
    write_row(&mut out, &sep_refs, &widths);
    for r in rows {
        let cells = [
            r.id.as_str(),
            r.title.as_str(),
            r.severity.as_str(),
            r.category.as_str(),
            r.status.as_str(),
            r.discovered_by.as_str(),
        ];
        write_row(&mut out, &cells, &widths);
    }
    if rows.is_empty() {
        out.push_str("(no findings)\n");
    }
    out
}

fn write_row(out: &mut String, cells: &[&str; 6], widths: &[usize; 6]) {
    for (i, cell) in cells.iter().enumerate() {
        let pad = widths[i].saturating_sub(cell.chars().count());
        if i > 0 {
            out.push_str(" | ");
        }
        out.push_str(cell);
        for _ in 0..pad {
            out.push(' ');
        }
    }
    out.push('\n');
}

// ---------------------------------------------------------------------------
// Draft promotion
// ---------------------------------------------------------------------------

/// Result of a successful promotion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promoted {
    /// Newly allocated `EFF-2026-NNN` id.
    pub allocated_id: String,
    /// Final on-disk path in the catalog directory.
    pub dest: PathBuf,
}

/// Promote `draft_path` into the catalog at `catalog_dir`. Allocates
/// the next free `EFF-2026-NNN`, rewrites the `id:` frontmatter
/// line, writes the file to `catalog_dir/<id>.md`, and deletes the
/// draft.
pub fn promote_draft(draft_path: &Path, catalog_dir: &Path) -> Result<Promoted, FindingsError> {
    // 1. Draft must live under <catalog_dir>/auto-generated/. We use
    //    a string-prefix check against the canonicalized form so
    //    callers can pass either relative or absolute paths.
    let canonical_draft = draft_path
        .canonicalize()
        .map_err(|source| FindingsError::Io {
            path: draft_path.to_path_buf(),
            source,
        })?;
    let canonical_catalog = catalog_dir
        .canonicalize()
        .map_err(|source| FindingsError::Io {
            path: catalog_dir.to_path_buf(),
            source,
        })?;
    let auto_dir = canonical_catalog.join("auto-generated");
    if !canonical_draft.starts_with(&auto_dir) {
        return Err(FindingsError::DraftNotUnderAutoGenerated {
            draft: draft_path.to_path_buf(),
            expected_under: auto_dir,
        });
    }

    // 2. Read + parse the draft. Validates frontmatter and required
    //    body sections.
    let body = fs::read_to_string(&canonical_draft).map_err(|source| FindingsError::Io {
        path: canonical_draft.clone(),
        source,
    })?;
    let parsed = parse_frontmatter(&body).map_err(|reason| FindingsError::InvalidDraft {
        path: canonical_draft.clone(),
        reason,
    })?;
    validate_body_sections(&body).map_err(|reason| FindingsError::InvalidDraft {
        path: canonical_draft.clone(),
        reason,
    })?;

    // 3. Allocate the next id by scanning the catalog directory.
    let allocated_id = allocate_next_id(&canonical_catalog)?;
    let dest = canonical_catalog.join(format!("{allocated_id}.md"));
    if dest.exists() {
        return Err(FindingsError::IdCollision { path: dest.clone() });
    }

    // 4. Rewrite the `id:` line in the frontmatter. The placeholder
    //    EFF-2026-PENDING is the expected starting value but we
    //    accept any `id:` line and replace it.
    let rewritten = rewrite_id_line(&body, &allocated_id, &parsed.raw_id_line)?;

    // 5. Write the destination file, then delete the draft. The
    //    write-then-delete order means a crash mid-promotion leaves a
    //    copy at both paths rather than losing the draft entirely.
    fs::write(&dest, rewritten).map_err(|source| FindingsError::Io {
        path: dest.clone(),
        source,
    })?;
    fs::remove_file(&canonical_draft).map_err(|source| FindingsError::Io {
        path: canonical_draft,
        source,
    })?;

    Ok(Promoted { allocated_id, dest })
}

/// Find the highest `EFF-2026-NNN` in `catalog_dir` and return
/// `EFF-2026-(NNN+1)`, zero-padded to three digits. Starts at
/// `EFF-2026-001` when the directory has no entries.
fn allocate_next_id(catalog_dir: &Path) -> Result<String, FindingsError> {
    let mut highest: u32 = 0;
    for entry in fs::read_dir(catalog_dir).map_err(|source| FindingsError::Io {
        path: catalog_dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| FindingsError::Io {
            path: catalog_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !is_catalog_file(&path) {
            continue;
        }
        if let Some(n) = parse_id_number(&path) {
            if n > highest {
                highest = n;
            }
        }
    }
    let next = highest.saturating_add(1);
    Ok(format!("EFF-2026-{next:03}"))
}

/// Match `EFF-2026-<NNN>.md` filenames and return the numeric part.
fn parse_id_number(path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    let suffix = stem.strip_prefix("EFF-2026-")?;
    suffix.parse::<u32>().ok()
}

/// True for files whose basename matches `EFF-2026-NNN.md`.
fn is_catalog_file(path: &Path) -> bool {
    if path.extension() != Some(OsStr::new("md")) {
        return false;
    }
    parse_id_number(path).is_some()
}

/// Stable id ordering: numeric on the trailing NNN so
/// `EFF-2026-010` sorts after `EFF-2026-009`, not before it.
fn compare_ids(a: &str, b: &str) -> Ordering {
    let parse =
        |s: &str| -> Option<u32> { s.strip_prefix("EFF-2026-").and_then(|n| n.parse().ok()) };
    match (parse(a), parse(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

fn rewrite_id_line(body: &str, new_id: &str, raw_id_line: &str) -> Result<String, FindingsError> {
    // The frontmatter parser captured the exact original `id:` line
    // (incl. trailing whitespace as it appeared in the file).
    // String-replace it in the source. If the line appears more than
    // once we refuse — that's a malformed frontmatter we shouldn't
    // silently corrupt.
    let count = body.matches(raw_id_line).count();
    if count != 1 {
        return Err(FindingsError::InvalidDraft {
            path: PathBuf::new(),
            reason: format!("expected exactly one occurrence of the id line, found {count}"),
        });
    }
    let new_line = format!("id: {new_id}");
    Ok(body.replacen(raw_id_line, &new_line, 1))
}

// ---------------------------------------------------------------------------
// Frontmatter parser
// ---------------------------------------------------------------------------

/// Required-field subset of a finding's YAML frontmatter, plus the
/// raw `id:` line so the promotion ceremony can rewrite it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub id: String,
    pub title: String,
    pub severity: String,
    pub category: String,
    pub status: String,
    pub discovered_by: String,
    /// Raw `id: <value>` line as it appeared in the source, for the
    /// rewrite step.
    pub raw_id_line: String,
}

/// Parse the YAML frontmatter at the top of `body`. Permissive about
/// extra fields and indentation in the `impact:` mapping (that
/// mapping isn't required for catalog operations). Strict about the
/// six required scalar fields.
pub fn parse_frontmatter(body: &str) -> Result<Frontmatter, String> {
    let body = body.strip_prefix('\u{feff}').unwrap_or(body);
    // Find the opening `---` line.
    let after_open = body
        .strip_prefix("---\n")
        .ok_or_else(|| "missing opening `---` line".to_string())?;
    // Find the closing `---` line. Accept `---\n` or `---` at end of
    // file. We split on the first occurrence after the opener.
    let close_idx = after_open
        .find("\n---")
        .ok_or_else(|| "missing closing `---` line".to_string())?;
    let yaml = &after_open[..close_idx];

    let mut id = None::<String>;
    let mut raw_id_line = None::<String>;
    let mut title = None::<String>;
    let mut severity = None::<String>;
    let mut category = None::<String>;
    let mut status = None::<String>;
    let mut discovered_by = None::<String>;

    for line in yaml.lines() {
        // Skip indented lines (they belong to a nested mapping like
        // `impact:`), blank lines, and YAML comments.
        let trimmed_full = line.trim_end();
        if trimmed_full.is_empty() || trimmed_full.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some((key, value)) = trimmed_full.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        // For mapping-valued keys (like `impact:` with no inline
        // value) the value is empty. Skip — we don't care about it
        // for catalog operations.
        if value.is_empty() {
            continue;
        }
        let unquoted = strip_yaml_quotes(value);
        match key {
            "id" => {
                id = Some(unquoted.to_string());
                raw_id_line = Some(trimmed_full.to_string());
            }
            "title" => title = Some(unquoted.to_string()),
            "severity" => severity = Some(unquoted.to_string()),
            "category" => category = Some(unquoted.to_string()),
            "status" => status = Some(unquoted.to_string()),
            "discovered_by" => discovered_by = Some(unquoted.to_string()),
            _ => {}
        }
    }

    let id = id.ok_or_else(|| "missing required field `id`".to_string())?;
    let raw_id_line = raw_id_line.ok_or_else(|| "missing required field `id`".to_string())?;
    let title = title.ok_or_else(|| "missing required field `title`".to_string())?;
    let severity = severity.ok_or_else(|| "missing required field `severity`".to_string())?;
    let category = category.ok_or_else(|| "missing required field `category`".to_string())?;
    let status = status.ok_or_else(|| "missing required field `status`".to_string())?;
    let discovered_by =
        discovered_by.ok_or_else(|| "missing required field `discovered_by`".to_string())?;

    Ok(Frontmatter {
        id,
        title,
        severity,
        category,
        status,
        discovered_by,
        raw_id_line,
    })
}

fn strip_yaml_quotes(s: &str) -> &str {
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Enforce that the draft body carries the four canonical sections.
/// Headings must appear at the start of a line as level-2 markdown.
pub fn validate_body_sections(body: &str) -> Result<(), String> {
    let required = [
        "## Evidence",
        "## Impact estimate",
        "## Proposed mitigation",
        "## References",
    ];
    let mut missing = Vec::new();
    for heading in required {
        let needle_with_newline = format!("\n{heading}");
        let starts_with = body.starts_with(heading);
        if !starts_with && !body.contains(&needle_with_newline) {
            missing.push(heading);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        let mut msg = String::from("missing required body section(s):");
        for h in missing {
            let _ = write!(&mut msg, " `{h}`");
        }
        Err(msg)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error type for `findings` subcommand operations. Display impls are
/// terse and reviewer-readable; the binary just prints them via
/// `eprintln!`.
#[derive(Debug)]
pub enum FindingsError {
    /// Catalog directory doesn't exist.
    CatalogMissing(PathBuf),
    /// I/O error against the named path.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Draft was outside `auto-generated/`.
    DraftNotUnderAutoGenerated {
        draft: PathBuf,
        expected_under: PathBuf,
    },
    /// Draft frontmatter or body sections failed validation.
    InvalidDraft { path: PathBuf, reason: String },
    /// Allocated id would overwrite an existing catalog file. Means
    /// the catalog has gaps and a manual sequence has overlapped with
    /// the allocator; the reviewer must intervene.
    IdCollision { path: PathBuf },
}

impl std::fmt::Display for FindingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CatalogMissing(p) => {
                write!(f, "catalog directory not found: {}", p.display())
            }
            Self::Io { path, source } => {
                write!(f, "i/o error on {}: {source}", path.display())
            }
            Self::DraftNotUnderAutoGenerated {
                draft,
                expected_under,
            } => write!(
                f,
                "draft {} is not under expected auto-generated dir {}",
                draft.display(),
                expected_under.display()
            ),
            Self::InvalidDraft { path, reason } => {
                if path.as_os_str().is_empty() {
                    write!(f, "invalid draft: {reason}")
                } else {
                    write!(f, "invalid draft at {}: {reason}", path.display())
                }
            }
            Self::IdCollision { path } => write!(
                f,
                "id collision: {} already exists; manual catalog repair required",
                path.display()
            ),
        }
    }
}

impl std::error::Error for FindingsError {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const VALID_DRAFT: &str = "---\n\
id: EFF-2026-PENDING\n\
title: Sample finding\n\
severity: medium\n\
category: redundant_metadata_fetch\n\
status: open\n\
discovered_by: MetadataOverFetchAnalyzer\n\
impact:\n\
  bytes_saved_per_build: 1000\n\
  requests_saved_per_build: 2\n\
  connections_saved_per_build: 0\n\
---\n\
\n\
## Evidence\n\
\n\
- foo\n\
\n\
## Impact estimate\n\
\n\
- bar\n\
\n\
## Proposed mitigation\n\
\n\
baz\n\
\n\
## References\n\
\n\
- PRD §18.9\n";

    #[test]
    fn parses_required_fields() {
        let fm = parse_frontmatter(VALID_DRAFT).expect("valid frontmatter");
        assert_eq!(fm.id, "EFF-2026-PENDING");
        assert_eq!(fm.title, "Sample finding");
        assert_eq!(fm.severity, "medium");
        assert_eq!(fm.category, "redundant_metadata_fetch");
        assert_eq!(fm.status, "open");
        assert_eq!(fm.discovered_by, "MetadataOverFetchAnalyzer");
        assert_eq!(fm.raw_id_line, "id: EFF-2026-PENDING");
    }

    #[test]
    fn parses_quoted_title_with_colon() {
        let draft = VALID_DRAFT.replace(
            "title: Sample finding",
            "title: \"Sample: finding with colon\"",
        );
        let fm = parse_frontmatter(&draft).expect("valid frontmatter");
        assert_eq!(fm.title, "Sample: finding with colon");
    }

    #[test]
    fn missing_required_field_errors() {
        let draft = VALID_DRAFT.replace("severity: medium\n", "");
        let err = parse_frontmatter(&draft).expect_err("should fail");
        assert!(err.contains("severity"), "{err}");
    }

    #[test]
    fn body_section_validator_accepts_full_draft() {
        validate_body_sections(VALID_DRAFT).expect("all four sections present");
    }

    #[test]
    fn body_section_validator_flags_missing_section() {
        let draft = VALID_DRAFT.replace("## References", "## Misc");
        let err = validate_body_sections(&draft).expect_err("missing References");
        assert!(err.contains("References"), "{err}");
    }

    #[test]
    fn id_compare_is_numeric() {
        let mut ids = vec!["EFF-2026-010", "EFF-2026-002", "EFF-2026-001"];
        ids.sort_by(|a, b| compare_ids(a, b));
        assert_eq!(ids, vec!["EFF-2026-001", "EFF-2026-002", "EFF-2026-010"]);
    }

    #[test]
    fn parse_id_number_round_trip() {
        assert_eq!(parse_id_number(Path::new("EFF-2026-042.md")), Some(42));
        assert_eq!(parse_id_number(Path::new("README.md")), None);
        assert_eq!(parse_id_number(Path::new("EFF-2025-001.md")), None);
    }
}
