//! Parent-chain merge + property interpolation.
//!
//! Given a [`RawPom`] and a [`ParentResolver`], produces an
//! [`EffectivePom`] in which every inherited field has been merged
//! down the parent chain and every `${...}` placeholder has been
//! substituted.
//!
//! This module deliberately does NOT yet:
//!
//! - Apply `<dependencyManagement>` defaults to dependency versions.
//! - Import BOM POMs (scope=import).
//! - Activate profiles.
//!
//! Those passes layer on top in a sibling module.
//!
//! ## Algorithm
//!
//! 1. Walk the chain of `<parent>` declarations from the input POM
//!    upward, collecting each ancestor into `parent_chain`. Bail with
//!    [`EffectiveError::ChainTooDeep`] past depth 10 (Maven's
//!    default) and [`EffectiveError::CircularParent`] on cycles.
//! 2. Merge the chain top-down: start with the root ancestor, fold in
//!    each child according to per-field rules (see [`merge`] below),
//!    ending with the input POM.
//! 3. Walk every string-valued field in the merged POM and
//!    recursively substitute `${...}` placeholders, capped at
//!    [`MAX_INTERPOLATION_DEPTH`].
//!
//! ## Merge rules (subset for v0.1)
//!
//! | Field              | Rule                                          |
//! |--------------------|-----------------------------------------------|
//! | `model_version`    | child wins (required on every POM)            |
//! | `group_id`         | child wins; inherit from parent if `None`     |
//! | `artifact_id`      | child wins (must be present on child)         |
//! | `version`          | child wins; inherit from parent if `None`     |
//! | `packaging`        | child wins; inherit if child default `"jar"`  |
//! | `name`/`desc`/`url`| child wins; inherit if child `None`           |
//! | `properties`       | union; child wins on key collision            |
//! | `dependencies`     | append (parent first, then child)             |
//! | `dependency_management.dependencies` | append (parent first)       |
//! | `build.plugins`    | append (parent first); honors `<inherited>`   |
//! | `modules`          | dropped (project-local)                       |
//! | `profiles`         | append; activation deferred to Task 3         |
//! | `repositories`     | append with id-based dedup (child wins)       |
//! | `plugin_repositories` | append with id-based dedup (child wins)    |
//! | `parent`           | dropped (already resolved)                    |

use std::collections::HashSet;

use indexmap::IndexMap;

use crate::raw::{
    DependencyManagement, Properties, RawBuild, RawDependency, RawParent, RawPlugin,
    RawPluginExecution, RawPluginManagement, RawPom, RawProfile, RawRepository, XmlValue,
};

/// Maximum number of ancestors that will be merged before we declare
/// the chain pathological. Mirrors Maven's own default.
pub const MAX_CHAIN_DEPTH: usize = 10;

/// Maximum number of recursive interpolation passes over a single
/// string before we conclude the placeholder web is circular.
pub const MAX_INTERPOLATION_DEPTH: usize = 10;

// ---------------------------------------------------------------------------
// Public data model
// ---------------------------------------------------------------------------

/// Result of applying parent-chain merge + property interpolation.
#[derive(Debug, Clone)]
pub struct EffectivePom {
    /// The fully merged, fully interpolated POM. Most consumers only
    /// need this field.
    pub pom: RawPom,
    /// Audit trail of every `${...}` substitution that fired. Useful
    /// for debugging surprising interpolation results.
    pub interpolations: Vec<Interpolation>,
    /// The chain of parents that were merged in, ordered from nearest
    /// (direct parent) to root. Empty if the input POM had no
    /// `<parent>` declaration.
    pub parent_chain: Vec<RawPom>,
}

/// A single `${...}` substitution recorded during interpolation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interpolation {
    /// The placeholder text as it appeared in the source, including
    /// the `${...}` delimiters.
    pub placeholder: String,
    /// The value the placeholder resolved to.
    pub resolved_to: String,
    /// Which field the placeholder appeared in.
    pub location: InterpolationLocation,
}

/// Where in the POM an [`Interpolation`] occurred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpolationLocation {
    /// In a `<properties>` value (the key is the property name).
    Property(String),
    /// In a `<dependency>`'s `<version>`.
    DependencyVersion { group: String, artifact: String },
    /// In a `<plugin>`'s `<version>`.
    PluginVersion { group: String, artifact: String },
    /// In a top-level scalar like `<version>` or `<name>`.
    PomField(&'static str),
    /// In a free-form `<configuration>` text node.
    PluginConfiguration { group: String, artifact: String },
    /// In a `<repositories>`/`<pluginRepositories>` URL.
    RepositoryUrl(String),
    /// Anything else; the string is a human-readable hint.
    Other(String),
}

/// Resolves a `<parent>` declaration to its POM. Production
/// implementations consult the local cache + Maven Central; test
/// implementations typically use a hardcoded map.
pub trait ParentResolver {
    /// Resolve a single parent declaration. Returning `Err(...)` is
    /// propagated as [`EffectiveError::ParentResolution`].
    fn resolve(&mut self, parent: &RawParent) -> Result<RawPom, String>;
}

/// Errors produced when constructing an effective POM.
#[derive(Debug, thiserror::Error)]
pub enum EffectiveError {
    /// The resolver returned an error for a `<parent>` declaration.
    #[error("parent {coords} could not be resolved: {reason}")]
    ParentResolution { coords: String, reason: String },
    /// The parent chain exceeded [`MAX_CHAIN_DEPTH`].
    #[error("parent chain exceeds maximum depth ({max})")]
    ChainTooDeep { max: usize },
    /// A POM in the chain was already seen at a higher level.
    #[error("circular parent reference detected at {coords}")]
    CircularParent { coords: String },
    /// A `${...}` placeholder could not be resolved (recursion limit
    /// hit, unknown property, or unsupported domain).
    #[error("unresolved placeholder {placeholder:?} in {location}")]
    UnresolvedPlaceholder {
        placeholder: String,
        location: String,
    },
}

/// Apply parent-chain merge + property interpolation to `root`.
pub fn build_effective<R: ParentResolver>(
    root: RawPom,
    resolver: &mut R,
) -> Result<EffectivePom, EffectiveError> {
    let parent_chain = collect_parent_chain(&root, resolver)?;

    // Merge top-down: start with the most distant ancestor, fold each
    // closer ancestor in, then finally fold in the input POM itself.
    let merged = {
        let mut iter = parent_chain.iter().rev().cloned();
        let mut acc: RawPom = iter.next().unwrap_or_default();
        for next in iter {
            acc = merge(acc, next);
        }
        if parent_chain.is_empty() {
            // No parents: the root POM is the entire merged content.
            root.clone()
        } else {
            merge(acc, root.clone())
        }
    };

    let mut effective = EffectivePom {
        pom: merged,
        interpolations: Vec::new(),
        parent_chain,
    };

    interpolate_pom(&mut effective)?;

    // After interpolation, drop the resolved `<parent>` element from
    // the merged POM — it has served its purpose.
    effective.pom.parent = None;

    Ok(effective)
}

