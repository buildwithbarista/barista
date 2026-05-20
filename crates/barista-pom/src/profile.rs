// SPDX-License-Identifier: MIT OR Apache-2.0

//! Post-merge POM interpretation passes:
//! - dependencyManagement application
//! - BOM imports (`<scope>import</scope>` dependencies)
//! - profile activation
//!
//! These run after [`crate::effective::build_effective`] has merged the
//! parent chain and interpolated `${...}` references. Together they
//! produce a "fully resolved" POM whose `<dependencies>` carry concrete
//! versions and active-profile contributions are folded in.
//!
//! ## Pipeline
//!
//! 1. **build_effective** on the input POM (parent merge + interpolation).
//! 2. **BOM imports** — every entry in `<dependencyManagement>` with
//!    `<scope>import</scope>` and `<type>pom</type>` is resolved via the
//!    [`ParentResolver`], recursively interpreted, and its
//!    `<dependencyManagement>` entries are spliced in. Cycle-detected
//!    and depth-capped.
//! 3. **Profile activation** — every `<profile>` is tested against the
//!    [`ActivationContext`]; active profiles contribute properties,
//!    dependencies, depMgt, plugins, repositories, modules. The result
//!    is re-interpolated so a profile-supplied property can be picked
//!    up by `${...}` references elsewhere in the POM.
//! 4. **depMgt application** — every entry in `<dependencies>` whose
//!    `<version>` (or `<scope>` / `<exclusions>`) is absent inherits
//!    from the matching `<dependencyManagement>` entry.
//!
//! ## Maven fidelity
//!
//! The Maven reference implementation lives in `maven-model-builder`:
//!
//! - BOM import: `org.apache.maven.model.composition.DefaultDependencyManagementImporter`
//! - depMgt injection: `org.apache.maven.model.management.DefaultDependencyManagementInjector`
//! - Profile activators: `org.apache.maven.model.profile.activation.*`
//!
//! Where this implementation deviates from Maven, the rationale is
//! noted in the relevant function's doc comment.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use indexmap::IndexMap;

use crate::effective::{EffectiveError, EffectivePom, ParentResolver, build_effective};
use crate::raw::{Properties, RawDependency, RawParent, RawPom, RawProfile, RawRepository};

/// Maximum number of nested BOM imports we'll follow before declaring
/// the graph pathological. Real-world projects rarely exceed 3 levels;
/// 16 leaves comfortable headroom.
pub const MAX_BOM_IMPORT_DEPTH: usize = 16;

// ---------------------------------------------------------------------------
// Activation context
// ---------------------------------------------------------------------------

