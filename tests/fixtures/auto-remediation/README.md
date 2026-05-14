# `tests/fixtures/auto-remediation/`

Reference fixtures for the security auto-remediation agent at
`.github/security-agent/`.

## `sonatype-query-transcript.json`

The expected shape and ordering of MCP calls the agent must make when
responding to a synthetic dependency-vulnerability finding. The agent's
prompt at `.github/security-agent/prompt.md` requires:

1. `mcp__sonatype-mcp__getRecommendedComponentVersions` — pick the target version
2. `mcp__sonatype-mcp__getComponentVersion` — confirm `malicious=false` and
   `policyCompliance.compliant=true` for the chosen target

…before any edit to `Cargo.toml` / `Cargo.lock`. This file is the diff target
for the end-to-end test that runs the agent against a planted-CVE issue and
captures its live tool-call transcript.

## Verification

Local: the JSON itself is the reference; parse it with `python3 -c "import
json; json.load(open('sonatype-query-transcript.json'))"` to confirm it is
well-formed.

CI: the planted-CVE acceptance test runs the agent via the `workflow_dispatch`
trigger on `.github/workflows/auto-remediate.yml` against a synthetic issue,
captures the agent's tool-call transcript, and asserts the call ordering
described under `expected_call_sequence` in this fixture (allowing additional
read-only calls between steps).
