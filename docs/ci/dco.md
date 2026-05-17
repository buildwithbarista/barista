# DCO (Developer Certificate of Origin) setup for `main`

This document is the operator-facing setup for the contributor-attestation
substrate that gates merges to `main`. It declares the project's chosen
policy, documents the alternative, and gives the operator the exact
`gh api` payload to wire it into branch protection.

The DCO substrate is a **precondition** for enabling the auto-remediation
workflow (`.github/workflows/auto-remediate.yml`) on `main`: until every
commit on `main` carries a contributor attestation, the bot's commit
identity is the weakest link in the chain. See also:

- `docs/ci/branch-protection.md` — the operator setup for the rest of
  the branch-protection ruleset (required status checks, force-push
  denial, dismiss-stale-reviews, etc.). DCO is one entry in that
  ruleset's `required_status_checks.contexts` list.
- `.github/security-agent/guardrails.yaml` — the C.5 auto-remediation
  policy declaration; its `branch_protection_precondition.operator_checklist`
  lists "DCO / signed-commits substrate in place (C.4 Task 6)" as a
  required pre-enable check.
- `.github/workflows/dco.yml` — the workflow this document configures
  GitHub to require.

## 1. Policy: DCO is the default; signed-commits is the documented alternative

**Default for this repository: DCO.** Every commit must carry a
`Signed-off-by:` trailer whose email matches the commit author's
email. Contributors generate the trailer with `git commit -s`. No GPG
or S/MIME key is required.

Rationale for picking DCO over GitHub's "Require signed commits"
branch-protection flag:

- **Lighter-weight onboarding.** No GPG key generation, no key upload
  to GitHub, no key-rotation procedure. The contributor surface is one
  flag on `git commit`. The trade-off is that DCO is an attestation,
  not a cryptographic proof; for a project at this maturity level
  (pre-1.0, open-source, broad contributor base expected) the
  attestation-vs-proof gap is acceptable.
- **OSS-standard pattern.** Used by the Linux kernel, Docker, GitLab,
  Kubernetes, and the CNCF generally. Outside contributors who have
  contributed to any of those will recognize `git commit -s` without
  needing to read this document.
- **Compatible with the auto-remediation bot.** The C.5 agent commits
  fixes as a GitHub App identity (`barista-security-bot[bot]` once the
  App is created — see the guardrails YAML's `identity.github_app_slug`).
  The App can be configured to author commits with a `Signed-off-by:`
  trailer pre-baked into its commit-message template; required-signature
  enforcement, by contrast, would require the App to hold a GPG key
  and sign each commit, which is operationally heavier.
- **Reversible.** Switching from DCO to signed-commits later is a
  one-line change to the branch-protection payload (see §4). Switching
  the other way is the same. The repository commits to a path here,
  not a permanent gate.

**Per-repo override:** an operator may pick signed-commits instead for
any individual public repository. The four public repos are independent
git repositories (`buildwithbarista/barista`, `buildwithbarista/barista.build`,
`buildwithbarista/homebrew-tap`, `buildwithbarista/barista-bench-site`)
and each one's branch-protection ruleset is configured separately.
The recommendation is to keep all four on the same policy for
contributor consistency; the default (DCO) is the recommendation for
all four unless an operator has a specific reason to upgrade.

## 2. Contributor surface

### One-time setup

Configure git with the identity that will appear on commits:

```bash
git config --global user.name "Your Name"
git config --global user.email "you@example.com"
```

The email used here MUST match the email on a commit's author line —
the DCO check verifies the `Signed-off-by:` trailer's email equals the
commit author's email. Project-local override (`--local` instead of
`--global`) is supported if you contribute under different identities
to different projects.

### Day-to-day workflow

Sign off every commit by appending `-s` (or `--signoff`):

```bash
git commit -s -m "feat(resolver): cache effective-POMs in O-REQ-04"
```

This appends a line of the form

```
Signed-off-by: Your Name <you@example.com>
```

to the commit message. The trailer is the DCO attestation — by adding
it, you affirm that you have the right to submit the work under the
project's license (see <https://developercertificate.org/> for the
verbatim text).

### Retroactive sign-off

If you've already made commits without `-s`:

```bash
# Sign off the most-recent commit only:
git commit --amend --signoff --no-edit

# Sign off every commit on the current branch back to <base>:
git rebase --signoff <base-branch>
```

Then force-push the rewritten branch:

```bash
git push --force-with-lease
```

The DCO check re-runs on every push, so a force-push that adds sign-offs
clears the gate immediately.

### Helpful aliases

A contributor who works on multiple DCO-required projects may want to
make `-s` automatic:

```bash
# Always pass --signoff to `git commit`:
git config --global format.signoff true

# Or define an alias for an explicit signoff invocation:
git config --global alias.cs 'commit -s'
```

`format.signoff true` is the cleanest option but it applies globally;
contributors who also work on projects that prohibit sign-offs (rare)
should use a project-local alias instead.