/// Values the profile activators consult to decide whether a profile is
/// active for a given build.
///
/// `Default` yields an empty context: no JDK, no OS, no user
/// properties, no `-P` flags, no basedir. This is appropriate for unit
/// tests and for environments where you want only `activeByDefault`
/// profiles to fire.
#[derive(Debug, Clone, Default)]
pub struct ActivationContext {
    /// e.g. `"21.0.4"` — matched against `<activation><jdk>` ranges.
    pub jdk_version: Option<String>,
    /// e.g. `"Mac OS X"` — matched against `<activation><os><name>`.
    pub os_name: Option<String>,
    /// e.g. `"aarch64"` — matched against `<activation><os><arch>`.
    pub os_arch: Option<String>,
    /// `"unix"` | `"windows"` | `"mac"` — matched against
    /// `<activation><os><family>`.
    pub os_family: Option<String>,
    /// Matched against `<activation><os><version>`.
    pub os_version: Option<String>,
    /// CLI `-D` flags + environment-derived user properties.
    pub user_properties: HashMap<String, String>,
    /// Profile ids explicitly forced on with `-P id`.
    pub active_profile_ids: HashSet<String>,
    /// Profile ids explicitly forced off with `-P !id`.
    pub inactive_profile_ids: HashSet<String>,
    /// Project base directory, used by `<file><exists>` activation.
    pub basedir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors produced when resolving a POM (BOM imports + profile
/// activation + depMgt application). Wraps [`EffectiveError`] so a
/// single error type covers the whole pipeline.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// Failure during parent-chain merge or `${...}` interpolation.
    #[error(transparent)]
    Effective(#[from] EffectiveError),
    /// BOM import graph contains a cycle.
    #[error("BOM import loop detected at {coords}")]
    BomImportCycle { coords: String },
    /// BOM import graph exceeded [`MAX_BOM_IMPORT_DEPTH`].
    #[error("BOM import depth exceeds maximum ({max})")]
    BomImportTooDeep { max: usize },
    /// A BOM-import dependency could not be resolved by the
    /// [`ParentResolver`].
    #[error("BOM import {coords} could not be resolved: {reason}")]
    BomImportResolution { coords: String, reason: String },
    /// A `<dependencyManagement>` entry referenced a coord whose
    /// `<version>` was missing after the parent chain, BOM imports,
    /// and interpolation all completed.
    #[error("dependencyManagement dependency for {coords} has no version after parent + BOM merge")]
    UnresolvedDepMgtVersion { coords: String },
    /// A `<dependency>` has no `<version>` and no matching
    /// `<dependencyManagement>` entry to fall back on.
    #[error("dependency {coords} has no version and no dependencyManagement default")]
    UnresolvedDependencyVersion { coords: String },
    /// A `<activation>` block was syntactically malformed (e.g. a
    /// version range that doesn't parse).
    #[error("invalid profile activation expression: {detail}")]
    InvalidActivation { detail: String },
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Result of full POM resolution. `pom` is the fully-resolved
/// [`RawPom`]; the other fields record what happened during
/// interpretation.
#[derive(Debug, Clone)]
pub struct ResolvedPom {
    /// The fully resolved POM: parent-merged, interpolated,
    /// BOM-imported, profile-applied, depMgt-applied. Every
    /// `<dependency>` carries a concrete version.
    pub pom: RawPom,
    /// The effective-POM intermediate (parent chain + interpolation
    /// audit trail). Useful for debugging and golden-test output.
    pub effective: EffectivePom,
    /// Ids of profiles that fired (in document order). Profiles with
    /// no `<id>` are recorded with their index.
    pub active_profile_ids: Vec<String>,
    /// Coordinates (`"group:artifact:version"`) of every BOM whose
    /// `<dependencyManagement>` was spliced in.
    pub imported_boms: Vec<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Resolve a POM end-to-end: parent merge + interpolation + BOM
/// imports + profile activation + depMgt application.
///
/// Returns a [`ResolvedPom`] whose `<dependencies>` carry concrete
/// versions (inherited from `<dependencyManagement>` if needed),
/// active-profile contributions have been folded in, and BOM-imported
/// depMgt entries have been spliced.
pub fn resolve_pom<R: ParentResolver>(
    root: RawPom,
    resolver: &mut R,
    ctx: &ActivationContext,
) -> Result<ResolvedPom, ResolveError> {
    // Pass 0: parent merge + interpolation.
    let effective = build_effective(root, resolver)?;
    let mut pom = effective.pom.clone();

    // Pass 1: BOM imports.
    let mut imported_boms = Vec::new();
    let mut import_stack: HashSet<(String, String, String)> = HashSet::new();
    expand_bom_imports(&mut pom, resolver, &mut imported_boms, &mut import_stack, 0)?;

    // Pass 2: profile activation.
    let active_profile_ids = apply_profiles(&mut pom, ctx)?;

    // After splicing BOM imports and profile contributions, freshly
    // introduced `${...}` placeholders need a second interpolation
    // pass. The POM no longer has a `<parent>` (build_effective
    // cleared it), so re-running build_effective just re-interpolates.
    if !imported_boms.is_empty() || !active_profile_ids.is_empty() {
        let reinterp = build_effective(pom, resolver)?;
        pom = reinterp.pom;
    }

    // Pass 3: depMgt application.
    apply_dependency_management(&mut pom)?;

    Ok(ResolvedPom {
        pom,
        effective,
        active_profile_ids,
        imported_boms,
    })
}

// ===========================================================================
// Pass 1: BOM imports
// ===========================================================================

/// Recursively expand `<scope>import</scope>` entries in
/// `pom.dependency_management`.
///
/// Maven's behaviour: imported BOMs contribute their
/// `<dependencyManagement>` entries at the position of the import
/// declaration; entries that come *later* in the importing POM's
/// depMgt override imported entries on coord collision. Since depMgt
/// is processed with "first-wins" semantics, this means imported
/// entries appear *after* the directly-declared ones. To keep that
/// invariant, we collect imported entries and append them *after*
/// removing the import declarations themselves.
fn expand_bom_imports<R: ParentResolver>(
    pom: &mut RawPom,
    resolver: &mut R,
    imported_boms: &mut Vec<String>,
    stack: &mut HashSet<(String, String, String)>,
    depth: usize,
) -> Result<(), ResolveError> {
    if depth > MAX_BOM_IMPORT_DEPTH {
        return Err(ResolveError::BomImportTooDeep {
            max: MAX_BOM_IMPORT_DEPTH,
        });
    }
    let Some(dm) = pom.dependency_management.as_mut() else {
        return Ok(());
    };

    // Partition: import declarations vs. real depMgt entries.
    let mut imports: Vec<RawDependency> = Vec::new();
    let mut kept: Vec<RawDependency> = Vec::with_capacity(dm.dependencies.len());
    for d in std::mem::take(&mut dm.dependencies) {
        if is_bom_import(&d) {
            imports.push(d);
        } else {
            kept.push(d);
        }
    }
    dm.dependencies = kept;

    // For each import, resolve the BOM, recursively expand its own
    // imports, and splice its depMgt entries.
    let mut appended: Vec<RawDependency> = Vec::new();
    for imp in imports {
        let version = imp.version.as_deref().unwrap_or("").to_string();
        if imp.group_id.is_empty() || imp.artifact_id.is_empty() || version.is_empty() {
            return Err(ResolveError::UnresolvedDepMgtVersion {
                coords: format!("{}:{}:{}", imp.group_id, imp.artifact_id, version),
            });
        }
        let key = (
            imp.group_id.clone(),
            imp.artifact_id.clone(),
            version.clone(),
        );
        let coords_str = format!("{}:{}:{}", key.0, key.1, key.2);

        if !stack.insert(key.clone()) {
            return Err(ResolveError::BomImportCycle { coords: coords_str });
        }

        let bom_raw = resolver
            .resolve(&RawParent {
                group_id: imp.group_id.clone(),
                artifact_id: imp.artifact_id.clone(),
                version: version.clone(),
                relative_path: None,
            })
            .map_err(|reason| ResolveError::BomImportResolution {
                coords: coords_str.clone(),
                reason,
            })?;

        // Recursively interpret the BOM. We run build_effective so the
        // BOM's own parent chain + interpolation happens, then recurse
        // into BOM imports it may itself declare.
        let bom_effective = build_effective(bom_raw, resolver)?;
        let mut bom_pom = bom_effective.pom;
        expand_bom_imports(&mut bom_pom, resolver, imported_boms, stack, depth + 1)?;

        if let Some(bom_dm) = bom_pom.dependency_management.take() {
            appended.extend(bom_dm.dependencies);
        }
        imported_boms.push(coords_str);
        stack.remove(&key);
    }

    // Append imported entries *after* the directly-declared ones so
    // that first-wins depMgt semantics give the importing POM
    // precedence.
    if !appended.is_empty() {
        if let Some(dm) = pom.dependency_management.as_mut() {
            dm.dependencies.extend(appended);
        }
    }

    Ok(())
}

fn is_bom_import(d: &RawDependency) -> bool {
    d.scope.as_deref() == Some("import") && d.r#type.as_deref() == Some("pom")
}

// ===========================================================================
// Pass 2: profile activation
// ===========================================================================

/// Evaluate every profile's `<activation>` and splice the contributions
/// of every active profile into the POM. Returns the ids of the active
/// profiles, in document order.
fn apply_profiles(pom: &mut RawPom, ctx: &ActivationContext) -> Result<Vec<String>, ResolveError> {
    // Maven semantics: `activeByDefault` profiles fire only when NO
    // other profile has been explicitly or implicitly activated. We
    // implement this by computing the set of non-default activations
    // first.
    let profiles = std::mem::take(&mut pom.profiles);

    // First pass: classify each profile.
    enum Status {
        Active,
        Inactive,
        DefaultCandidate,
    }
    let mut statuses: Vec<Status> = Vec::with_capacity(profiles.len());
    let mut any_explicit_active = false;
    for p in &profiles {
        let id = p.id.as_deref().unwrap_or("");
        if !id.is_empty() && ctx.inactive_profile_ids.contains(id) {
            statuses.push(Status::Inactive);
            continue;
        }
        if !id.is_empty() && ctx.active_profile_ids.contains(id) {
            statuses.push(Status::Active);
            any_explicit_active = true;
            continue;
        }
        match evaluate_activation(p, ctx)? {
            ActivationOutcome::Active => {
                statuses.push(Status::Active);
                any_explicit_active = true;
            }
            ActivationOutcome::Inactive => statuses.push(Status::Inactive),
            ActivationOutcome::DefaultCandidate => statuses.push(Status::DefaultCandidate),
            ActivationOutcome::Unconfigured => statuses.push(Status::Inactive),
        }
    }

    // Resolve default candidates.
    for s in statuses.iter_mut() {
        if matches!(s, Status::DefaultCandidate) {
            *s = if any_explicit_active {
                Status::Inactive
            } else {
                Status::Active
            };
        }
    }

    let mut active_ids = Vec::new();
    let mut inactive: Vec<RawProfile> = Vec::new();
    for (i, (p, s)) in profiles.into_iter().zip(statuses).enumerate() {
        match s {
            Status::Active => {
                let id = p.id.clone().unwrap_or_else(|| format!("<profile-{i}>"));
                active_ids.push(id);
                splice_profile(pom, p);
            }
            Status::Inactive | Status::DefaultCandidate => {
                inactive.push(p);
            }
        }
    }
    // Preserve inactive profiles on the POM (some downstream
    // consumers — e.g. IDEs offering profile toggles — want them).
    pom.profiles = inactive;

    Ok(active_ids)
}

#[derive(Debug)]
enum ActivationOutcome {
    /// At least one activator said "yes" and none said "no".
    Active,
    /// At least one activator said "no" (a negative match).
    Inactive,
    /// No activators present except `<activeByDefault>true</activeByDefault>`.
    DefaultCandidate,
    /// No `<activation>` block at all, and no `-P` activation.
    Unconfigured,
}

/// Evaluate a profile's `<activation>` block.
///
/// Multiple activators within a single `<activation>` block are AND-ed
/// together in Maven: every present activator must say "yes" for the
/// profile to be active. `<activeByDefault>` is special: a profile
/// with only `activeByDefault=true` becomes a default candidate
/// (active iff no other profile fires).
fn evaluate_activation(
    profile: &RawProfile,
    ctx: &ActivationContext,
) -> Result<ActivationOutcome, ResolveError> {
    let Some(act) = profile.activation.as_ref() else {
        return Ok(ActivationOutcome::Unconfigured);
    };

    let active_by_default = act.active_by_default.as_deref() == Some("true");
    let has_jdk = act.jdk.is_some();
    let has_os = act.os.is_some();
    let has_property = act.property.is_some();
    let has_file = act.file.is_some();
    let any_real_activator = has_jdk || has_os || has_property || has_file;

    if !any_real_activator {
        if active_by_default {
            return Ok(ActivationOutcome::DefaultCandidate);
        }
        return Ok(ActivationOutcome::Unconfigured);
    }

    // Each present activator votes. ANY "no" makes the profile
    // inactive. ALL must say "yes" (or be absent) for active.
    if has_jdk {
        let spec = act.jdk.as_deref().unwrap_or("");
        let Some(have) = ctx.jdk_version.as_deref() else {
            return Ok(ActivationOutcome::Inactive);
        };
        if !jdk_matches(spec, have)? {
            return Ok(ActivationOutcome::Inactive);
        }
    }
    if has_os && !os_matches(act.os.as_ref().unwrap(), ctx) {
        return Ok(ActivationOutcome::Inactive);
    }
    if has_property {
        let prop = act.property.as_ref().unwrap();
        if !property_matches(prop.name.as_deref(), prop.value.as_deref(), ctx) {
            return Ok(ActivationOutcome::Inactive);
        }
    }
    if has_file {
        let f = act.file.as_ref().unwrap();
        if !file_matches(f.exists.as_deref(), f.missing.as_deref(), ctx) {
            return Ok(ActivationOutcome::Inactive);
        }
    }
    Ok(ActivationOutcome::Active)
}

// -- JDK activator -----------------------------------------------------------

/// Maven's `<jdk>` spec syntax:
///
/// - `"1.8"` — prefix match: matches `1.8`, `1.8.x`. Also Maven treats
///   `"8"` as matching `8.x.x` (modern JDK numbering).
/// - `"[1.8,1.9)"` — half-open interval. Brackets `[]` are inclusive,
///   parens `()` are exclusive. An empty side means unbounded.
/// - `"!1.8"` — negation of a prefix match.
fn jdk_matches(spec: &str, have: &str) -> Result<bool, ResolveError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(true);
    }
    if let Some(rest) = spec.strip_prefix('!') {
        return Ok(!jdk_prefix_matches(rest.trim(), have));
    }
    if spec.starts_with('[') || spec.starts_with('(') {
        return jdk_range_matches(spec, have);
    }
    Ok(jdk_prefix_matches(spec, have))
}

fn jdk_prefix_matches(spec: &str, have: &str) -> bool {
    // Exact match or prefix-followed-by-dot.
    if have == spec {
        return true;
    }
    if let Some(rest) = have.strip_prefix(spec) {
        if rest.starts_with('.') {
            return true;
        }
    }
    false
}

fn jdk_range_matches(spec: &str, have: &str) -> Result<bool, ResolveError> {
    // Format: [lo,hi] / [lo,hi) / (lo,hi] / (lo,hi). Either side may
    // be empty meaning unbounded.
    let bytes = spec.as_bytes();
    if bytes.len() < 3 {
        return Err(ResolveError::InvalidActivation {
            detail: format!("jdk range too short: {spec:?}"),
        });
    }
    let lo_inclusive = bytes[0] == b'[';
    let hi_inclusive = bytes[bytes.len() - 1] == b']';
    if !(bytes[0] == b'[' || bytes[0] == b'(')
        || !(bytes[bytes.len() - 1] == b']' || bytes[bytes.len() - 1] == b')')
    {
        return Err(ResolveError::InvalidActivation {
            detail: format!("jdk range missing brackets: {spec:?}"),
        });
    }
    let inner = &spec[1..spec.len() - 1];
    let (lo_s, hi_s) = inner
        .split_once(',')
        .ok_or_else(|| ResolveError::InvalidActivation {
            detail: format!("jdk range missing comma: {spec:?}"),
        })?;
    let lo = parse_version(lo_s.trim());
    let hi = parse_version(hi_s.trim());
    let v = parse_version(have);

    if let Some(lo) = lo {
        let cmp = compare_versions(&v, &lo);
        if (lo_inclusive && cmp.is_lt()) || (!lo_inclusive && !cmp.is_gt()) {
            return Ok(false);
        }
    }
    if let Some(hi) = hi {
        let cmp = compare_versions(&v, &hi);
        if (hi_inclusive && cmp.is_gt()) || (!hi_inclusive && !cmp.is_lt()) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Parse a dotted version string (e.g. `"1.8.0_321"`, `"21.0.4"`) into
/// a vector of numeric components. Non-numeric trailing parts are
/// dropped — only the numeric prefix participates in comparison.
/// Returns `None` for an empty string (unbounded sentinel).
fn parse_version(s: &str) -> Option<Vec<u64>> {
    if s.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for piece in s.split(['.', '_', '-']) {
        if piece.is_empty() {
            continue;
        }
        let num: String = piece.chars().take_while(|c| c.is_ascii_digit()).collect();
        if num.is_empty() {
            break;
        }
        if let Ok(n) = num.parse::<u64>() {
            out.push(n);
        } else {
            break;
        }
    }
    Some(out)
}

fn compare_versions(a: &Option<Vec<u64>>, b: &[u64]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let empty = Vec::new();
    let a = a.as_ref().unwrap_or(&empty);
    let len = a.len().max(b.len());
    for i in 0..len {
        let ai = a.get(i).copied().unwrap_or(0);
        let bi = b.get(i).copied().unwrap_or(0);
        match ai.cmp(&bi) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

// -- OS activator ------------------------------------------------------------

fn os_matches(os: &crate::raw::XmlValue, ctx: &ActivationContext) -> bool {
    // Each present sub-element must match. Children syntax:
    //   <name>...</name>, <family>...</family>, <arch>...</arch>,
    //   <version>...</version>. Each may be negated with leading `!`.
    fn child_text<'a>(v: &'a crate::raw::XmlValue, name: &str) -> Option<&'a str> {
        v.children
            .get(name)
            .and_then(|vs| vs.first())
            .and_then(|c| c.text.as_deref())
    }
    fn check(spec: Option<&str>, have: Option<&str>) -> bool {
        let Some(spec) = spec else {
            return true;
        };
        let (negate, raw) = if let Some(rest) = spec.strip_prefix('!') {
            (true, rest.trim())
        } else {
            (false, spec.trim())
        };
        let matches = match have {
            Some(h) => h.eq_ignore_ascii_case(raw),
            None => false,
        };
        if negate { !matches } else { matches }
    }
    check(child_text(os, "name"), ctx.os_name.as_deref())
        && check(child_text(os, "family"), ctx.os_family.as_deref())
        && check(child_text(os, "arch"), ctx.os_arch.as_deref())
        && check(child_text(os, "version"), ctx.os_version.as_deref())
}

// -- property activator ------------------------------------------------------

fn property_matches(
    name: Option<&str>,
    expected_value: Option<&str>,
    ctx: &ActivationContext,
) -> bool {
    let Some(name) = name else {
        return false;
    };
    let (negate, key) = if let Some(rest) = name.strip_prefix('!') {
        (true, rest.trim())
    } else {
        (false, name.trim())
    };
    let actual = ctx.user_properties.get(key);
    let matches = match (actual, expected_value) {
        (Some(a), Some(want)) => a == want,
        (Some(_), None) => true,
        (None, _) => false,
    };
    if negate { !matches } else { matches }
}

// -- file activator (v0.1 stub) ----------------------------------------------

/// File-based activation. v0.1 honors `<exists>` and `<missing>`
/// against `ctx.basedir`. When `basedir` is `None`, file activation
/// declines (returns false) so tests and parse-only pipelines don't
/// trip on filesystem state that doesn't exist yet.
fn file_matches(exists: Option<&str>, missing: Option<&str>, ctx: &ActivationContext) -> bool {
    let Some(base) = ctx.basedir.as_ref() else {
        return false;
    };
    if let Some(path) = exists {
        let full = base.join(path);
        if !full.exists() {
            return false;
        }
    }
    if let Some(path) = missing {
        let full = base.join(path);
        if full.exists() {
            return false;
        }
    }
    true
}

// -- splicing ---------------------------------------------------------------

/// Splice an active profile's contributions into the POM.
fn splice_profile(pom: &mut RawPom, profile: RawProfile) {
    // Properties: profile wins on collision (Maven semantic).
    for (k, v) in profile.properties.entries {
        pom.properties.entries.insert(k, v);
    }

    // Dependencies and depMgt: append.
    pom.dependencies.extend(profile.dependencies);
    if let Some(dm) = profile.dependency_management {
        match pom.dependency_management.as_mut() {
            Some(existing) => existing.dependencies.extend(dm.dependencies),
            None => pom.dependency_management = Some(dm),
        }
    }

    // Build: append plugins (parent-style merge).
    if let Some(build) = profile.build {
        let target = pom.build.get_or_insert_with(Default::default);
        target.plugins.extend(build.plugins);
        if let Some(pm) = build.plugin_management {
            match target.plugin_management.as_mut() {
                Some(existing) => existing.plugins.extend(pm.plugins),
                None => target.plugin_management = Some(pm),
            }
        }
        target.resources.extend(build.resources);
        target.test_resources.extend(build.test_resources);
        target.filters.extend(build.filters);
        // Scalars: profile wins if set.
        macro_rules! winset {
            ($field:ident) => {
                if build.$field.is_some() {
                    target.$field = build.$field;
                }
            };
        }
        winset!(source_directory);
        winset!(script_source_directory);
        winset!(test_source_directory);
        winset!(output_directory);
        winset!(test_output_directory);
        winset!(final_name);
        winset!(default_goal);
        winset!(directory);
    }

    // Repositories / plugin repositories: append with id-dedup
    // (profile wins).
    pom.repositories = dedup_repositories(&pom.repositories, &profile.repositories);
    pom.plugin_repositories =
        dedup_repositories(&pom.plugin_repositories, &profile.plugin_repositories);

    // Modules: append, deduping.
    for m in profile.modules {
        if !pom.modules.contains(&m) {
            pom.modules.push(m);
        }
    }
}

fn dedup_repositories(
    existing: &[RawRepository],
    incoming: &[RawRepository],
) -> Vec<RawRepository> {
    let incoming_ids: HashSet<String> = incoming.iter().filter_map(|r| r.id.clone()).collect();
    let mut out: Vec<RawRepository> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for r in existing {
        if let Some(id) = r.id.as_ref() {
            if incoming_ids.contains(id) {
                continue;
            }
            seen.insert(id.clone());
        }
        out.push(r.clone());
    }
    for r in incoming {
        if let Some(id) = r.id.as_ref() {
            if !seen.insert(id.clone()) {
                continue;
            }
        }
        out.push(r.clone());
    }
    out
}

// ===========================================================================
// Pass 3: depMgt application
// ===========================================================================

/// Walk `pom.dependencies` (and `pom.dependency_management.dependencies`)
/// and inherit `<version>` / `<scope>` / `<exclusions>` from matching
/// `<dependencyManagement>` entries.
///
/// Key is `(group, artifact, type, classifier)`. `type` defaults to
/// `"jar"`, `classifier` defaults to `None`.
///
/// Within `<dependencyManagement>`, first-occurrence wins (Maven's
/// `LATER WINS` rule applies to direct `<dependencies>`, not depMgt
/// itself — see DefaultDependencyManagementInjector#mergeDependency).
fn apply_dependency_management(pom: &mut RawPom) -> Result<(), ResolveError> {
    // First, let depMgt entries themselves inherit from BOM-imported
    // entries: if a directly-declared depMgt entry has no version,
    // borrow from a later (BOM-imported) entry with the same key.
    if let Some(dm) = pom.dependency_management.as_mut() {
        let snapshot = dm.dependencies.clone();
        let by_key = build_index_from(&snapshot);
        for d in dm.dependencies.iter_mut() {
            if d.version.is_some() {
                continue;
            }
            // Skip BOM-import declarations (they were already expanded
            // by Pass 1; any remaining ones are pathological but we
            // don't synthesize a version for them here).
            if is_bom_import(d) {
                continue;
            }
            let key = DepKey::of(d);
            if let Some(other) = by_key.get(&key) {
                if !std::ptr::eq(other, d) {
                    if d.version.is_none() {
                        d.version = other.version.clone();
                    }
                    if d.scope.is_none() {
                        d.scope = other.scope.clone();
                    }
                    if d.exclusions.is_empty() {
                        d.exclusions = other.exclusions.clone();
                    }
                }
            }
        }
    }

    // Build the index AFTER intra-depMgt inheritance so BOM-imported
    // entries can promote versionless directly-declared ones.
    let dm_index: IndexMap<DepKey, RawDependency> = build_dm_index(pom);

    // Apply to project dependencies.
    for d in pom.dependencies.iter_mut() {
        // system-scoped deps are special — Maven does not inject
        // anything into them from depMgt.
        if d.scope.as_deref() == Some("system") {
            continue;
        }
        if let Some(default) = dm_index.get(&DepKey::of(d)) {
            // Versions: inherit only when child is absent. Maven
            // explicitly DOES NOT override a declared version from
            // depMgt; depMgt provides defaults, not enforcement.
            if d.version.is_none() {
                d.version = default.version.clone();
            }
            if d.scope.is_none() {
                d.scope = default.scope.clone();
            }
            if d.exclusions.is_empty() {
                d.exclusions = default.exclusions.clone();
            }
            // We deliberately do NOT inherit <optional> from depMgt —
            // optional is per-dependency-declaration semantics and
            // Maven excludes it from the injector.
        }
        if d.version.is_none() {
            return Err(ResolveError::UnresolvedDependencyVersion {
                coords: format!("{}:{}", d.group_id, d.artifact_id),
            });
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DepKey {
    group: String,
    artifact: String,
    /// Defaults to `"jar"` when unset, matching Maven.
    ty: String,
    /// Empty string when no classifier — `None`/`Some("")` collapsed.
    classifier: String,
}

impl DepKey {
    fn of(d: &RawDependency) -> Self {
        Self {
            group: d.group_id.clone(),
            artifact: d.artifact_id.clone(),
            ty: d.r#type.clone().unwrap_or_else(|| "jar".to_string()),
            classifier: d.classifier.clone().unwrap_or_default(),
        }
    }
}

fn build_dm_index(pom: &RawPom) -> IndexMap<DepKey, RawDependency> {
    let mut out: IndexMap<DepKey, RawDependency> = IndexMap::new();
    let Some(dm) = pom.dependency_management.as_ref() else {
        return out;
    };
    for d in &dm.dependencies {
        if is_bom_import(d) {
            continue;
        }
        // First-occurrence wins: don't overwrite.
        out.entry(DepKey::of(d)).or_insert_with(|| d.clone());
    }
    out
}

fn build_index_from(deps: &[RawDependency]) -> IndexMap<DepKey, RawDependency> {
    let mut out: IndexMap<DepKey, RawDependency> = IndexMap::new();
    for d in deps {
        if is_bom_import(d) {
            continue;
        }
        if d.version.is_some() {
            out.entry(DepKey::of(d)).or_insert_with(|| d.clone());
        }
    }
    out
}

// Re-export for downstream code that prefers `barista_pom::*` paths.
#[allow(dead_code)]
fn _props_for_doc_link(_p: &Properties) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{
        DependencyManagement, RawActivation, RawActivationFile, RawActivationProperty, RawBuild,
        RawDependency, RawExclusion, RawParent, RawPlugin, RawPom, RawProfile, RawRepository,
        XmlValue,
    };
    use std::collections::HashMap;

    // ---- fixture helpers --------------------------------------------------

    #[derive(Default)]
    struct Fixture {
        poms: HashMap<(String, String, String), RawPom>,
    }

    impl Fixture {
        fn add(&mut self, pom: RawPom) {
            let key = (
                pom.group_id.clone().unwrap_or_default(),
                pom.artifact_id.clone(),
                pom.version.clone().unwrap_or_default(),
            );
            self.poms.insert(key, pom);
        }
    }

    impl ParentResolver for Fixture {
        fn resolve(&mut self, parent: &RawParent) -> Result<RawPom, String> {
            self.poms
                .get(&(
                    parent.group_id.clone(),
                    parent.artifact_id.clone(),
                    parent.version.clone(),
                ))
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "not in fixture: {}:{}:{}",
                        parent.group_id, parent.artifact_id, parent.version
                    )
                })
        }
    }

    fn pom(group: &str, artifact: &str, version: &str) -> RawPom {
        RawPom {
            model_version: "4.0.0".to_string(),
            group_id: Some(group.to_string()),
            artifact_id: artifact.to_string(),
            version: Some(version.to_string()),
            packaging: "jar".to_string(),
            ..RawPom::default()
        }
    }

    fn dep(group: &str, artifact: &str, version: Option<&str>) -> RawDependency {
        RawDependency {
            group_id: group.to_string(),
            artifact_id: artifact.to_string(),
            version: version.map(str::to_string),
            ..RawDependency::default()
        }
    }

    fn import(group: &str, artifact: &str, version: &str) -> RawDependency {
        RawDependency {
            group_id: group.to_string(),
            artifact_id: artifact.to_string(),
            version: Some(version.to_string()),
            scope: Some("import".to_string()),
            r#type: Some("pom".to_string()),
            ..RawDependency::default()
        }
    }

    fn ctx_empty() -> ActivationContext {
        ActivationContext::default()
    }

    // ---- depMgt application ----------------------------------------------

    #[test]
    fn test_01_bare_dep_inherits_version_from_depmgt() {
        let mut f = Fixture::default();
        let mut p = pom("g", "a", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("io.x", "lib", Some("3.2.1"))],
        });
        p.dependencies.push(dep("io.x", "lib", None));

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.dependencies[0].version.as_deref(), Some("3.2.1"));
    }