// ---------------------------------------------------------------------------
// Parent-chain walk
// ---------------------------------------------------------------------------

fn collect_parent_chain<R: ParentResolver>(
    root: &RawPom,
    resolver: &mut R,
) -> Result<Vec<RawPom>, EffectiveError> {
    let mut chain = Vec::new();
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let root_coords = pom_coords(root);
    seen.insert(root_coords);

    let mut current = root.parent.clone();
    while let Some(p) = current {
        if chain.len() >= MAX_CHAIN_DEPTH {
            return Err(EffectiveError::ChainTooDeep {
                max: MAX_CHAIN_DEPTH,
            });
        }
        let coords = (p.group_id.clone(), p.artifact_id.clone(), p.version.clone());
        if !seen.insert(coords.clone()) {
            return Err(EffectiveError::CircularParent {
                coords: format!("{}:{}:{}", coords.0, coords.1, coords.2),
            });
        }
        let parent_pom =
            resolver
                .resolve(&p)
                .map_err(|reason| EffectiveError::ParentResolution {
                    coords: format!("{}:{}:{}", p.group_id, p.artifact_id, p.version),
                    reason,
                })?;
        current = parent_pom.parent.clone();
        chain.push(parent_pom);
    }
    Ok(chain)
}

fn pom_coords(p: &RawPom) -> (String, String, String) {
    let g = p
        .group_id
        .clone()
        .or_else(|| p.parent.as_ref().map(|pp| pp.group_id.clone()))
        .unwrap_or_default();
    let v = p
        .version
        .clone()
        .or_else(|| p.parent.as_ref().map(|pp| pp.version.clone()))
        .unwrap_or_default();
    (g, p.artifact_id.clone(), v)
}

// ---------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------

/// Fold `child` onto `parent`, returning the merged POM. `child`'s
/// fields take precedence where the rule is "child wins". This
/// matches Maven's `MavenModelMerger` for the v0.1 field subset.
fn merge(parent: RawPom, child: RawPom) -> RawPom {
    let mut out = child;

    // Scalars: inherit from parent when child is absent / default.
    if out.group_id.is_none() {
        out.group_id = parent.group_id.clone();
    }
    if out.version.is_none() {
        out.version = parent.version.clone();
    }
    if out.packaging == "jar" && parent.packaging != "jar" {
        out.packaging = parent.packaging.clone();
    }
    if out.name.is_none() {
        out.name = parent.name.clone();
    }
    if out.description.is_none() {
        out.description = parent.description.clone();
    }
    if out.url.is_none() {
        out.url = parent.url.clone();
    }
    if out.inception_year.is_none() {
        out.inception_year = parent.inception_year.clone();
    }

    // Properties: union, child wins on collision. Preserve parent
    // ordering for parent-only keys, then append child keys in their
    // own order.
    out.properties = merge_properties(&parent.properties, &out.properties);

    // Dependencies: parent's first, then child's. Phase-2 resolver
    // does deduplication; here we just concatenate.
    let mut deps = parent.dependencies.clone();
    deps.extend(std::mem::take(&mut out.dependencies));
    out.dependencies = deps;

    // dependencyManagement: append; child wins is enforced at
    // application time (Task 3).
    out.dependency_management = merge_dependency_management(
        parent.dependency_management.clone(),
        out.dependency_management.take(),
    );

    // build: per-field merge, honoring <inherited>false on plugins.
    out.build = merge_build(parent.build.clone(), out.build.take());

    // profiles: append. Activation lives in Task 3.
    let mut profiles = parent.profiles.clone();
    profiles.extend(std::mem::take(&mut out.profiles));
    out.profiles = profiles;

    // repositories / pluginRepositories: append with id-based dedup
    // (child wins on id collision).
    out.repositories = merge_repositories(&parent.repositories, &out.repositories);
    out.plugin_repositories =
        merge_repositories(&parent.plugin_repositories, &out.plugin_repositories);

    // modules and parent are project-local; never inherited.
    out
}

fn merge_properties(parent: &Properties, child: &Properties) -> Properties {
    let mut entries: IndexMap<String, String> = IndexMap::new();
    for (k, v) in &parent.entries {
        entries.insert(k.clone(), v.clone());
    }
    for (k, v) in &child.entries {
        entries.insert(k.clone(), v.clone());
    }
    Properties { entries }
}

fn merge_dependency_management(
    parent: Option<DependencyManagement>,
    child: Option<DependencyManagement>,
) -> Option<DependencyManagement> {
    match (parent, child) {
        (None, None) => None,
        (Some(p), None) => Some(p),
        (None, Some(c)) => Some(c),
        (Some(p), Some(c)) => {
            let mut deps = p.dependencies;
            deps.extend(c.dependencies);
            Some(DependencyManagement { dependencies: deps })
        }
    }
}

fn merge_build(parent: Option<RawBuild>, child: Option<RawBuild>) -> Option<RawBuild> {
    match (parent, child) {
        (None, None) => None,
        (Some(p), None) => {
            // Inherit parent build, but strip non-inheritable plugins.
            let mut p = p;
            p.plugins.retain(is_plugin_inheritable);
            if let Some(pm) = p.plugin_management.as_mut() {
                pm.plugins.retain(is_plugin_inheritable);
            }
            Some(p)
        }
        (None, Some(c)) => Some(c),
        (Some(p), Some(mut c)) => {
            // Per-scalar: child wins; inherit from parent if None.
            if c.source_directory.is_none() {
                c.source_directory = p.source_directory;
            }
            if c.script_source_directory.is_none() {
                c.script_source_directory = p.script_source_directory;
            }
            if c.test_source_directory.is_none() {
                c.test_source_directory = p.test_source_directory;
            }
            if c.output_directory.is_none() {
                c.output_directory = p.output_directory;
            }
            if c.test_output_directory.is_none() {
                c.test_output_directory = p.test_output_directory;
            }
            if c.final_name.is_none() {
                c.final_name = p.final_name;
            }
            if c.default_goal.is_none() {
                c.default_goal = p.default_goal;
            }
            if c.directory.is_none() {
                c.directory = p.directory;
            }

            // filters / resources / testResources: append.
            let mut filters = p.filters;
            filters.extend(c.filters);
            c.filters = filters;

            let mut resources = p.resources;
            resources.extend(c.resources);
            c.resources = resources;

            let mut test_resources = p.test_resources;
            test_resources.extend(c.test_resources);
            c.test_resources = test_resources;

            // plugins: append, parent's first, dropping <inherited>false
            // entries before they reach the child.
            let mut plugins: Vec<RawPlugin> = p
                .plugins
                .into_iter()
                .filter(is_plugin_inheritable)
                .collect();
            plugins.extend(c.plugins);
            c.plugins = plugins;

            // pluginManagement: similar.
            c.plugin_management = merge_plugin_management(p.plugin_management, c.plugin_management);

            Some(c)
        }
    }
}

