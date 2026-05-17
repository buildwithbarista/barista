#!/usr/bin/env bash
# Verify .github/security-agent/guardrails.yaml parses cleanly and
# encodes every one of the six guardrail clauses (a)-(f) from the
# auto-remediation policy.
#
# Usage:
#   bash scripts/test-guardrails-yaml.sh
#
# Exits 0 on success. Exits non-zero with a diagnostic on the first
# failure.
#
# Why a shell script that shells to Python? `python3 -c 'import yaml'`
# is already a dependency of the pre-commit framework + Semgrep, so
# no new tool needs to land on a contributor's machine for this test
# to run. A pure-Rust round-trip test would also work but would pull
# `serde_yaml` into the workspace just for this single config file;
# Python's `yaml` module is the lower-overhead choice.

set -euo pipefail

GUARDRAILS_YAML=".github/security-agent/guardrails.yaml"

if [[ ! -f "${GUARDRAILS_YAML}" ]]; then
  echo "::error::${GUARDRAILS_YAML} not found"
  exit 1
fi

python3 - <<'PYEOF'
import sys
import yaml

path = ".github/security-agent/guardrails.yaml"

# (1) Round-trip-load — bare parse must succeed.
try:
    with open(path, "r", encoding="utf-8") as f:
        doc = yaml.safe_load(f)
except yaml.YAMLError as e:
    print(f"::error::{path} failed to parse as YAML: {e}", file=sys.stderr)
    sys.exit(1)

if not isinstance(doc, dict):
    print(f"::error::{path} top level must be a mapping; got {type(doc).__name__}", file=sys.stderr)
    sys.exit(1)

errors = []

def require(condition, message):
    if not condition:
        errors.append(message)

def require_keys(mapping, keys, where):
    if not isinstance(mapping, dict):
        errors.append(f"{where} must be a mapping; got {type(mapping).__name__}")
        return
    for k in keys:
        if k not in mapping:
            errors.append(f"{where}.{k} is missing")

# Schema version.
require(doc.get("schema_version") == 1, "schema_version must equal 1")

# (a) Non-admin bot identity.
identity = doc.get("identity", {})
require_keys(
    identity,
    ["github_app_slug", "required_permissions", "forbidden_permissions", "bot_login"],
    "identity",
)
require(
    "administration:write" in identity.get("forbidden_permissions", []),
    "identity.forbidden_permissions must list 'administration:write' (clause (a): non-admin bot)",
)
require(
    isinstance(identity.get("required_permissions"), list) and len(identity["required_permissions"]) > 0,
    "identity.required_permissions must be a non-empty list",
)

# (b) Branch-protected `main` requires human approval; agent NOT exempt.
pr = doc.get("pull_request", {})
require_keys(
    pr,
    [
        "base_branch",
        "required_approving_review_count",
        "agent_is_exempt_from_review",
        "required_status_checks",
        "templates",
        "dismiss_stale_reviews_on_push",
        "allow_force_push_to_pr_branch",
    ],
    "pull_request",
)
require(
    pr.get("base_branch") == "main",
    "pull_request.base_branch must be 'main'",
)
require(
    isinstance(pr.get("required_approving_review_count"), int) and pr["required_approving_review_count"] >= 1,
    "pull_request.required_approving_review_count must be >= 1 (clause (b): human approval required)",
)
require(
    pr.get("agent_is_exempt_from_review") is False,
    "pull_request.agent_is_exempt_from_review must be False (clause (b): agent NOT exempt)",
)
require(
    pr.get("allow_force_push_to_pr_branch") is False,
    "pull_request.allow_force_push_to_pr_branch must be False",
)

# (c) Required status checks — full SAST + SCA + secret-scan suite.
checks = pr.get("required_status_checks", [])
require(
    isinstance(checks, list) and len(checks) > 0,
    "pull_request.required_status_checks must be a non-empty list",
)

required_check_groups = {
    "secret-scan": ["gitleaks", "trufflehog"],
    "sca": ["cargo-deny", "cargo-audit", "OSV-Scanner"],
    "sast": ["clippy (workspace, all-targets)", "Semgrep"],
}
for group, members in required_check_groups.items():
    for m in members:
        require(
            m in checks,
            f"pull_request.required_status_checks must include '{m}' (clause (c): {group} coverage)",
        )