    #[test]
    fn test_02_versioned_dep_not_overridden() {
        let mut f = Fixture::default();
        let mut p = pom("g", "a", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("io.x", "lib", Some("3.2.1"))],
        });
        p.dependencies.push(dep("io.x", "lib", Some("9.9.9")));

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.dependencies[0].version.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn test_03_scope_inherited_from_depmgt() {
        let mut f = Fixture::default();
        let mut p = pom("g", "a", "1");
        let mut dm_dep = dep("io.x", "lib", Some("1"));
        dm_dep.scope = Some("test".to_string());
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![dm_dep],
        });
        p.dependencies.push(dep("io.x", "lib", None));

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.dependencies[0].scope.as_deref(), Some("test"));
    }

    #[test]
    fn test_04_exclusions_inherited_from_depmgt() {
        let mut f = Fixture::default();
        let mut p = pom("g", "a", "1");
        let mut dm_dep = dep("io.x", "lib", Some("1"));
        dm_dep.exclusions = vec![RawExclusion {
            group_id: "junk".to_string(),
            artifact_id: "stuff".to_string(),
        }];
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![dm_dep],
        });
        p.dependencies.push(dep("io.x", "lib", None));

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.dependencies[0].exclusions.len(), 1);
        assert_eq!(r.pom.dependencies[0].exclusions[0].group_id, "junk");
    }

    #[test]
    fn test_05_classifier_distinguishes_depmgt_match() {
        let mut f = Fixture::default();
        let mut p = pom("g", "a", "1");
        let mut dm_dep = dep("io.x", "lib", Some("1.0"));
        dm_dep.classifier = Some("linux".to_string());
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![dm_dep],
        });
        // A dep with no classifier should NOT match the linux entry.
        p.dependencies.push(dep("io.x", "lib", Some("2.0")));
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.dependencies[0].version.as_deref(), Some("2.0"));
    }

    #[test]
    fn test_06_system_scope_not_overridden() {
        let mut f = Fixture::default();
        let mut p = pom("g", "a", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("io.x", "lib", Some("1.0"))],
        });
        let mut sys_dep = dep("io.x", "lib", Some("9.9"));
        sys_dep.scope = Some("system".to_string());
        sys_dep.system_path = Some("/tmp/x.jar".to_string());
        p.dependencies.push(sys_dep);

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        // System dep keeps its own version, scope.
        assert_eq!(r.pom.dependencies[0].version.as_deref(), Some("9.9"));
        assert_eq!(r.pom.dependencies[0].scope.as_deref(), Some("system"));
    }

    #[test]
    fn test_07_unresolved_dep_errors() {
        let mut f = Fixture::default();
        let mut p = pom("g", "a", "1");
        p.dependencies.push(dep("io.x", "lib", None));
        let err = resolve_pom(p, &mut f, &ctx_empty()).unwrap_err();
        assert!(matches!(
            err,
            ResolveError::UnresolvedDependencyVersion { .. }
        ));
    }

    // ---- BOM imports -----------------------------------------------------

    #[test]
    fn test_08_single_bom_import_contributes_depmgt() {
        let mut f = Fixture::default();
        let mut bom = pom("org.x", "bom", "1.0");
        bom.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("org.y", "lib", Some("4.5.6"))],
        });
        f.add(bom);

        let mut p = pom("g", "a", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![import("org.x", "bom", "1.0")],
        });
        p.dependencies.push(dep("org.y", "lib", None));

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.imported_boms.len(), 1);
        assert_eq!(r.pom.dependencies[0].version.as_deref(), Some("4.5.6"));
    }

    #[test]
    fn test_09_two_boms_first_wins_within_depmgt() {
        let mut f = Fixture::default();
        // BOM A says lib=1.0
        let mut bom_a = pom("org.x", "boma", "1");
        bom_a.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("org.y", "lib", Some("1.0"))],
        });
        f.add(bom_a);
        // BOM B says lib=2.0
        let mut bom_b = pom("org.x", "bomb", "1");
        bom_b.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("org.y", "lib", Some("2.0"))],
        });
        f.add(bom_b);

        let mut p = pom("g", "a", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![import("org.x", "boma", "1"), import("org.x", "bomb", "1")],
        });
        p.dependencies.push(dep("org.y", "lib", None));

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        // BOM A came first → its version wins (1.0).
        assert_eq!(r.pom.dependencies[0].version.as_deref(), Some("1.0"));
    }

    #[test]
    fn test_10_directly_declared_overrides_bom_imported() {
        let mut f = Fixture::default();
        let mut bom = pom("org.x", "bom", "1");
        bom.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("org.y", "lib", Some("9.9.9"))],
        });
        f.add(bom);

        let mut p = pom("g", "a", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![
                // Directly-declared override comes first; first-wins.
                dep("org.y", "lib", Some("3.0.0")),
                import("org.x", "bom", "1"),
            ],
        });
        p.dependencies.push(dep("org.y", "lib", None));

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.dependencies[0].version.as_deref(), Some("3.0.0"));
    }

    #[test]
    fn test_11_bom_import_cycle_detected() {
        let mut f = Fixture::default();
        // BOM A imports BOM B; BOM B imports BOM A.
        let mut a = pom("org.x", "a", "1");
        a.dependency_management = Some(DependencyManagement {
            dependencies: vec![import("org.x", "b", "1")],
        });
        let mut b = pom("org.x", "b", "1");
        b.dependency_management = Some(DependencyManagement {
            dependencies: vec![import("org.x", "a", "1")],
        });
        f.add(a);
        f.add(b);

        let mut p = pom("g", "p", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![import("org.x", "a", "1")],
        });
        let err = resolve_pom(p, &mut f, &ctx_empty()).unwrap_err();
        assert!(matches!(err, ResolveError::BomImportCycle { .. }));
    }

    #[test]
    fn test_12_bom_depth_cap_exceeded() {
        let mut f = Fixture::default();
        // Chain: bom0 → bom1 → bom2 → ... → bomN
        let n = MAX_BOM_IMPORT_DEPTH + 2;
        for i in 0..n {
            let mut b = pom("org.d", &format!("b{i}"), "1");
            if i + 1 < n {
                b.dependency_management = Some(DependencyManagement {
                    dependencies: vec![import("org.d", &format!("b{}", i + 1), "1")],
                });
            }
            f.add(b);
        }

        let mut p = pom("g", "p", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![import("org.d", "b0", "1")],
        });
        let err = resolve_pom(p, &mut f, &ctx_empty()).unwrap_err();
        assert!(matches!(err, ResolveError::BomImportTooDeep { .. }));
    }

    #[test]
    fn test_13_bom_unresolved_error() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![import("missing", "bom", "1")],
        });
        let err = resolve_pom(p, &mut f, &ctx_empty()).unwrap_err();
        // The bom is missing → resolver returns error inside
        // build_effective (no parent declared) → BomImportResolution.
        assert!(matches!(err, ResolveError::BomImportResolution { .. }));
    }

    // ---- profile activation ----------------------------------------------

    #[test]
    fn test_14_active_by_default_fires_when_no_other() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("dev".to_string()),
            activation: Some(RawActivation {
                active_by_default: Some("true".to_string()),
                ..RawActivation::default()
            }),
            properties: crate::raw::Properties {
                entries: {
                    let mut m = IndexMap::new();
                    m.insert("env".to_string(), "dev".to_string());
                    m
                },
            },
            ..RawProfile::default()
        });
        p.dependencies.push(dep("g", "stuff", Some("1")));

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.active_profile_ids, vec!["dev".to_string()]);
        assert_eq!(
            r.pom.properties.entries.get("env").map(String::as_str),
            Some("dev")
        );
    }

    #[test]
    fn test_15_active_by_default_suppressed_by_explicit_other() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("dev".to_string()),
            activation: Some(RawActivation {
                active_by_default: Some("true".to_string()),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        p.profiles.push(RawProfile {
            id: Some("ci".to_string()),
            ..RawProfile::default()
        });

        let mut ctx = ctx_empty();
        ctx.active_profile_ids.insert("ci".to_string());

        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert_eq!(r.active_profile_ids, vec!["ci".to_string()]);
    }

    #[test]
    fn test_16_jdk_range_matches() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("modern".to_string()),
            activation: Some(RawActivation {
                jdk: Some("[17,)".to_string()),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let mut ctx = ctx_empty();
        ctx.jdk_version = Some("21.0.4".to_string());

        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert_eq!(r.active_profile_ids, vec!["modern".to_string()]);
    }

    #[test]
    fn test_17_jdk_range_no_match() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("ancient".to_string()),
            activation: Some(RawActivation {
                jdk: Some("(,1.8]".to_string()),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let mut ctx = ctx_empty();
        ctx.jdk_version = Some("11.0.0".to_string());

        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert!(r.active_profile_ids.is_empty());
    }

    #[test]
    fn test_18_jdk_prefix_match() {
        // <jdk>1.8</jdk> matches "1.8.0_321".
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("eight".to_string()),
            activation: Some(RawActivation {
                jdk: Some("1.8".to_string()),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let mut ctx = ctx_empty();
        ctx.jdk_version = Some("1.8.0_321".to_string());

        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert_eq!(r.active_profile_ids, vec!["eight".to_string()]);
    }

    #[test]
    fn test_19_property_activation_name_only() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("when-set".to_string()),
            activation: Some(RawActivation {
                property: Some(RawActivationProperty {
                    name: Some("magic".to_string()),
                    value: None,
                }),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let mut ctx = ctx_empty();
        ctx.user_properties
            .insert("magic".to_string(), "x".to_string());

        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert_eq!(r.active_profile_ids, vec!["when-set".to_string()]);
    }

    #[test]
    fn test_20_property_activation_name_and_value() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("prod".to_string()),
            activation: Some(RawActivation {
                property: Some(RawActivationProperty {
                    name: Some("env".to_string()),
                    value: Some("prod".to_string()),
                }),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let mut ctx = ctx_empty();
        ctx.user_properties
            .insert("env".to_string(), "prod".to_string());
        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert_eq!(r.active_profile_ids, vec!["prod".to_string()]);

        let mut p2 = pom("g", "p", "1");
        p2.profiles.push(RawProfile {
            id: Some("prod".to_string()),
            activation: Some(RawActivation {
                property: Some(RawActivationProperty {
                    name: Some("env".to_string()),
                    value: Some("prod".to_string()),
                }),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let mut ctx2 = ctx_empty();
        ctx2.user_properties
            .insert("env".to_string(), "stage".to_string());
        let r2 = resolve_pom(p2, &mut f, &ctx2).expect("resolves");
        assert!(r2.active_profile_ids.is_empty());
    }

    #[test]
    fn test_21_property_activation_absent() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("ifset".to_string()),
            activation: Some(RawActivation {
                property: Some(RawActivationProperty {
                    name: Some("X".to_string()),
                    value: None,
                }),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert!(r.active_profile_ids.is_empty());
    }

    #[test]
    fn test_22_explicit_active_overrides_activation() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        // Profile has a JDK activator that would FAIL against ctx —
        // explicit -P overrides.
        p.profiles.push(RawProfile {
            id: Some("force".to_string()),
            activation: Some(RawActivation {
                jdk: Some("[99,)".to_string()),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let mut ctx = ctx_empty();
        ctx.jdk_version = Some("11.0".to_string());
        ctx.active_profile_ids.insert("force".to_string());

        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert_eq!(r.active_profile_ids, vec!["force".to_string()]);
    }

    #[test]
    fn test_23_explicit_inactive_overrides_activation() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("dev".to_string()),
            activation: Some(RawActivation {
                active_by_default: Some("true".to_string()),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let mut ctx = ctx_empty();
        ctx.inactive_profile_ids.insert("dev".to_string());

        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert!(r.active_profile_ids.is_empty());
    }

    #[test]
    fn test_24_active_profile_contributes_dependencies() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("itest".to_string()),
            activation: Some(RawActivation {
                active_by_default: Some("true".to_string()),
                ..RawActivation::default()
            }),
            dependencies: vec![dep("org.junit", "junit", Some("5.10.0"))],
            ..RawProfile::default()
        });
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.dependencies.len(), 1);
        assert_eq!(r.pom.dependencies[0].artifact_id, "junit");
    }

    #[test]
    fn test_25_multiple_active_profiles_compose() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        for id in ["a", "b"] {
            p.profiles.push(RawProfile {
                id: Some(id.to_string()),
                activation: Some(RawActivation {
                    property: Some(RawActivationProperty {
                        name: Some(format!("on_{id}")),
                        value: None,
                    }),
                    ..RawActivation::default()
                }),
                properties: crate::raw::Properties {
                    entries: {
                        let mut m = IndexMap::new();
                        m.insert(format!("k_{id}"), id.to_string());
                        m
                    },
                },
                ..RawProfile::default()
            });
        }
        let mut ctx = ctx_empty();
        ctx.user_properties.insert("on_a".into(), "1".into());
        ctx.user_properties.insert("on_b".into(), "1".into());

        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert_eq!(r.active_profile_ids.len(), 2);
        assert_eq!(
            r.pom.properties.entries.get("k_a").map(String::as_str),
            Some("a")
        );
        assert_eq!(
            r.pom.properties.entries.get("k_b").map(String::as_str),
            Some("b")
        );
    }

    #[test]
    fn test_26_os_family_activation() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        let mut os = XmlValue::default();
        os.children
            .entry("family".into())
            .or_default()
            .push(XmlValue {
                text: Some("unix".to_string()),
                ..XmlValue::default()
            });
        p.profiles.push(RawProfile {
            id: Some("nixy".to_string()),
            activation: Some(RawActivation {
                os: Some(os),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });

        let mut ctx = ctx_empty();
        ctx.os_family = Some("unix".to_string());
        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert_eq!(r.active_profile_ids, vec!["nixy".to_string()]);
    }

    #[test]
    fn test_27_property_negation() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("unless".to_string()),
            activation: Some(RawActivation {
                property: Some(RawActivationProperty {
                    name: Some("!skip".to_string()),
                    value: None,
                }),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        // skip is not set → !skip is true.
        assert_eq!(r.active_profile_ids, vec!["unless".to_string()]);
    }

    // ---- end-to-end ------------------------------------------------------

    #[test]
    fn test_28_end_to_end_parent_bom_profile_depmgt() {
        let mut f = Fixture::default();

        // Parent contributes a property used in the project's depMgt
        // somewhere indirectly via a profile-supplied property below.
        let parent = pom("org.ex", "parent", "1");
        f.add(parent);

        // BOM is self-contained: it declares its own property and
        // uses it in its own depMgt (this mirrors how real-world
        // BOMs like spring-boot-dependencies work).
        let mut bom = pom("org.ex", "bom", "1");
        bom.properties = crate::raw::Properties {
            entries: {
                let mut m = IndexMap::new();
                m.insert("foo.version".to_string(), "9.9.9".to_string());
                m
            },
        };
        bom.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("org.ex", "foo", Some("${foo.version}"))],
        });
        f.add(bom);

        let mut p = pom("g", "p", "1");
        p.parent = Some(RawParent {
            group_id: "org.ex".to_string(),
            artifact_id: "parent".to_string(),
            version: "1".to_string(),
            relative_path: None,
        });
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![import("org.ex", "bom", "1")],
        });
        p.dependencies.push(dep("org.ex", "foo", None));

        // Plus an active profile contributing a dep.
        p.profiles.push(RawProfile {
            id: Some("def".to_string()),
            activation: Some(RawActivation {
                active_by_default: Some("true".to_string()),
                ..RawActivation::default()
            }),
            dependencies: vec![dep("org.ex", "bar", Some("1.2.3"))],
            ..RawProfile::default()
        });

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.imported_boms.len(), 1);
        assert_eq!(r.active_profile_ids, vec!["def".to_string()]);
        assert_eq!(r.pom.dependencies.len(), 2);
        let foo = r
            .pom
            .dependencies
            .iter()
            .find(|d| d.artifact_id == "foo")
            .unwrap();
        assert_eq!(foo.version.as_deref(), Some("9.9.9"));
        let bar = r
            .pom
            .dependencies
            .iter()
            .find(|d| d.artifact_id == "bar")
            .unwrap();
        assert_eq!(bar.version.as_deref(), Some("1.2.3"));
    }

    // ---- additional edges ------------------------------------------------

    #[test]
    fn test_29_nested_bom_import() {
        // BOM A imports BOM B which has the real entry.
        let mut f = Fixture::default();
        let mut bom_b = pom("org.x", "b", "1");
        bom_b.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("io.y", "z", Some("7.7.7"))],
        });
        f.add(bom_b);

        let mut bom_a = pom("org.x", "a", "1");
        bom_a.dependency_management = Some(DependencyManagement {
            dependencies: vec![import("org.x", "b", "1")],
        });
        f.add(bom_a);

        let mut p = pom("g", "p", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![import("org.x", "a", "1")],
        });
        p.dependencies.push(dep("io.y", "z", None));

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.dependencies[0].version.as_deref(), Some("7.7.7"));
        assert_eq!(r.imported_boms.len(), 2);
    }

    #[test]
    fn test_30_inactive_profile_left_on_pom() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("dormant".to_string()),
            activation: Some(RawActivation {
                jdk: Some("[99,)".to_string()),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert!(r.active_profile_ids.is_empty());
        assert_eq!(r.pom.profiles.len(), 1);
        assert_eq!(r.pom.profiles[0].id.as_deref(), Some("dormant"));
    }

    #[test]
    fn test_31_depmgt_for_managed_dep_inherits_from_bom() {
        // The project declares a depMgt entry without a version, and
        // a BOM-imported entry provides the version. The managed
        // entry should then resolve any matching plain-dep.
        let mut f = Fixture::default();
        let mut bom = pom("org.x", "bom", "1");
        bom.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("io.y", "lib", Some("8.0"))],
        });
        f.add(bom);

        let mut p = pom("g", "p", "1");
        let bare_dm = dep("io.y", "lib", None);
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![bare_dm, import("org.x", "bom", "1")],
        });
        p.dependencies.push(dep("io.y", "lib", None));

        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.dependencies[0].version.as_deref(), Some("8.0"));
    }

    #[test]
    fn test_32_jdk_negation() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("not-8".to_string()),
            activation: Some(RawActivation {
                jdk: Some("!1.8".to_string()),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let mut ctx = ctx_empty();
        ctx.jdk_version = Some("21.0.0".to_string());
        let r = resolve_pom(p, &mut f, &ctx).expect("resolves");
        assert_eq!(r.active_profile_ids, vec!["not-8".to_string()]);
    }

    #[test]
    fn test_33_file_activation_no_basedir_inactive() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("filey".to_string()),
            activation: Some(RawActivation {
                file: Some(RawActivationFile {
                    exists: Some("pom.xml".to_string()),
                    missing: None,
                }),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        // No basedir set; file activator must decline.
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert!(r.active_profile_ids.is_empty());
    }

    #[test]
    fn test_34_profile_plugin_appended() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("plug".to_string()),
            activation: Some(RawActivation {
                active_by_default: Some("true".to_string()),
                ..RawActivation::default()
            }),
            build: Some(RawBuild {
                plugins: vec![RawPlugin {
                    group_id: "g.x".to_string(),
                    artifact_id: "thing".to_string(),
                    version: Some("1".to_string()),
                    ..RawPlugin::default()
                }],
                ..RawBuild::default()
            }),
            ..RawProfile::default()
        });
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        let plugins = &r.pom.build.as_ref().unwrap().plugins;
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].artifact_id, "thing");
    }

    #[test]
    fn test_35_profile_repository_id_dedup() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.repositories.push(RawRepository {
            id: Some("central".to_string()),
            url: Some("https://base".to_string()),
            ..RawRepository::default()
        });
        p.profiles.push(RawProfile {
            id: Some("rp".to_string()),
            activation: Some(RawActivation {
                active_by_default: Some("true".to_string()),
                ..RawActivation::default()
            }),
            repositories: vec![RawRepository {
                id: Some("central".to_string()),
                url: Some("https://overridden".to_string()),
                ..RawRepository::default()
            }],
            ..RawProfile::default()
        });
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.repositories.len(), 1);
        assert_eq!(
            r.pom.repositories[0].url.as_deref(),
            Some("https://overridden")
        );
    }

    #[test]
    fn test_36_profile_module_dedup() {
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.packaging = "pom".to_string();
        p.modules = vec!["core".into()];
        p.profiles.push(RawProfile {
            id: Some("xtra".to_string()),
            activation: Some(RawActivation {
                active_by_default: Some("true".to_string()),
                ..RawActivation::default()
            }),
            modules: vec!["core".into(), "extras".into()],
            ..RawProfile::default()
        });
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.modules, vec!["core", "extras"]);
    }

    #[test]
    fn test_37_default_with_no_other_profile_fires() {
        // No other profile at all, only a default.
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("only".to_string()),
            activation: Some(RawActivation {
                active_by_default: Some("true".to_string()),
                ..RawActivation::default()
            }),
            ..RawProfile::default()
        });
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.active_profile_ids, vec!["only".to_string()]);
    }

    #[test]
    fn test_38_no_activation_block_inactive() {
        // Profile with no <activation> and no -P flag never fires.
        let mut f = Fixture::default();
        let mut p = pom("g", "p", "1");
        p.profiles.push(RawProfile {
            id: Some("dormant".to_string()),
            activation: None,
            ..RawProfile::default()
        });
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert!(r.active_profile_ids.is_empty());
    }

    #[test]
    fn test_39_versioned_depmgt_not_overwritten_by_bom() {
        // Project's depMgt has a concrete version for foo; BOM tries
        // to override it. First-wins → project's wins.
        let mut f = Fixture::default();
        let mut bom = pom("org.x", "bom", "1");
        bom.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("io.y", "foo", Some("BOM-VER"))],
        });
        f.add(bom);

        let mut p = pom("g", "p", "1");
        p.dependency_management = Some(DependencyManagement {
            dependencies: vec![
                dep("io.y", "foo", Some("PROJ-VER")),
                import("org.x", "bom", "1"),
            ],
        });
        p.dependencies.push(dep("io.y", "foo", None));
        let r = resolve_pom(p, &mut f, &ctx_empty()).expect("resolves");
        assert_eq!(r.pom.dependencies[0].version.as_deref(), Some("PROJ-VER"));
    }
}
