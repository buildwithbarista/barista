# Auto-generated finding drafts

Drafts emitted by `barista-netanalyze` land here. Every file in this directory carries
the placeholder id `EFF-2026-PENDING` — drafts are **not** part of the catalog until a
human promotes them.

## Do not edit drafts here

Drafts are transient: the next pipeline run will overwrite them. If a draft is worth
keeping, promote it:

```bash
cargo xtask findings promote docs/efficiency/findings/auto-generated/<file>.md
```

That subcommand allocates the next free `EFF-2026-NNN`, rewrites the `id:` frontmatter,
moves the file into the parent `findings/` directory, and deletes the original.

## What's tracked in git

This directory is **gitignored** except for:

- this `README.md`
- the sibling `.gitignore`
- any draft a maintainer explicitly force-adds because it's a long-lived work item that
  has not yet been promoted (rare; the bias is toward promoting or discarding promptly)

The catalog itself — `docs/efficiency/findings/EFF-2026-*.md` — is the durable record.

## See also

- [`../README.md`](../README.md) — the catalog landing page; lifecycle, schema, ID
  policy.
- `crates/barista-netanalyze/` — the pipeline that writes drafts here.
