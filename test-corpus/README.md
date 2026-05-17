# Maven-compat test corpus

This directory holds the **Maven-compatibility test corpus**: a curated set of
real-world Maven projects that Barista must be able to build identically to
upstream Maven. Golden tests and the weekly `corpus-baseline` CI workflow run
each project end-to-end and compare results.

## How it works (on-demand materialization)

Only **lockfiles** (`corpus.lock.toml`) are checked in. Upstream source trees
are cloned at runtime into `<id>/checkout/` (gitignored). This keeps the repo
small and avoids vendoring third-party code.

```
test-corpus/
├── README.md
├── commons-lang/
│   ├── corpus.lock.toml      # checked in
│   └── checkout/             # cloned on demand; gitignored
└── ...
```

## Adding a project

1. `mkdir test-corpus/<id> && $EDITOR test-corpus/<id>/corpus.lock.toml`
   (see schema below; copy an existing lockfile as a starting point).
2. From the repo root: `scripts/materialize-corpus.sh --filter <id>`.
3. `cd test-corpus/<id>/checkout && mvn -B verify` to confirm it builds
   cleanly with the pinned toolchain.
4. Add `<id>` to the `matrix.project` list in
   `.github/workflows/corpus-baseline.yml`.
5. Open a PR. The `corpus-baseline` workflow will verify the build on CI.

## `corpus.lock.toml` schema

| Key                  | Required | Description                                                                              |
| -------------------- | -------- | ---------------------------------------------------------------------------------------- |
| `id`                 | yes      | Short slug matching the directory name.                                                  |
| `description`        | yes      | One-line summary of the project.                                                         |
| `git_url`            | yes      | Upstream clone URL.                                                                      |
| `ref`                | yes      | Tag, branch, or commit SHA to materialize.                                               |
| `ref_kind`           | yes      | `"tag"`, `"branch"`, `"commit"`, or `"vendored"` (see below).                            |
| `jdk`                | yes      | JDK version; must match a cell in the `barback` CI matrix (`"17"` or `"21"`).            |
| `maven_version`      | yes      | Maven version; must match a cell in the `barback` CI matrix.                             |
| `build_cmd`          | yes      | Build command run in `checkout/`. Defaults to `mvn -B -DskipTests=false verify`.         |
| `build_time_minutes` | no       | Rough wall time on a typical CI runner. Informational; helps budget workflow timeouts.   |
| `notes`              | no       | Free-form maintainer notes — pinning gotchas, why this project is in the corpus, etc.    |

The materialization script ignores unknown keys, so the schema can be extended
without breaking existing tooling. Prefer **tags** for `ref` (not branches or
commits) — they're stable and produce a reproducible corpus.

### Vendored entries

A `ref_kind = "vendored"` entry is a self-contained Maven project whose
sources live inside the corpus directory itself (under `<id>/vendor/`).
There is no upstream `git clone`; the materialization script copies the
`vendor/` subtree into `checkout/` so downstream tools see the same layout
as upstream-cloned projects. Use vendored entries for synthetic shapes
that have no natural upstream — e.g. a "minimal Spring Boot starter-web"
target that exists to exercise a specific dependency footprint.

For vendored entries:

- `git_url` should be empty (`""`).
- `ref` should be a hand-maintained version slug describing the dominant
  dependency (e.g. `spring-boot-3.5.7`) so the lockfile reads
  deterministically.
- The full source tree lives at `<id>/vendor/` and is checked into the
  repo. Treat any change to it as a corpus-version bump.

## Growth target

The corpus grows incrementally:

- **Today:** 5 seed projects covering simple single-module and multi-module
  shapes.
- **Mid-term:** ~50 projects spanning a wider range of plugins and shapes.
- **Long-term:** ~100 projects, the full Maven-compat baseline.

The exact roster is intentionally open — see the open question on corpus
selection in the project tracker. Submissions are welcome.

## CI

The `corpus-baseline` workflow (`.github/workflows/corpus-baseline.yml`) runs
on PRs that touch this directory, the materialization script, or the workflow
itself, plus weekly on a schedule to catch upstream-tag churn. Each project
gets its own matrix cell: materialize, install the pinned JDK and Maven,
run the lockfile's `build_cmd`, and upload `target/*.log` on failure.

## Gitignored

`test-corpus/*/checkout/` — materialized upstream sources.