fn merge_plugin_management(
    parent: Option<RawPluginManagement>,
    child: Option<RawPluginManagement>,
) -> Option<RawPluginManagement> {
    match (parent, child) {
        (None, None) => None,
        (Some(mut p), None) => {
            p.plugins.retain(is_plugin_inheritable);
            Some(p)
        }
        (None, Some(c)) => Some(c),
        (Some(p), Some(mut c)) => {
            let mut plugins: Vec<RawPlugin> = p
                .plugins
                .into_iter()
                .filter(is_plugin_inheritable)
                .collect();
            plugins.extend(std::mem::take(&mut c.plugins));
            c.plugins = plugins;
            Some(c)
        }
    }
}

fn is_plugin_inheritable(p: &RawPlugin) -> bool {
    !matches!(p.inherited.as_deref(), Some("false"))
}

fn merge_repositories(parent: &[RawRepository], child: &[RawRepository]) -> Vec<RawRepository> {
    let mut out: Vec<RawRepository> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    // Child wins on id collision, so collect child ids first.
    let child_ids: HashSet<String> = child.iter().filter_map(|r| r.id.clone()).collect();

    for r in parent {
        if let Some(id) = r.id.as_ref() {
            if child_ids.contains(id) {
                continue; // overridden by child
            }
            seen_ids.insert(id.clone());
        }
        out.push(r.clone());
    }
    for r in child {
        if let Some(id) = r.id.as_ref() {
            if !seen_ids.insert(id.clone()) {
                // duplicate id within child itself: keep first
                continue;
            }
        }
        out.push(r.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Interpolation
// ---------------------------------------------------------------------------

/// Snapshot of the POM fields required to resolve `${project.*}`
/// references. Built once before interpolation begins so callers can
/// reason about `${project.X}` independently of the mutation in
/// progress.
#[derive(Debug, Clone)]
struct ProjectContext {
    group_id: Option<String>,
    artifact_id: String,
    version: Option<String>,
    parent_group_id: Option<String>,
    parent_artifact_id: Option<String>,
    parent_version: Option<String>,
}

impl ProjectContext {
    fn from_pom(pom: &RawPom) -> Self {
        let parent = pom.parent.as_ref();
        Self {
            group_id: pom
                .group_id
                .clone()
                .or_else(|| parent.map(|p| p.group_id.clone())),
            artifact_id: pom.artifact_id.clone(),
            version: pom
                .version
                .clone()
                .or_else(|| parent.map(|p| p.version.clone())),
            parent_group_id: parent.map(|p| p.group_id.clone()),
            parent_artifact_id: parent.map(|p| p.artifact_id.clone()),
            parent_version: parent.map(|p| p.version.clone()),
        }
    }
}

fn interpolate_pom(eff: &mut EffectivePom) -> Result<(), EffectiveError> {
    let ctx = ProjectContext::from_pom(&eff.pom);
    let props = eff.pom.properties.entries.clone();

    let resolver = Resolver {
        ctx: &ctx,
        props: &props,
    };

    // Properties: interpolate every value. We keep iterating the
    // snapshot map (already cloned above) so property-to-property
    // references resolve via the recursive substitution path inside
    // `interp`.
    {
        let mut new_props: IndexMap<String, String> = IndexMap::with_capacity(props.len());
        for (k, v) in &eff.pom.properties.entries {
            let loc = InterpolationLocation::Property(k.clone());
            let v2 = interp(v, &resolver, &loc, &mut eff.interpolations)?;
            new_props.insert(k.clone(), v2);
        }
        eff.pom.properties.entries = new_props;
    }

    // Top-level scalars.
    interp_opt(
        &mut eff.pom.group_id,
        &resolver,
        InterpolationLocation::PomField("groupId"),
        &mut eff.interpolations,
    )?;
    interp_opt(
        &mut eff.pom.version,
        &resolver,
        InterpolationLocation::PomField("version"),
        &mut eff.interpolations,
    )?;
    interp_opt(
        &mut eff.pom.name,
        &resolver,
        InterpolationLocation::PomField("name"),
        &mut eff.interpolations,
    )?;
    interp_opt(
        &mut eff.pom.description,
        &resolver,
        InterpolationLocation::PomField("description"),
        &mut eff.interpolations,
    )?;
    interp_opt(
        &mut eff.pom.url,
        &resolver,
        InterpolationLocation::PomField("url"),
        &mut eff.interpolations,
    )?;
    interp_opt(
        &mut eff.pom.inception_year,
        &resolver,
        InterpolationLocation::PomField("inceptionYear"),
        &mut eff.interpolations,
    )?;

    // Dependencies (both regular and managed).
    for d in &mut eff.pom.dependencies {
        interp_dependency(d, &resolver, &mut eff.interpolations)?;
    }
    if let Some(dm) = eff.pom.dependency_management.as_mut() {
        for d in &mut dm.dependencies {
            interp_dependency(d, &resolver, &mut eff.interpolations)?;
        }
    }

    // Build (plugins, resources, scalars).
    if let Some(b) = eff.pom.build.as_mut() {
        interp_build(b, &resolver, &mut eff.interpolations)?;
    }

    // Profiles: interpolate the scalar bits in the profile body. We
    // do NOT activate them here — that's Task 3.
    for prof in &mut eff.pom.profiles {
        interp_profile(prof, &resolver, &mut eff.interpolations)?;
    }

    // Repositories.
    for r in &mut eff.pom.repositories {
        interp_repository(r, &resolver, &mut eff.interpolations)?;
    }
    for r in &mut eff.pom.plugin_repositories {
        interp_repository(r, &resolver, &mut eff.interpolations)?;
    }

    Ok(())
}

fn interp_dependency(
    d: &mut RawDependency,
    resolver: &Resolver<'_>,
    log: &mut Vec<Interpolation>,
) -> Result<(), EffectiveError> {
    let loc = InterpolationLocation::DependencyVersion {
        group: d.group_id.clone(),
        artifact: d.artifact_id.clone(),
    };
    d.group_id = interp(&d.group_id, resolver, &loc, log)?;
    d.artifact_id = interp(&d.artifact_id, resolver, &loc, log)?;
    interp_opt(&mut d.version, resolver, loc.clone(), log)?;
    interp_opt(&mut d.scope, resolver, loc.clone(), log)?;
    interp_opt(&mut d.classifier, resolver, loc.clone(), log)?;
    interp_opt(&mut d.r#type, resolver, loc.clone(), log)?;
    interp_opt(&mut d.system_path, resolver, loc.clone(), log)?;
    interp_opt(&mut d.optional, resolver, loc.clone(), log)?;
    for x in &mut d.exclusions {
        x.group_id = interp(&x.group_id, resolver, &loc, log)?;
        x.artifact_id = interp(&x.artifact_id, resolver, &loc, log)?;
    }
    Ok(())
}

fn interp_build(
    b: &mut RawBuild,
    resolver: &Resolver<'_>,
    log: &mut Vec<Interpolation>,
) -> Result<(), EffectiveError> {
    let loc = InterpolationLocation::Other("build".to_string());
    interp_opt(&mut b.source_directory, resolver, loc.clone(), log)?;
    interp_opt(&mut b.script_source_directory, resolver, loc.clone(), log)?;
    interp_opt(&mut b.test_source_directory, resolver, loc.clone(), log)?;
    interp_opt(&mut b.output_directory, resolver, loc.clone(), log)?;
    interp_opt(&mut b.test_output_directory, resolver, loc.clone(), log)?;
    interp_opt(&mut b.final_name, resolver, loc.clone(), log)?;
    interp_opt(&mut b.default_goal, resolver, loc.clone(), log)?;
    interp_opt(&mut b.directory, resolver, loc.clone(), log)?;

    for p in &mut b.plugins {
        interp_plugin(p, resolver, log)?;
    }
    if let Some(pm) = b.plugin_management.as_mut() {
        for p in &mut pm.plugins {
            interp_plugin(p, resolver, log)?;
        }
    }
    Ok(())
}

fn interp_plugin(
    p: &mut RawPlugin,
    resolver: &Resolver<'_>,
    log: &mut Vec<Interpolation>,
) -> Result<(), EffectiveError> {
    let loc = InterpolationLocation::PluginVersion {
        group: p.group_id.clone(),
        artifact: p.artifact_id.clone(),
    };
    p.group_id = interp(&p.group_id, resolver, &loc, log)?;
    p.artifact_id = interp(&p.artifact_id, resolver, &loc, log)?;
    interp_opt(&mut p.version, resolver, loc.clone(), log)?;

    let cfg_loc = InterpolationLocation::PluginConfiguration {
        group: p.group_id.clone(),
        artifact: p.artifact_id.clone(),
    };
    if let Some(cfg) = p.configuration.as_mut() {
        interp_xml_value(cfg, resolver, &cfg_loc, log)?;
    }
    for d in &mut p.dependencies {
        interp_dependency(d, resolver, log)?;
    }
    for ex in &mut p.executions {
        interp_plugin_execution(ex, resolver, &cfg_loc, log)?;
    }
    Ok(())
}

fn interp_plugin_execution(
    e: &mut RawPluginExecution,
    resolver: &Resolver<'_>,
    cfg_loc: &InterpolationLocation,
    log: &mut Vec<Interpolation>,
) -> Result<(), EffectiveError> {
    interp_opt(&mut e.id, resolver, cfg_loc.clone(), log)?;
    interp_opt(&mut e.phase, resolver, cfg_loc.clone(), log)?;
    for g in &mut e.goals {
        *g = interp(g, resolver, cfg_loc, log)?;
    }
    if let Some(cfg) = e.configuration.as_mut() {
        interp_xml_value(cfg, resolver, cfg_loc, log)?;
    }
    Ok(())
}

fn interp_profile(
    pr: &mut RawProfile,
    resolver: &Resolver<'_>,
    log: &mut Vec<Interpolation>,
) -> Result<(), EffectiveError> {
    let loc = InterpolationLocation::Other(format!("profile[{}]", pr.id.as_deref().unwrap_or("?")));
    interp_opt(&mut pr.id, resolver, loc.clone(), log)?;
    for d in &mut pr.dependencies {
        interp_dependency(d, resolver, log)?;
    }
    if let Some(dm) = pr.dependency_management.as_mut() {
        for d in &mut dm.dependencies {
            interp_dependency(d, resolver, log)?;
        }
    }
    if let Some(b) = pr.build.as_mut() {
        interp_build(b, resolver, log)?;
    }
    for r in &mut pr.repositories {
        interp_repository(r, resolver, log)?;
    }
    for r in &mut pr.plugin_repositories {
        interp_repository(r, resolver, log)?;
    }
    Ok(())
}

fn interp_repository(
    r: &mut RawRepository,
    resolver: &Resolver<'_>,
    log: &mut Vec<Interpolation>,
) -> Result<(), EffectiveError> {
    let loc = InterpolationLocation::RepositoryUrl(r.id.clone().unwrap_or_else(|| "?".to_string()));
    interp_opt(&mut r.id, resolver, loc.clone(), log)?;
    interp_opt(&mut r.name, resolver, loc.clone(), log)?;
    interp_opt(&mut r.url, resolver, loc.clone(), log)?;
    interp_opt(&mut r.layout, resolver, loc.clone(), log)?;
    Ok(())
}

fn interp_xml_value(
    v: &mut XmlValue,
    resolver: &Resolver<'_>,
    loc: &InterpolationLocation,
    log: &mut Vec<Interpolation>,
) -> Result<(), EffectiveError> {
    if let Some(t) = v.text.as_mut() {
        *t = interp(t, resolver, loc, log)?;
    }
    for (_k, va) in v.attributes.iter_mut() {
        *va = interp(va, resolver, loc, log)?;
    }
    for children in v.children.values_mut() {
        for child in children.iter_mut() {
            interp_xml_value(child, resolver, loc, log)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// String-level interpolation
// ---------------------------------------------------------------------------

struct Resolver<'a> {
    ctx: &'a ProjectContext,
    props: &'a IndexMap<String, String>,
}

impl Resolver<'_> {
    fn lookup(&self, key: &str) -> Option<String> {
        // ${project.*}
        if let Some(rest) = key.strip_prefix("project.") {
            return self.lookup_project(rest);
        }
        // ${env.X}
        if let Some(name) = key.strip_prefix("env.") {
            return std::env::var(name).ok();
        }
        // ${settings.X} is explicitly NOT handled in Task 2; the
        // caller decides via UnresolvedPlaceholder.
        if key.starts_with("settings.") {
            return None;
        }
        // Bare property reference.
        self.props.get(key).cloned()
    }

    fn lookup_project(&self, rest: &str) -> Option<String> {
        match rest {
            "groupId" => self.ctx.group_id.clone(),
            "artifactId" => Some(self.ctx.artifact_id.clone()),
            "version" => self.ctx.version.clone(),
            "parent.groupId" => self.ctx.parent_group_id.clone(),
            "parent.artifactId" => self.ctx.parent_artifact_id.clone(),
            "parent.version" => self.ctx.parent_version.clone(),
            // basedir is filesystem-dependent and not relevant to a
            // parse-only pipeline; resolve to empty so consumers
            // don't trip on UnresolvedPlaceholder.
            "basedir" => Some(String::new()),
            _ => None,
        }
    }
}

fn interp(
    input: &str,
    resolver: &Resolver<'_>,
    loc: &InterpolationLocation,
    log: &mut Vec<Interpolation>,
) -> Result<String, EffectiveError> {
    if !input.contains("${") {
        return Ok(input.to_string());
    }
    let mut current = input.to_string();
    for _ in 0..MAX_INTERPOLATION_DEPTH {
        let (next, fired) = pass(&current, resolver, loc, log)?;
        if !fired {
            return Ok(next);
        }
        if !next.contains("${") {
            return Ok(next);
        }
        if next == current {
            // No progress and still contains a placeholder — the only
            // way this happens is a self-referencing chain like
            // ${a}=${a}. Treat as unresolved.
            return Err(EffectiveError::UnresolvedPlaceholder {
                placeholder: first_placeholder(&next).unwrap_or_else(|| next.clone()),
                location: format!("{:?}", loc),
            });
        }
        current = next;
    }
    Err(EffectiveError::UnresolvedPlaceholder {
        placeholder: first_placeholder(&current).unwrap_or(current),
        location: format!("{:?}", loc),
    })
}

fn interp_opt(
    target: &mut Option<String>,
    resolver: &Resolver<'_>,
    loc: InterpolationLocation,
    log: &mut Vec<Interpolation>,
) -> Result<(), EffectiveError> {
    if let Some(s) = target.as_mut() {
        *s = interp(s, resolver, &loc, log)?;
    }
    Ok(())
}

/// Perform a single pass of substitution. Returns `(new_string,
/// any_fired)`. Unresolved placeholders are an error.
fn pass(
    input: &str,
    resolver: &Resolver<'_>,
    loc: &InterpolationLocation,
    log: &mut Vec<Interpolation>,
) -> Result<(String, bool), EffectiveError> {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut fired = false;

    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            // Find the matching '}'. We do not support nested
            // placeholders within a single pass — multi-pass
            // recursion handles `${${x}}` cases.
            if let Some(end_rel) = input[i + 2..].find('}') {
                let end = i + 2 + end_rel;
                let key = &input[i + 2..end];
                let placeholder = &input[i..=end];

                match resolver.lookup(key) {
                    Some(v) => {
                        log.push(Interpolation {
                            placeholder: placeholder.to_string(),
                            resolved_to: v.clone(),
                            location: loc.clone(),
                        });
                        out.push_str(&v);
                        fired = true;
                        i = end + 1;
                        continue;
                    }
                    None => {
                        return Err(EffectiveError::UnresolvedPlaceholder {
                            placeholder: placeholder.to_string(),
                            location: format!("{:?}", loc),
                        });
                    }
                }
            }
            // Unterminated '${' — copy literally.
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok((out, fired))
}

fn first_placeholder(s: &str) -> Option<String> {
    let start = s.find("${")?;
    let end = s[start + 2..].find('}')?;
    Some(s[start..=start + 2 + end].to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{
        DependencyManagement, Properties, RawBuild, RawDependency, RawExclusion, RawParent,
        RawPlugin, RawPluginManagement, RawPom, RawProfile, RawRepository, XmlValue,
    };
    use std::collections::HashMap;

    // -----------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct TestResolver {
        poms: HashMap<(String, String, String), RawPom>,
        force_error: HashMap<(String, String, String), String>,
    }

    impl TestResolver {
        fn add(&mut self, pom: RawPom) {
            let key = (
                pom.group_id.clone().unwrap_or_default(),
                pom.artifact_id.clone(),
                pom.version.clone().unwrap_or_default(),
            );
            self.poms.insert(key, pom);
        }
    }

    impl ParentResolver for TestResolver {
        fn resolve(&mut self, parent: &RawParent) -> Result<RawPom, String> {
            let key = (
                parent.group_id.clone(),
                parent.artifact_id.clone(),
                parent.version.clone(),
            );
            if let Some(reason) = self.force_error.get(&key) {
                return Err(reason.clone());
            }
            self.poms
                .get(&key)
                .cloned()
                .ok_or_else(|| format!("not in test fixture: {}:{}:{}", key.0, key.1, key.2))
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

    fn parent_ref(group: &str, artifact: &str, version: &str) -> RawParent {
        RawParent {
            group_id: group.to_string(),
            artifact_id: artifact.to_string(),
            version: version.to_string(),
            relative_path: None,
        }
    }

    fn props(entries: &[(&str, &str)]) -> Properties {
        let mut map = IndexMap::new();
        for (k, v) in entries {
            map.insert((*k).to_string(), (*v).to_string());
        }
        Properties { entries: map }
    }

    fn dep(group: &str, artifact: &str, version: Option<&str>) -> RawDependency {
        RawDependency {
            group_id: group.to_string(),
            artifact_id: artifact.to_string(),
            version: version.map(str::to_string),
            ..RawDependency::default()
        }
    }

    // -----------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------

    #[test]
    fn test_01_no_parent_returns_input_unchanged() {
        let mut r = TestResolver::default();
        let input = pom("g", "a", "1.0");
        let eff = build_effective(input.clone(), &mut r).expect("ok");
        assert!(eff.parent_chain.is_empty());
        assert_eq!(eff.pom.artifact_id, "a");
        assert_eq!(eff.pom.group_id.as_deref(), Some("g"));
        assert_eq!(eff.pom.version.as_deref(), Some("1.0"));
        assert!(eff.interpolations.is_empty());
    }

    #[test]
    fn test_02_single_parent_inherits_version() {
        let mut r = TestResolver::default();
        r.add(pom("p.g", "parent", "5.0"));

        let mut child = RawPom {
            model_version: "4.0.0".to_string(),
            artifact_id: "child".to_string(),
            packaging: "jar".to_string(),
            parent: Some(parent_ref("p.g", "parent", "5.0")),
            ..RawPom::default()
        };
        child.group_id = None;
        child.version = None;

        let eff = build_effective(child, &mut r).expect("ok");
        assert_eq!(eff.parent_chain.len(), 1);
        assert_eq!(eff.pom.group_id.as_deref(), Some("p.g"));
        assert_eq!(eff.pom.version.as_deref(), Some("5.0"));
        assert_eq!(eff.pom.artifact_id, "child");
    }

    #[test]
    fn test_03_two_level_chain_propagates() {
        let mut r = TestResolver::default();
        let mut grandparent = pom("g.gp", "gp", "1.0");
        grandparent.url = Some("https://gp.example".to_string());

        let mut parent = pom("g.p", "p", "2.0");
        parent.parent = Some(parent_ref("g.gp", "gp", "1.0"));
        parent.description = Some("from-parent".to_string());

        r.add(grandparent);
        r.add(parent);

        let child = RawPom {
            model_version: "4.0.0".to_string(),
            artifact_id: "c".to_string(),
            packaging: "jar".to_string(),
            parent: Some(parent_ref("g.p", "p", "2.0")),
            ..RawPom::default()
        };

        let eff = build_effective(child, &mut r).expect("ok");
        assert_eq!(eff.parent_chain.len(), 2);
        assert_eq!(eff.pom.url.as_deref(), Some("https://gp.example"));
        assert_eq!(eff.pom.description.as_deref(), Some("from-parent"));
    }

    #[test]
    fn test_04_properties_merge_child_wins() {
        let mut r = TestResolver::default();
        let mut parent = pom("g", "p", "1.0");
        parent.properties = props(&[("foo", "a"), ("bar", "b")]);
        r.add(parent);

        let mut child = pom("g", "c", "1.0");
        child.parent = Some(parent_ref("g", "p", "1.0"));
        child.properties = props(&[("bar", "c"), ("baz", "d")]);

        let eff = build_effective(child, &mut r).expect("ok");
        let p = &eff.pom.properties.entries;
        assert_eq!(p.get("foo").map(String::as_str), Some("a"));
        assert_eq!(p.get("bar").map(String::as_str), Some("c"));
        assert_eq!(p.get("baz").map(String::as_str), Some("d"));
    }

    #[test]
    fn test_05_interp_simple_property_reference() {
        let mut r = TestResolver::default();
        let mut input = pom("g", "a", "${proj.ver}");
        input.properties = props(&[("proj.ver", "1.0")]);
        let eff = build_effective(input, &mut r).expect("ok");
        assert_eq!(eff.pom.version.as_deref(), Some("1.0"));
        assert!(
            eff.interpolations
                .iter()
                .any(|i| i.placeholder == "${proj.ver}" && i.resolved_to == "1.0")
        );
    }

    #[test]
    fn test_06_interp_project_references() {
        let mut r = TestResolver::default();
        let mut input = pom("com.ex", "thing", "9");
        input.name = Some("${project.groupId}:${project.artifactId}".to_string());
        let eff = build_effective(input, &mut r).expect("ok");
        assert_eq!(eff.pom.name.as_deref(), Some("com.ex:thing"));
    }

    #[test]
    fn test_07_interp_recursive_resolution() {
        let mut r = TestResolver::default();
        let mut input = pom("g", "a", "${a}");
        input.properties = props(&[("a", "${b}"), ("b", "foo")]);
        let eff = build_effective(input, &mut r).expect("ok");
        assert_eq!(eff.pom.version.as_deref(), Some("foo"));
    }

    #[test]
    fn test_08_interp_circular_errors() {
        let mut r = TestResolver::default();
        let mut input = pom("g", "a", "${a}");
        input.properties = props(&[("a", "${b}"), ("b", "${a}")]);
        let err = build_effective(input, &mut r).unwrap_err();
        assert!(matches!(err, EffectiveError::UnresolvedPlaceholder { .. }));
    }

    #[test]
    fn test_09_interp_unresolved_errors() {
        let mut r = TestResolver::default();
        let mut input = pom("g", "a", "1.0");
        input.name = Some("${nope}".to_string());
        let err = build_effective(input, &mut r).unwrap_err();
        match err {
            EffectiveError::UnresolvedPlaceholder { placeholder, .. } => {
                assert_eq!(placeholder, "${nope}");
            }
            other => panic!("expected UnresolvedPlaceholder, got {:?}", other),
        }
    }

    #[test]
    fn test_10_interp_in_dependency_version() {
        let mut r = TestResolver::default();
        let mut input = pom("g", "a", "1.0");
        input.properties = props(&[("spring.version", "6.1.0")]);
        input.dependencies.push(dep(
            "org.springframework",
            "spring-core",
            Some("${spring.version}"),
        ));
        let eff = build_effective(input, &mut r).expect("ok");
        assert_eq!(eff.pom.dependencies[0].version.as_deref(), Some("6.1.0"));
    }

    #[test]
    fn test_11_interp_in_plugin_configuration() {
        let mut r = TestResolver::default();
        let mut cfg = XmlValue::default();
        cfg.children
            .entry("outputDir".to_string())
            .or_default()
            .push(XmlValue {
                text: Some("${project.basedir}/target".to_string()),
                ..XmlValue::default()
            });

        let plugin = RawPlugin {
            group_id: "org.apache.maven.plugins".to_string(),
            artifact_id: "maven-compiler-plugin".to_string(),
            version: Some("3.11.0".to_string()),
            configuration: Some(cfg),
            ..RawPlugin::default()
        };
        let mut input = pom("g", "a", "1.0");
        input.build = Some(RawBuild {
            plugins: vec![plugin],
            ..RawBuild::default()
        });

        let eff = build_effective(input, &mut r).expect("ok");
        let outer_cfg = eff.pom.build.as_ref().unwrap().plugins[0]
            .configuration
            .as_ref()
            .unwrap();
        let od = &outer_cfg.children["outputDir"][0];
        // ${project.basedir} resolves to "" in v0.1; we therefore
        // expect the path to start with "/target".
        assert_eq!(od.text.as_deref(), Some("/target"));
    }

    #[test]
    fn test_12_dependencies_append() {
        let mut r = TestResolver::default();
        let mut parent = pom("g", "p", "1.0");
        parent
            .dependencies
            .extend([dep("g1", "a1", Some("1")), dep("g2", "a2", Some("2"))]);
        r.add(parent);

        let mut child = pom("g", "c", "1.0");
        child.parent = Some(parent_ref("g", "p", "1.0"));
        child.dependencies.extend([
            dep("g3", "a3", Some("3")),
            dep("g4", "a4", Some("4")),
            dep("g5", "a5", Some("5")),
        ]);

        let eff = build_effective(child, &mut r).expect("ok");
        assert_eq!(eff.pom.dependencies.len(), 5);
        // Parent's deps come first.
        assert_eq!(eff.pom.dependencies[0].artifact_id, "a1");
        assert_eq!(eff.pom.dependencies[4].artifact_id, "a5");
    }

    #[test]
    fn test_13_repository_id_dedup_child_wins() {
        let mut r = TestResolver::default();
        let mut parent = pom("g", "p", "1.0");
        parent.repositories.push(RawRepository {
            id: Some("central".to_string()),
            url: Some("https://parent.example".to_string()),
            ..RawRepository::default()
        });
        r.add(parent);

        let mut child = pom("g", "c", "1.0");
        child.parent = Some(parent_ref("g", "p", "1.0"));
        child.repositories.push(RawRepository {
            id: Some("central".to_string()),
            url: Some("https://child.example".to_string()),
            ..RawRepository::default()
        });

        let eff = build_effective(child, &mut r).expect("ok");
        assert_eq!(eff.pom.repositories.len(), 1);
        assert_eq!(
            eff.pom.repositories[0].url.as_deref(),
            Some("https://child.example")
        );
    }

    #[test]
    fn test_14_plugin_inherited_false_skipped() {
        let mut r = TestResolver::default();
        let mut parent = pom("g", "p", "1.0");
        parent.build = Some(RawBuild {
            plugins: vec![
                RawPlugin {
                    group_id: "g.x".to_string(),
                    artifact_id: "keepme".to_string(),
                    ..RawPlugin::default()
                },
                RawPlugin {
                    group_id: "g.x".to_string(),
                    artifact_id: "dropme".to_string(),
                    inherited: Some("false".to_string()),
                    ..RawPlugin::default()
                },
            ],
            ..RawBuild::default()
        });
        r.add(parent);

        let mut child = pom("g", "c", "1.0");
        child.parent = Some(parent_ref("g", "p", "1.0"));

        let eff = build_effective(child, &mut r).expect("ok");
        let plugins = &eff.pom.build.as_ref().unwrap().plugins;
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].artifact_id, "keepme");
    }

    #[test]
    fn test_15_env_var_lookup() {
        unsafe {
            std::env::set_var("BARISTA_TEST_ENV_VAR_15", "ZAP");
        }
        let mut r = TestResolver::default();
        let input = pom("g", "a", "${env.BARISTA_TEST_ENV_VAR_15}");
        let eff = build_effective(input, &mut r).expect("ok");
        assert_eq!(eff.pom.version.as_deref(), Some("ZAP"));
        // Cleanup
        unsafe {
            std::env::remove_var("BARISTA_TEST_ENV_VAR_15");
        }
    }

    #[test]
    fn test_16_chain_depth_exceeded() {
        // Build 12 ancestors linked head-to-tail.
        let mut r = TestResolver::default();
        for i in 0..12u32 {
            let mut p = pom("g", &format!("p{}", i), "1");
            if i + 1 < 12 {
                p.parent = Some(parent_ref("g", &format!("p{}", i + 1), "1"));
            }
            r.add(p);
        }
        let mut child = pom("g", "child", "1.0");
        child.parent = Some(parent_ref("g", "p0", "1"));
        let err = build_effective(child, &mut r).unwrap_err();
        assert!(matches!(err, EffectiveError::ChainTooDeep { .. }));
    }

    #[test]
    fn test_17_cycle_detected() {
        let mut r = TestResolver::default();
        let mut a = pom("g", "A", "1");
        a.parent = Some(parent_ref("g", "B", "1"));
        let mut b = pom("g", "B", "1");
        b.parent = Some(parent_ref("g", "A", "1"));
        r.add(a);
        r.add(b);

        let mut child = pom("g", "child", "1");
        child.parent = Some(parent_ref("g", "A", "1"));
        let err = build_effective(child, &mut r).unwrap_err();
        assert!(matches!(err, EffectiveError::CircularParent { .. }));
    }

    #[test]
    fn test_18_parent_resolver_error_propagates() {
        let mut r = TestResolver::default();
        // Parent not added to fixture → resolver returns error.
        let mut child = pom("g", "c", "1");
        child.parent = Some(parent_ref("g", "missing-parent", "9"));
        let err = build_effective(child, &mut r).unwrap_err();
        match err {
            EffectiveError::ParentResolution { coords, .. } => {
                assert!(coords.contains("missing-parent"));
            }
            other => panic!("expected ParentResolution, got {:?}", other),
        }
    }

    #[test]
    fn test_19_property_values_self_interpolate() {
        // Properties values get interpolated; this is what enables
        // `<spring.version>${project.version}</spring.version>` style
        // declarations.
        let mut r = TestResolver::default();
        let mut input = pom("g", "a", "3.2.1");
        input.properties = props(&[("derived", "v=${project.version}")]);
        let eff = build_effective(input, &mut r).expect("ok");
        assert_eq!(
            eff.pom
                .properties
                .entries
                .get("derived")
                .map(String::as_str),
            Some("v=3.2.1")
        );
    }

    #[test]
    fn test_20_settings_placeholder_unresolved() {
        // `${settings.*}` is explicitly out of scope for Task 2 and
        // must surface as UnresolvedPlaceholder.
        let mut r = TestResolver::default();
        let mut input = pom("g", "a", "1");
        input.url = Some("${settings.localRepository}".to_string());
        let err = build_effective(input, &mut r).unwrap_err();
        assert!(matches!(err, EffectiveError::UnresolvedPlaceholder { .. }));
    }

    #[test]
    fn test_21_dependency_management_appends() {
        let mut r = TestResolver::default();
        let mut parent = pom("g", "p", "1");
        parent.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("a.g", "a", Some("1"))],
        });
        r.add(parent);

        let mut child = pom("g", "c", "1");
        child.parent = Some(parent_ref("g", "p", "1"));
        child.dependency_management = Some(DependencyManagement {
            dependencies: vec![dep("b.g", "b", Some("2"))],
        });

        let eff = build_effective(child, &mut r).expect("ok");
        let dm = eff.pom.dependency_management.unwrap();
        assert_eq!(dm.dependencies.len(), 2);
        assert_eq!(dm.dependencies[0].artifact_id, "a");
        assert_eq!(dm.dependencies[1].artifact_id, "b");
    }

    #[test]
    fn test_22_packaging_inheritance() {
        let mut r = TestResolver::default();
        let mut parent = pom("g", "p", "1");
        parent.packaging = "pom".to_string();
        r.add(parent);

        let mut child = pom("g", "c", "1");
        child.parent = Some(parent_ref("g", "p", "1"));
        // child.packaging defaults to "jar" — but parent says "pom",
        // so it should inherit.
        let eff = build_effective(child, &mut r).expect("ok");
        assert_eq!(eff.pom.packaging, "pom");
    }

    #[test]
    fn test_23_modules_not_inherited() {
        let mut r = TestResolver::default();
        let mut parent = pom("g", "p", "1");
        parent.modules = vec!["api".into(), "core".into()];
        r.add(parent);

        let mut child = pom("g", "c", "1");
        child.parent = Some(parent_ref("g", "p", "1"));
        let eff = build_effective(child, &mut r).expect("ok");
        assert!(eff.pom.modules.is_empty());
    }

    #[test]
    fn test_24_parent_field_cleared_in_result() {
        let mut r = TestResolver::default();
        r.add(pom("g", "p", "1"));
        let mut child = pom("g", "c", "1");
        child.parent = Some(parent_ref("g", "p", "1"));
        let eff = build_effective(child, &mut r).expect("ok");
        assert!(eff.pom.parent.is_none());
    }

    #[test]
    fn test_25_interpolation_audit_trail_populated() {
        let mut r = TestResolver::default();
        let mut input = pom("g", "a", "${v}");
        input.properties = props(&[("v", "9.9")]);
        input.name = Some("${project.artifactId}".to_string());
        let eff = build_effective(input, &mut r).expect("ok");
        assert!(eff.interpolations.iter().any(|i| i.resolved_to == "9.9"));
        assert!(
            eff.interpolations
                .iter()
                .any(|i| i.resolved_to == "a" && i.placeholder == "${project.artifactId}")
        );
    }

    #[test]
    fn test_26_exclusion_interpolation() {
        let mut r = TestResolver::default();
        let mut input = pom("g", "a", "1");
        input.properties = props(&[("excl.g", "junk.g"), ("excl.a", "junk.a")]);
        let mut d = dep("x", "y", Some("1"));
        d.exclusions.push(RawExclusion {
            group_id: "${excl.g}".to_string(),
            artifact_id: "${excl.a}".to_string(),
        });
        input.dependencies.push(d);

        let eff = build_effective(input, &mut r).expect("ok");
        let x = &eff.pom.dependencies[0].exclusions[0];
        assert_eq!(x.group_id, "junk.g");
        assert_eq!(x.artifact_id, "junk.a");
    }

    #[test]
    fn test_27_pluginmanagement_inherited_false() {
        let mut r = TestResolver::default();
        let mut parent = pom("g", "p", "1");
        parent.build = Some(RawBuild {
            plugin_management: Some(RawPluginManagement {
                plugins: vec![
                    RawPlugin {
                        group_id: "g.x".to_string(),
                        artifact_id: "keepme".to_string(),
                        ..RawPlugin::default()
                    },
                    RawPlugin {
                        group_id: "g.x".to_string(),
                        artifact_id: "dropme".to_string(),
                        inherited: Some("false".to_string()),
                        ..RawPlugin::default()
                    },
                ],
            }),
            ..RawBuild::default()
        });
        r.add(parent);

        let mut child = pom("g", "c", "1");
        child.parent = Some(parent_ref("g", "p", "1"));
        let eff = build_effective(child, &mut r).expect("ok");
        let pm = eff
            .pom
            .build
            .as_ref()
            .unwrap()
            .plugin_management
            .as_ref()
            .unwrap();
        assert_eq!(pm.plugins.len(), 1);
        assert_eq!(pm.plugins[0].artifact_id, "keepme");
    }

    #[test]
    fn test_28_build_scalar_inherited() {
        let mut r = TestResolver::default();
        let mut parent = pom("g", "p", "1");
        parent.build = Some(RawBuild {
            source_directory: Some("src/main/jaba".to_string()),
            final_name: Some("${project.artifactId}-final".to_string()),
            ..RawBuild::default()
        });
        r.add(parent);

        let mut child = pom("g", "c", "1");
        child.parent = Some(parent_ref("g", "p", "1"));
        // Child has its own (empty) build to exercise the (Some,Some) merge path.
        child.build = Some(RawBuild::default());
        let eff = build_effective(child, &mut r).expect("ok");
        let b = eff.pom.build.as_ref().unwrap();
        assert_eq!(b.source_directory.as_deref(), Some("src/main/jaba"));
        // final_name should have been interpolated with the merged
        // groupId/artifactId of the *child* (project context).
        assert_eq!(b.final_name.as_deref(), Some("c-final"));
    }

    #[test]
    fn test_29_profiles_appended_inactivated() {
        let mut r = TestResolver::default();
        let mut parent = pom("g", "p", "1");
        parent.profiles.push(RawProfile {
            id: Some("from-parent".to_string()),
            ..RawProfile::default()
        });
        r.add(parent);

        let mut child = pom("g", "c", "1");
        child.parent = Some(parent_ref("g", "p", "1"));
        child.profiles.push(RawProfile {
            id: Some("from-child".to_string()),
            ..RawProfile::default()
        });

        let eff = build_effective(child, &mut r).expect("ok");
        let ids: Vec<_> = eff
            .pom
            .profiles
            .iter()
            .map(|p| p.id.clone().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["from-parent", "from-child"]);
    }

    #[test]
    fn test_30_no_interpolation_when_no_placeholders() {
        let mut r = TestResolver::default();
        let mut input = pom("g", "a", "1.0");
        input.name = Some("plain-string".to_string());
        let eff = build_effective(input, &mut r).expect("ok");
        // Should produce no audit entries — fast path took.
        assert!(eff.interpolations.is_empty());
    }
}