## 3. Operator setup — apply the DCO gate via `gh api`

The DCO check is a GitHub Actions workflow (`.github/workflows/dco.yml`)
that produces a status check named `dco`. To make it block merges, add
`dco` to the branch-protection ruleset's `required_status_checks.contexts`
list on `main`.

**Order of operations:**

1. Land the `dco.yml` workflow in a PR (this milestone's PR does that).
2. Wait for the workflow to produce at least one run on `main`. GitHub's
   branch-protection API rejects `required_status_checks.contexts`
   entries that the API has never observed on the branch.
3. Apply the branch-protection payload below. Run from the repository
   root, with `gh` authenticated as a repository admin.

```bash
# Resolve owner/repo from the local git remote (assumes `origin` is
# the canonical remote on GitHub).
REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner)

# Read the current ruleset, append `dco` to required_status_checks.contexts,
# and PUT the updated ruleset back. Idempotent — re-running with `dco`
# already present is a no-op.
gh api "/repos/${REPO}/branches/main/protection" \
  --jq '.required_status_checks.contexts + ["dco"] | unique' \
  > /tmp/contexts.json

gh api \
  --method PUT \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  "/repos/${REPO}/branches/main/protection" \
  --input - <<EOF
{
  "required_status_checks": {
    "strict": true,
    "contexts": $(cat /tmp/contexts.json)
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
  "required_signatures": false
}
EOF
```

Note that `required_signatures` is `false` — this is the DCO posture.
Setting it to `true` would layer GitHub-enforced GPG/S-MIME signature
verification on top of the DCO check, which is operationally heavier
than the project has committed to. See §4 for how to flip that flag if
you're upgrading a repo to signed-commits.

The remainder of the payload mirrors the C.5 T4 branch-protection
setup verbatim (see `docs/ci/branch-protection.md`); the operator can
apply both in one go by merging the two payloads' `required_status_checks.contexts`
lists.

### Web UI equivalent

For operators who prefer point-and-click, the equivalent settings are
applied via the repository's Settings → Branches → Branch protection
rules → `main` page:

1. **Require status checks to pass before merging**: ensure it is
   enabled (already set per `docs/ci/branch-protection.md`).
2. **Status checks that are required**: search for `dco` and add it.
   It must appear in the search results (the workflow has already
   produced at least one run on `main`).
3. **Require signed commits**: leave **disabled** for DCO posture; see
   §4 to enable for signed-commits posture instead.

### Verification

```bash
REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner)

gh api "/repos/${REPO}/branches/main/protection" \
  --jq '.required_status_checks.contexts | index("dco")'
```

Expected output: a non-null integer (the array index of `dco`). A
`null` output means the branch-protection ruleset does not include
the DCO check — re-run the PUT above.

## 4. Alternative — signed commits (per-repo opt-in)

To use signed commits on a repository instead of DCO:

1. **Drop the `dco` check from required_status_checks.contexts.** Use
   the same `gh api` PUT as §3 with `dco` removed from the contexts
   list. The `dco.yml` workflow can stay in the repo (it will still
   run on PRs, just non-blocking); deleting it is the cleaner choice if
   you're committing to signed-commits long-term.

2. **Set `required_signatures: true` in the branch-protection payload.**
   GitHub then verifies every commit on `main` has a valid GPG, SSH,
   or S/MIME signature.

3. **Document the contributor workflow** for the affected repo:

   - **SSH-signed commits** (recommended for new contributors —
     reuses the SSH key they already have for `git push`):
     ```bash
     git config --global commit.gpgsign true
     git config --global gpg.format ssh
     git config --global user.signingkey ~/.ssh/id_ed25519.pub
     ```
     Then upload the same SSH key to GitHub as a *signing key* (separate
     from the *authentication* key, even if they're the same key
     material): GitHub → Settings → SSH and GPG keys → New SSH key →
     key type "Signing Key".

   - **GPG-signed commits** (heavier-weight, but works with any git
     hosting and any signing-verification tool):
     ```bash
     gpg --full-generate-key  # ed25519 recommended
     gpg --armor --export <key-id>  # upload to GitHub → GPG keys
     git config --global commit.gpgsign true
     git config --global user.signingkey <key-id>
     ```

4. **Auto-remediation bot impact.** A GitHub App that authors commits
   under signed-commits posture must hold a signing key and sign each
   commit it produces. The `barista-security-bot` App's design (see
   the C.5 T4 guardrails YAML's `identity` block) does NOT yet include
   a signing key; switching a repo to signed-commits before the App is
   provisioned with a key will cause every auto-remediation PR to be
   blocked from merge on `main`. Provision the App's signing key
   first, then flip the flag.

## 5. Auto-remediation bot interaction

The auto-remediation workflow (`.github/workflows/auto-remediate.yml`)
runs the Claude Code action under a GitHub App identity (today:
`GITHUB_TOKEN`; once C.5 T4's bot identity lands:
`barista-security-bot[bot]`). The agent's commits must satisfy the
DCO check on `main` just like any contributor's commits — the
guardrails YAML's `pull_request.agent_is_exempt_from_review: false`
declares this property; the DCO check is one of the gates that property
materially enforces.

Two configuration surfaces handle the bot's sign-off:

1. **Commit author identity.** The App's installation token authors
   commits with the configurable author identity:

   ```
   Author:  barista-security-bot[bot] <<app-id>+barista-security-bot[bot]@users.noreply.github.com>
   ```

   This identity is set once on the App's GitHub configuration page;
   the agent's prompt instructs it to commit under this identity for
   every fix it proposes. Recorded as a forward-pointer in the
   guardrails YAML's `identity.bot_login`.

2. **Commit-message templating.** The agent's prompt (`.github/security-agent/prompt.md`)
   instructs the agent to append a `Signed-off-by:` trailer to every
   commit it authors. The trailer's email MUST match the App's author
   email above; the agent's commit template is:

   ```
   <conventional-commit subject>

   <body>

   Fixes #<issue-number>
   Signed-off-by: barista-security-bot[bot] <<email-from-App-config>>
   ```

   This is the "two-strikes-and-stop" + commit-author rules from the
   C.5 T3 agent prompt. The DCO check then passes for the agent's
   commits the same way it passes for human commits.

When C.5 T4's bot identity is provisioned, the operator must:

1. Set the App's commit author email to the `<app-id>+...@users.noreply.github.com`
   form GitHub generates.
2. Update `.github/security-agent/prompt.md` to bake that exact email
   into the `Signed-off-by:` trailer template. (The current prompt
   already covers the convention; the email is the part that has to
   match the live App.)
3. Verify by triggering a synthetic auto-remediation run on a
   non-`main` branch and inspecting the resulting commit's trailer
   with `git log -1 --format='%(trailers:key=Signed-off-by)'`.

If the bot's commits are missing the sign-off, the DCO check fails the
PR and the workflow's two-strikes-and-stop policy will halt the agent
after the second consecutive CI failure — which is the desired
defense-in-depth: a bot misconfiguration cannot get fixes onto `main`.

## 6. Per-repo apply order

The DCO policy applies to all four public repos. Suggested apply
order:

1. **`buildwithbarista/barista`** (this repo). Highest-volume PR
   surface and the one the auto-remediation bot opens PRs against.
   Land DCO here first so the contributor docs are accurate before
   external contributors arrive.

2. **`buildwithbarista/barista.build`** (marketing site +
   documentation). Lower PR volume but the same contributor surface;
   DCO is cheap to add and keeps the policy consistent across repos.

3. **`buildwithbarista/homebrew-tap`** (Homebrew formula). Almost
   entirely bot-authored content (a release-cut workflow updates the
   formula on each tag). The bot identity that updates the tap must be
   configured to sign off; otherwise this repo's DCO gate blocks every
   release.

4. **`buildwithbarista/barista-bench-site`** (benchmarks site). Lowest
   PR volume; apply last when steady state on the other three is
   confirmed.

**Scoping note:** the workflow file (`.github/workflows/dco.yml`), the
CODEOWNERS file (`.github/CODEOWNERS`), this document, and the
`scripts/check-dco.sh` helper live only in `buildwithbarista/barista`
under this milestone. Replicating the pattern to the other three repos
is a sibling operator task — copy the four files verbatim (adjusting
team names in CODEOWNERS if those repos have different owner sets) and
apply the §3 `gh api` payload per repo. No code changes to the other
repos are required.

## 7. Re-running this setup

The DCO setup is idempotent:

- Re-running the `gh api` PUT with `dco` already in the contexts list
  produces the same state.
- The `dco.yml` workflow itself runs on every PR push; no operator
  action is needed for ongoing operation.

Re-apply after:

- A new public repo is added under `buildwithbarista/` — copy the
  workflow + CODEOWNERS + this doc to the new repo, then apply §3.
- The `dco` job's `name:` field changes (it must not — that would
  rename the required status check and block every PR until branch
  protection is re-applied).
- The `barista-security-bot` GitHub App is created or its commit
  author email changes — update the prompt's sign-off template per
  §5.

## 8. Operator checklist

Before declaring DCO live on `main`, confirm:

- [ ] `.github/workflows/dco.yml` is on `main` and has produced at
      least one successful run.
- [ ] `dco` appears in
      `gh api /repos/<owner>/<repo>/branches/main/protection --jq '.required_status_checks.contexts'`.
- [ ] `required_signatures` is `false` in the same payload (DCO posture).
- [ ] `CONTRIBUTING.md`'s "Sign-off" section is current (already lands
      with the M0.1 Task 9 content; this milestone does not edit it).
- [ ] At least one human PR has merged through the DCO gate (smoke
      test that the wiring is end-to-end correct, not just configured).
- [ ] If the auto-remediation bot identity exists: a synthetic
      bot-authored PR has merged through the DCO gate.
