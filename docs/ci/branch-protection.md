# Branch protection setup for `main`

This document is the operator-facing setup for the GitHub
branch-protection ruleset on `main`. It is the wiring step that turns
the declared policy in `.github/security-agent/guardrails.yaml` into
GitHub-enforced gates.

The auto-remediation agent
(`.github/workflows/auto-remediate.yml`) MUST NOT be enabled on `main`
until every section below is complete. The agent's safety model
depends on `main` being branch-protected such that:

- agent-authored PRs cannot bypass review (the bot has no admin
  privileges; the App's permissions explicitly exclude the
  `administration:write` scope per the guardrails YAML);
- every required status check (the full secret-scan + SCA + SAST +
  CodeQL + workflow-lint suite) must be green before merge; and
- force-pushes to `main` are denied even for repository admins.

The companion policy declaration lives at
`.github/security-agent/guardrails.yaml`. When the live
branch-protection ruleset and the declared policy drift, the live
ruleset is authoritative for what GitHub enforces, but the policy
declaration is authoritative for what the policy *says* — drift is a
misconfiguration to be reconciled, not a feature.

## 1. Prerequisites

Before applying branch protection, confirm:

- `gh` CLI is installed and authenticated against an account with
  admin rights on the repository (`gh auth status` shows the
  expected user; the user is in the org's repo admins).
- The repository's required status checks have produced at least one
  successful run on `main`. GitHub's branch-protection API rejects
  `required_status_checks.contexts` entries that the API has never
  observed on the branch, so each check name listed in the guardrails
  YAML's `pull_request.required_status_checks` must have already
  appeared at least once.
- The DCO / signed-commits substrate from C.4 Task 6 is in place. The
  branch-protection rule below requires signed commits; flipping that
  flag on without the DCO bot wired up will block every PR until the
  bot is live.

## 2. Apply the ruleset via `gh api`

The following invocation applies the full ruleset on `main`. Run from
the repository root:

```bash
# Resolve owner/repo from the local git remote (assumes `origin` is
# the canonical remote on GitHub).
REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner)

gh api \
  --method PUT \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  "/repos/${REPO}/branches/main/protection" \
  --input - <<'EOF'
{
  "required_status_checks": {
    "strict": true,
    "contexts": [
      "gitleaks",
      "trufflehog",
      "cargo-deny",
      "cargo-audit",
      "OSV-Scanner",
      "OWASP Dependency-Check (barback)",
      "Trivy fs",
      "clippy (workspace, all-targets)",
      "unsafe-line ratchet",
      "Semgrep",
      "Semgrep custom-rule round-trip",
      "SpotBugs + FindSecBugs (barback)",
      "Analyze (rust)",
      "Analyze (java-kotlin)",
      "zizmor",
      "zizmor synthetic-failure round-trip",
      "actionlint",
      "security-agent config validation"
    ]
  },
  "enforce_admins": true,
  "required_pull_request_reviews": {
    "dismiss_stale_reviews": true,
    "require_code_owner_reviews": false,
    "required_approving_review_count": 1,
    "require_last_push_approval": true
  },
  "restrictions": null,
  "required_linear_history": true,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "block_creations": false,
  "required_conversation_resolution": true,
  "lock_branch": false,
  "allow_fork_syncing": false,
  "required_signatures": true
}
EOF
```

Key field rationale:

| Field | Value | Why |
|---|---|---|
| `required_status_checks.contexts` | full list from guardrails YAML | Every secret-scan / SCA / SAST / CodeQL / workflow-lint check must be green before merge eligibility. |
| `required_status_checks.strict` | `true` | The PR branch must be up to date with `main` before merge. Prevents merging a PR whose checks last ran against a stale base. |
| `enforce_admins` | `true` | Admins are NOT exempt. The agent's bot identity has no admin scope (per the guardrails `identity.forbidden_permissions` list); this closes the loop for human admins too. |
| `required_pull_request_reviews.required_approving_review_count` | `1` | At least one human approval per PR. Matches `pull_request.required_approving_review_count` in the guardrails YAML. |
| `required_pull_request_reviews.dismiss_stale_reviews` | `true` | Any push to the PR branch invalidates prior approvals. Matches `pull_request.dismiss_stale_reviews_on_push: true` in the guardrails YAML. |
| `required_pull_request_reviews.require_last_push_approval` | `true` | The last push to a PR must be approved separately even if earlier commits were approved. Defense in depth against an approver who reviewed once and never re-reviewed. |
| `allow_force_pushes` | `false` | Force-pushes to `main` are denied. Matches `pull_request.allow_force_push_to_pr_branch: false` philosophy at the base-branch level. |
| `required_linear_history` | `true` | Merge commits are disallowed; the merge strategy is squash-or-rebase. Keeps the audit trail linear. |
| `required_conversation_resolution` | `true` | Review-thread conversations must be resolved before merge. Forces reviewer concerns to be addressed, not just acknowledged. |
| `required_signatures` | `true` | All commits on `main` must be GPG/S/MIME-signed. Depends on the C.4 Task 6 DCO / signed-commits substrate. |

## 3. Web UI equivalent

For operators who prefer point-and-click, the equivalent settings are
applied via the repository's Settings → Branches → Branch protection
rules → `main` page:

1. **Branch name pattern**: `main`.
2. **Require a pull request before merging**: enabled.
   - **Require approvals**: `1`.
   - **Dismiss stale pull request approvals when new commits are
     pushed**: enabled.
   - **Require approval of the most recent reviewable push**: enabled.
3. **Require status checks to pass before merging**: enabled.
   - **Require branches to be up to date before merging**: enabled.
   - **Status checks**: search and add each entry from the
     `required_status_checks` list in the guardrails YAML.
4. **Require conversation resolution before merging**: enabled.
5. **Require signed commits**: enabled.
6. **Require linear history**: enabled.
7. **Do not allow bypassing the above settings**: enabled (this is
   `enforce_admins` in API terms).
8. **Allow force pushes**: disabled.
9. **Allow deletions**: disabled.

## 4. Verification

After applying the ruleset, confirm it took effect:

```bash
REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner)

gh api "/repos/${REPO}/branches/main/protection" --jq '
  {
    required_checks: .required_status_checks.contexts,
    strict: .required_status_checks.strict,
    review_count: .required_pull_request_reviews.required_approving_review_count,
    dismiss_stale: .required_pull_request_reviews.dismiss_stale_reviews,
    last_push_approval: .required_pull_request_reviews.require_last_push_approval,
    enforce_admins: .enforce_admins.enabled,
    allow_force_pushes: .allow_force_pushes.enabled,
    required_signatures: .required_signatures.enabled,
    linear_history: .required_linear_history.enabled,
    conversation_resolution: .required_conversation_resolution.enabled
  }
'
```

Expected output: every boolean field is `true`, `required_checks`
matches the guardrails YAML list exactly (including order-independent
membership), `review_count` is `1` (or higher), and `strict` is
`true`.

If the output does not match, do not enable the auto-remediation
workflow. The agent's safety model depends on every gate above
being live.

## 5. Re-running this setup

The branch-protection ruleset is idempotent: re-applying the same
payload produces the same state. Re-run the `gh api` invocation
after any of the following:

- A new required status check is added (a new workflow job's
  `name:` field appears in the guardrails YAML); the contexts list
  must be expanded to include it.
- A status check is renamed (a workflow job's `name:` field
  changes); the contexts list must be updated, otherwise the
  branch-protection rule will block every PR waiting for a check
  that no longer exists.
- The required-approval count changes in the guardrails YAML.
- The GitHub App identity is created or rotated.

The guardrails YAML is the source of intent; this document is the
operator's procedure for materializing that intent in GitHub.

## 6. Re-confirm before enabling auto-remediation

Per the guardrails YAML's `branch_protection_precondition.operator_checklist`,
the operator must verify every condition below is true before enabling
the auto-remediation workflow on `main`:

- [ ] Branch protection enabled on `main`.
- [ ] All `required_status_checks` from the guardrails YAML appear in
      the branch-protection ruleset.
- [ ] `required_approving_review_count: 1` (or higher) on `main`.
- [ ] Force-push to `main` disabled.
- [ ] GitHub App created with permissions matching
      `identity.required_permissions` and NONE of
      `identity.forbidden_permissions`.
- [ ] DCO / signed-commits substrate in place (C.4 Task 6).

When every box is checked, the `[H]`-gated enabling of
auto-remediation on `main` may proceed.