# (d) PR template directory migration.
templates = pr.get("templates", {})
require_keys(templates, ["default", "agent", "agent_url_query"], "pull_request.templates")
require(
    templates.get("default") == ".github/PULL_REQUEST_TEMPLATE/default.md",
    "pull_request.templates.default must point to the directory-form default template",
)
require(
    templates.get("agent") == ".github/PULL_REQUEST_TEMPLATE/security-auto-remediation.md",
    "pull_request.templates.agent must point to the auto-remediation template",
)
require(
    templates.get("agent_url_query") == "?template=security-auto-remediation.md",
    "pull_request.templates.agent_url_query must be the documented query string",
)

migration = doc.get("pr_template_migration", {})
require(
    migration.get("status") == "complete",
    "pr_template_migration.status must be 'complete'",
)
sections = migration.get("agent_template_required_sections", [])
expected_sections = [
    "## Summary",
    "## What the agent did",
    "## Why this is the right fix",
    "## Sonatype verification",
    "## Verification",
    "## Status checks",
    "## Reviewer notes",
    "## Provenance",
]
for s in expected_sections:
    require(
        s in sections,
        f"pr_template_migration.agent_template_required_sections must include '{s}' (clause (d): template structure)",
    )

# (e) Per-day rate limit.
rate = doc.get("rate_limits", {})
require_keys(rate, ["max_prs_per_day", "max_attempts_per_issue", "window"], "rate_limits")
require(
    isinstance(rate.get("max_prs_per_day"), int) and rate["max_prs_per_day"] >= 1,
    "rate_limits.max_prs_per_day must be a positive integer (clause (e): per-day cap)",
)
require(
    isinstance(rate.get("max_attempts_per_issue"), int) and rate["max_attempts_per_issue"] >= 1,
    "rate_limits.max_attempts_per_issue must be a positive integer",
)

# (f) Two-strikes-and-stop failure policy.
fp = doc.get("failure_policy", {})
require_keys(
    fp,
    ["consecutive_ci_failures_before_giveup", "on_giveup", "ci_failure_definition"],
    "failure_policy",
)
require(
    fp.get("consecutive_ci_failures_before_giveup") == 2,
    "failure_policy.consecutive_ci_failures_before_giveup must equal 2 (clause (f): two strikes)",
)
on_giveup = fp.get("on_giveup", {})
require_keys(
    on_giveup,
    ["action", "comment_template", "halt_further_attempts_without_human_retrigger"],
    "failure_policy.on_giveup",
)
require(
    on_giveup.get("action") == "post_triage_comment",
    "failure_policy.on_giveup.action must be 'post_triage_comment'",
)
require(
    on_giveup.get("halt_further_attempts_without_human_retrigger") is True,
    "failure_policy.on_giveup.halt_further_attempts_without_human_retrigger must be True (clause (f): stop)",
)
require(
    isinstance(on_giveup.get("comment_template"), str)
    and "auto-remediation agent" in on_giveup["comment_template"].lower(),
    "failure_policy.on_giveup.comment_template must mention 'auto-remediation agent'",
)

# Branch-protection precondition.
bp = doc.get("branch_protection_precondition", {})
require_keys(bp, ["required", "documented_setup", "operator_checklist"], "branch_protection_precondition")
require(
    bp.get("required") is True,
    "branch_protection_precondition.required must be True",
)
require(
    bp.get("documented_setup") == "docs/ci/branch-protection.md",
    "branch_protection_precondition.documented_setup must point to docs/ci/branch-protection.md",
)
require(
    isinstance(bp.get("operator_checklist"), list) and len(bp["operator_checklist"]) > 0,
    "branch_protection_precondition.operator_checklist must be a non-empty list",
)

# Cross-references — sanity check that the policy points at its
# neighbouring surfaces.
xrefs = doc.get("cross_references", {})
for key in ["prompt", "allowed_tools", "workflow", "finding_issue_filer", "branch_protection_setup", "pr_templates"]:
    require(key in xrefs, f"cross_references.{key} is missing")

if errors:
    print("guardrails.yaml validation FAILED:", file=sys.stderr)
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    sys.exit(1)

print("guardrails.yaml validation OK — all 6 clauses (a)-(f) encoded.")
PYEOF
