# Barista CLI output schemas — v1

This directory hosts the published JSON Schemas (Draft 2020-12) for `barista`
CLI structured output. They are language-neutral artifacts: the Rust renderer
in `crates/barista-cli/src/output/` is one consumer; downstream tooling
(IDE plugins, CI bots, dashboards) is the other.

## Schemas

| File                  | `command` discriminator | Emitted by                          |
| --------------------- | ----------------------- | ----------------------------------- |
| `pull.json`           | `pull`                  | `barista pull --output json`        |
| `grind-tree.json`     | `grind-tree`            | `barista grind tree --format json`  |
| `verify.json`         | `verify`                | `barista verify --output json` (stub; see PRD §9 / M5.x) |
| `progress-event.json` | _(see `event`)_         | one per line of `--output ndjson`   |

All top-level documents declare `additionalProperties: false` and use a
constant-string `command` field as a discriminator so consumers can route on
a single key without resorting to structural sniffing. Field names are
kebab-case throughout (`project-root`, `lockfile-status`, …).

## Versioning

The schema set is versioned by directory: `v1/` is the current stable shape.
Any backward-incompatible change — renaming a field, narrowing an enum,
adding a required field — lands as a sibling `v2/` directory; `v1/` continues
to be served at its existing `$id` URLs indefinitely. Backward-compatible
additions (new optional fields, new enum values on output-only fields that
consumers tolerate) are made in place.

## Validating a document

The schemas are vanilla Draft 2020-12, so any compliant validator works.

With [`ajv`](https://ajv.js.org/) (Node.js):

```sh
npx ajv-cli validate \
  -s schema/output/v1/pull.json \
  -d sample-pull.json \
  --spec=draft2020 --strict=false
```

With Python (`jsonschema`):

```sh
python -m jsonschema -i sample-pull.json schema/output/v1/pull.json
```

The Rust integration test under
`crates/barista-cli/tests/output_schema_validation.rs` exercises every schema
against representative happy-path documents plus a negative case.

## Worked examples

### `pull.json`

```json
{
  "command": "pull",
  "project-root": "/Users/dev/projects/example",
  "lockfile-status": "written",
  "entries": 142,
  "fetched": 17,
  "no-fetch": false,
  "strict": false,
  "warnings": [
    "snapshot artifact com.example:foo:1.0-SNAPSHOT refetched from network"
  ]
}
```

### `grind-tree.json`

```json
{
  "command": "grind-tree",
  "root-coord": "com.example:app:1.0.0",
  "nodes": [
    {
      "coord": "com.example:app:1.0.0",
      "scope": "compile",
      "depth": 0,
      "children": ["org.slf4j:slf4j-api:2.0.13"]
    },
    {
      "coord": "org.slf4j:slf4j-api:2.0.13",
      "scope": "compile",
      "depth": 1,
      "children": []
    }
  ]
}
```

### `verify.json` (stub)

```json
{
  "command": "verify",
  "status": "not-yet-implemented",
  "details": []
}
```

### `progress-event.json`

NDJSON stream — one object per line. A representative `pull` run:

```
{"event":"started","timestamp":"2026-05-14T12:00:00.000Z","phase":"resolve"}
{"event":"resolving","timestamp":"2026-05-14T12:00:00.123Z","phase":"resolve","progress":42}
{"event":"fetching","timestamp":"2026-05-14T12:00:01.456Z","phase":"fetch","coord":"org.slf4j:slf4j-api:2.0.13","progress":0}
{"event":"fetched","timestamp":"2026-05-14T12:00:01.789Z","phase":"fetch","coord":"org.slf4j:slf4j-api:2.0.13","progress":100}
{"event":"cached","timestamp":"2026-05-14T12:00:01.790Z","phase":"fetch","coord":"com.example:already-cached:1.0.0"}
{"event":"writing-lockfile","timestamp":"2026-05-14T12:00:02.000Z","phase":"lock-write"}
{"event":"completed","timestamp":"2026-05-14T12:00:02.100Z"}
{"event":"result","timestamp":"2026-05-14T12:00:02.101Z","payload":{"command":"pull","project-root":"/Users/dev/projects/example","lockfile-status":"written","entries":142,"fetched":17,"no-fetch":false,"strict":false}}
```

Per-variant required fields are enforced via `if`/`then` branches inside
`allOf`: `fetching`/`fetched`/`cached` require `coord`; `result`/`error`
require `payload`.
