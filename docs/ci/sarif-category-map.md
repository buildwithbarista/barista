# SARIF category map

This file is the canonical reference for the SARIF `category:` values
the security workflows under `.github/workflows/` upload to GitHub
code scanning. Each category appears as a distinct row in the Security
tab so findings group cleanly by tool.

## Conventions

- Every category is the **lowercase tool short name**.
- Where a single tool emits multiple SARIF documents from one workflow
  (e.g., CodeQL's per-language analysis), a stable suffix is appended
  via the analyzer's documented mechanism (CodeQL uses
  `/language:<name>`). No two workflows reuse the same category.
- Categories must remain stable across releases — changing a category
  effectively resets the alert history for that tool in the Security
  tab.

## Native SARIF emitters

These tools emit SARIF directly; the workflow consumes the upstream
output and uploads it via `github/codeql-action/upload-sarif`.

| Scanner   | Workflow                              | Category              | Source SARIF                         |
|-----------|---------------------------------------|-----------------------|--------------------------------------|
| gitleaks  | `secret-scan.yml`                     | `gitleaks`            | `results.sarif` (action output)      |
| trufflehog| `secret-scan.yml`                     | `trufflehog`          | `trufflehog.sarif` (action output)   |
| trivy-fs  | `sca.yml`, `sca-nightly.yml`          | `trivy-fs`            | `trivy-results.sarif`                |
| osv-scanner| `sca.yml`, `sca-nightly.yml`         | (action default)      | reusable workflow handles upload     |
| owasp-dc  | `sca.yml`, `sca-nightly.yml`          | `owasp-dc`            | `barback/target/dependency-check-report.sarif` |
| semgrep   | `sast.yml`                            | `semgrep`             | `semgrep.sarif`                      |
| codeql    | `codeql.yml`                          | `/language:rust`, `/language:java` | action-native (analyze step) |
| zizmor    | `workflow-lint.yml`                   | `zizmor`              | action-native (`advanced-security: true`) |

## Adapter-translated SARIF emitters

These tools do not emit SARIF natively. A small adapter under
`.github/scripts/` parses the tool's JSON output and produces SARIF
2.1.0; the workflow uploads the adapter's output.

| Scanner    | Workflow                              | Category      | Adapter                                       |
|------------|---------------------------------------|---------------|-----------------------------------------------|
| cargo-deny | `sca.yml`, `sca-nightly.yml`          | `cargo-deny`  | `.github/scripts/cargo-deny-to-sarif.py`      |
| cargo-audit| `sca.yml`, `sca-nightly.yml`          | `cargo-audit` | `.github/scripts/cargo-audit-to-sarif.py`     |
| spotbugs   | `sast.yml`                            | `spotbugs`    | `barback/pom.xml` (`<sarifOutput>true</sarifOutput>` on `spotbugs-maven-plugin`) — emits `target/spotbugsSarif.json` natively when toggled; no Python adapter needed |

The SpotBugs entry sits in this section because the plugin's SARIF
output is gated behind a configuration toggle rather than being the
default; the `<sarifOutput>true</sarifOutput>` block in `barback/pom.xml`
is what makes the plugin emit the file the upload step consumes.

## Adding a new scanner

1. Pick a lowercase category that doesn't collide with anything in
   the tables above.
2. Add the SARIF upload step in the scanner's workflow with
   `category: <name>` and `if: always()` so findings surface even
   when the scan step fails.
3. Append a row to the appropriate table here in the same PR.
4. If the scanner is the first to run on the security suite's
   `workflow_run` trigger, add the workflow's `name:` to
   `.github/workflows/security-finding-to-issue.yml` so issues get
   filed automatically.
5. If the scanner doesn't emit SARIF natively, add an adapter under
   `.github/scripts/<scanner>-to-sarif.py` and reference it from the
   workflow.

## Verifying SARIF upload

- The Security tab shows one row per category under "Tool"; if a
  category is missing, the upload step failed silently (look at the
  workflow run's job summary).
- The `security-finding-to-issue.yml` workflow queries the
  `/repos/{owner}/{repo}/code-scanning/alerts` endpoint, which
  aggregates every uploaded SARIF run; alerts that originate from a
  newly-added scanner show up there once the first upload completes.
