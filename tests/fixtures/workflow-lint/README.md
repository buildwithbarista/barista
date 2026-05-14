# workflow-lint fixtures

This directory holds **deliberately insecure** GitHub Actions workflow
files used to verify the workflow-lint gate in
`.github/workflows/workflow-lint.yml`.

The fixtures live here — not in `.github/workflows/` — so GitHub does
not schedule them. They exist only to be fed to zizmor as input files
so the lint job can assert zizmor produces findings on the patterns it
is supposed to catch.

## Fixtures

### `insecure_pull_request_target.yml`

The canonical "pwn request" anti-pattern:

- Trigger: `pull_request_target` (runs in the base-repo context with
  read-write secrets).
- Checkout: `${{ github.event.pull_request.head.ref }}` from
  `${{ github.event.pull_request.head.repo.full_name }}` (attacker-
  controlled code from a fork).
- Side dish: `${{ github.event.pull_request.title }}` interpolated
  into a `run:` block, which is a template-injection sink.

zizmor's `dangerous-triggers` and/or `template-injection` rules **must**
flag this file. The `zizmor-fixture` job in `workflow-lint.yml` runs
zizmor against it and fails the build if zizmor either exits zero or
produces zero findings.

## Why this lives in `tests/fixtures/`

- Files under `.github/workflows/` are auto-scheduled by GitHub
  Actions. A vulnerable workflow placed there would run in production.
- The lint workflow needs an offline, deterministic input it can scan
  on every PR to prove the gate is wired correctly. A static fixture
  in the repo is the simplest way to guarantee that.

If you add a new fixture, document it above and reference it from the
`zizmor-fixture` job (or add a new round-trip job) so the round-trip
test always exercises every fixture you check in.
