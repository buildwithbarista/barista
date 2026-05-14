# Strict-resolver conflict fixtures

Synthetic dependency-graph fixtures that exercise the conflict cases the strict
resolver must handle. Each fixture is a self-contained TOML file describing a
small synthetic registry plus the expected outcome.

These fixtures are consumed by the strict-resolver test suite. They use
synthetic coordinates (`org.example:*`, `com.synthetic:*`) so they remain stable
across upstream changes.

## File layout

```
strict-conflicts/
├── README.md                        # this file
├── 01-clean-no-conflict.toml
├── 02-hard-range-satisfied.toml
├── ...
└── 15-optional-pulls-conflict.toml
```

A fixture's `id` field must match its filename without the `.toml` extension.

## Fixture schema

```toml
id = "string"                   # matches filename stem
description = "string"          # one-sentence human-readable summary
expected_outcome = "Resolved"   # one of: "Resolved", "Conflict"

# Required when expected_outcome = "Resolved": the version each coord
# should resolve to.
[expected_versions]
"org.example:lib" = "1.0.0"

# Required when expected_outcome = "Conflict": the dep-graph edges that
# the resolver's derivation tree must surface. Order is not significant.
[[expected_edges]]
from  = "org.example:root:1.0.0"
to    = "org.example:lib"
range = "[1.0]"

# One [[node]] per (coords, version) available in the synthetic registry.
# `dependencies` is what that node declares in its <dependencies> block.
[[node]]
coords = "org.example:root"
version = "1.0.0"
dependencies = [
    { coords = "org.example:lib", version = "[1.0]" },
]

[[node]]
coords = "org.example:lib"
version = "1.0.0"
```

### Field reference

| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Must equal the filename without extension. |
| `description` | string | yes | One sentence. |
| `expected_outcome` | enum | yes | `Resolved` or `Conflict`. |
| `expected_versions` | table | when `Resolved` | Map of `g:a` → version string. |
| `expected_edges` | array of tables | when `Conflict` | Each has `from`, `to`, `range`. |
| `node` | array of tables | yes (≥1) | One per `(coords, version)` in the registry. |
| `node.coords` | string | yes | `groupId:artifactId`. |
| `node.version` | string | yes | Concrete version (no range). |
| `node.dependencies` | array of tables | no | Defaults to empty. |
| `node.dependencies[].coords` | string | yes | `groupId:artifactId` of the requested dep. |
| `node.dependencies[].version` | string | yes | Maven `VersionSpec` — soft (`1.0`) or hard (`[1.0]`, `[1.0,2.0)`). |
| `node.dependencies[].scope` | string | no | Maven scope: `compile`, `runtime`, `test`, `provided`, `system`. |
| `node.dependencies[].optional` | bool | no | Defaults to `false`. |

### Conventions

- Every fixture has a **root node**: the first node whose coords end in `:root`.
- A fixture is internally consistent: every declared dependency's `coords`
  appears as the `coords` of at least one `[[node]]`. The loader test enforces
  this.
- Concrete versions in `[[node]].version` use semver-style strings
  (`1.0.0`, `2.0.0`). Ranges live only in `[[node]].dependencies[].version`.
- Conflict fixtures should produce a derivation tree that names the listed
  edges. T4 verifies this by exact-set comparison on `expected_edges`.
- Classifier and packaging variants use the extended Maven coord form
  `groupId:artifactId:packaging:classifier` where needed.

## The 15 fixtures

| # | id | Outcome | What it tests |
|---|---|---|---|
| 01 | clean-no-conflict | Resolved | Baseline: two-level tree, no conflict. |
| 02 | hard-range-satisfied | Resolved | A hard range `[1.0,2.0)` matched by exactly one available version. |
| 03 | hard-range-no-version | Conflict | A hard range with no satisfying version in the registry. |
| 04 | diamond-hard-conflict | Conflict | Diamond where two paths demand incompatible hard versions. |
| 05 | diamond-soft-resolves | Resolved | Diamond with soft requirements; strict resolver picks a version that satisfies both. |
| 06 | three-way-conflict | Conflict | Three paths each demand a different hard version. |
| 07 | cycle | Conflict | Direct cycle A → B → C → A. |
| 08 | version-range-narrowing | Resolved | Two ranges intersect to a non-empty window; pick the highest in the window. |
| 09 | excluded-version-via-multiple-ranges | Conflict | Two ranges that would overlap only on a version that does not exist in the registry. |
| 10 | snapshot-vs-release | Conflict | One path requires `-SNAPSHOT`, another a `-GA` release; strict mode flags. |
| 11 | deep-transitive-conflict | Conflict | Depth-5 transitive chain that conflicts with a shallow declaration. |
| 12 | classifier-distinct-no-conflict | Resolved | Same `groupId:artifactId` resolved at different classifiers; NOT a conflict. |
| 13 | empty-root | Resolved | Root with no declared dependencies; trivially resolved. |
| 14 | bom-import-narrowing | Conflict | BOM import pins a version that conflicts with a direct declaration. |
| 15 | optional-pulls-conflict | Resolved | An optional dependency would conflict, but is skipped per Maven semantics. |
