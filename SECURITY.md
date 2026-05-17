# Security policy

Thanks for helping keep Barista and its users safe. If you believe you have found a
security vulnerability in Barista, please report it privately through one of the channels
below — **not** as a public GitHub issue, pull request, or discussion.

- **Preferred:** [GitHub Private Vulnerability Reports][gh-pvr] on this repository.
- **Fallback (reporters without a GitHub account):** email
  **security@buildwithbarista.io**.

You should receive an acknowledgement within **three business days** of your initial
report, and an initial triage assessment within **seven business days**. If you do not, please
follow up — your message may have been filtered.

[gh-pvr]: https://github.com/buildwithbarista/barista/security/advisories/new

> **Pre-release placeholders.** Barista is pre-release software. Until the v0.1 GA release,
> the **security@buildwithbarista.io** address and any PGP key fingerprint listed below are
> placeholders pending final project-owner confirmation. The GitHub Private Vulnerability
> Reports channel is authoritative in the interim. Service-level expectations in this
> document are reasonable defaults proposed by the maintainers and are subject to
> founder ratification before the first public push.

## Supported versions

Barista is pre-1.0. While `0.x` releases are in flight, only the **latest published `0.1.x`**
receives security fixes. Once a later minor (`0.2.x`, `0.3.x`, …) is published, the previous
minor enters a **90-day grace period** during which critical security fixes will still be
backported on a best-effort basis. After that window closes, the only supported branch is the
current minor.

| Version             | Supported                                    |
|---------------------|----------------------------------------------|
| `0.1.x` (latest)    | Yes — current pre-release line               |
| `0.1.x` (older patch)| Upgrade to the latest `0.1.x`               |
| Pre-`0.1` snapshots | No                                           |

When `1.0.0` ships, this section will be replaced with a stable-release support policy
(at minimum: the current major + the previous major for a published deprecation window).

## Reporting a vulnerability

### Primary channel — GitHub Private Vulnerability Reports

GitHub Private Vulnerability Reports (PVR) is the preferred intake. PVR creates a private
advisory draft that maintainers can collaborate on with you, lets us request a CVE through
GitHub when warranted, and produces the published advisory automatically once the embargo
lifts.

To open one:

1. Visit <https://github.com/buildwithbarista/barista/security/advisories/new>
   (or the "Security" tab → "Report a vulnerability" on this repository).
2. Fill in the summary, severity, and affected components.
3. Attach a reproducer or proof-of-concept in the description or as a private file.

You will be added as a collaborator on the draft advisory and can continue the conversation
there.

### Fallback channel — email

If you do not have a GitHub account, or PVR is otherwise unavailable, email the report to
**security@buildwithbarista.io**. Use the subject line `SECURITY: <short summary>` so the
message routes correctly.

### What to include

A useful report typically contains:

- **Affected version** — release tag, commit SHA, or branch.
- **Affected component** — `barista` CLI, `barback` daemon, `roastery` cache server, a
  specific Rust crate (e.g. `barista-coords`), the Maven compatibility layer, build
  artifacts, or the Homebrew tap.
- **Environment** — operating system, JDK version, Maven version (where relevant).
- **Impact** — what an attacker can do with this, who is exposed, and any preconditions
  (network position, local file access, malicious dependency, etc.).
- **Reproduction steps** — a clear sequence, ideally with a minimal proof-of-concept that
  a maintainer can run.
- **Severity** — your own assessment if you have one (CVSS v3.1 vector is welcome but
  not required).
- **Contact** — how to reach you for follow-up.
- **Credit preference** — whether you would like to be credited publicly when the
  advisory is published, and the name or alias to use.

If your report contains sensitive material (e.g. an exploit payload that targets a third
party), say so up front and we will agree on an encrypted channel before you send it.

## What happens after you report

1. **Acknowledgement.** A maintainer acknowledges receipt within three business days.
2. **Triage.** Within seven business days, you receive an initial assessment — whether we
   confirm the issue, what severity we have provisionally assigned, and what we need from
   you next.
3. **Investigation.** Maintainers reproduce the issue, assess scope (which versions,
   components, and platforms are affected), and develop a fix on a private branch or in
   a GitHub Security Advisory workspace.
4. **Fix development.** Once a fix is ready, we coordinate the release plan with you —
   release window, advisory text, and credit. If a CVE identifier is appropriate, we
   request one through GitHub's CVE Numbering Authority.
5. **Public disclosure.** On the agreed date, we publish the GitHub Security Advisory,
   release the fix, and add an entry to [`CHANGELOG.md`](CHANGELOG.md). The advisory
   links to the fix commits and lists affected versions.
6. **Post-disclosure.** If you opted in to recognition, you appear in the advisory's
   credits and in the project's hall of fame (see [Recognition](#recognition) below).

The SLAs above are reasonable defaults; they will be re-confirmed by the project owners
before the first public push.

### When we request a CVE

We will request a CVE identifier for any confirmed vulnerability that:

- affects a published release (or a `main` branch revision that downstream packagers may
  have shipped), and
- is exploitable in a default or commonly recommended configuration, and
- requires a remediation step by users (patch, configuration change, or version bump).

Hardening improvements that close a theoretical gap without a known exploit path, or issues
that only affect unreleased development branches, are generally fixed without a CVE but will
still receive an advisory if disclosure is useful to downstream consumers.

## Embargo and disclosure policy

Barista follows **coordinated disclosure** with a target embargo of **90 days** from the
date of acknowledgement to public disclosure. This window gives maintainers time to develop,
test, and ship a fix; it gives downstream packagers (Homebrew, distribution maintainers,
Docker image publishers) time to pick the fix up; and it gives users time to upgrade before
the issue is publicly known.

- **Standard embargo.** 90 days from acknowledgement, or upon publication of a fixed
  release, whichever comes first.
- **Extensions.** We will consider an extension if the fix is unusually invasive, requires
  coordination across multiple upstream projects, or depends on a downstream release window
  (for example, a Maven Central change). Extensions are agreed in writing with the
  reporter and capped at an additional 90 days unless there are exceptional
  circumstances.
- **Accelerated disclosure.** We reserve the right to disclose earlier than the agreed
  date if:
  - there is credible evidence the issue is being actively exploited in the wild;
  - the issue has been disclosed publicly by a third party (intentionally or
    inadvertently) and silence would put more users at risk than a coordinated
    announcement;
  - a fix has shipped and the embargo no longer serves users.
- **Reporter early-disclosure.** Reporters are asked not to disclose the issue publicly
  before the agreed date. If you have a hard external deadline (an academic publication,
  a conference talk, a regulatory filing), tell us early — we will work with you to align
  the public release with that date wherever we can.

## Out of scope

The following are generally **not** treated as security vulnerabilities, though good-faith
reports are still welcome and we will route them to the right place:

- **Bugs in upstream dependencies.** If the root cause is in a third-party crate, Maven
  plugin, or runtime library, please report to that project. We track upstream
  advisories through our SCA suite (see [CONTRIBUTING.md][contrib-sca]) and will pick
  up a fix in a coming release; if you have already filed an upstream report and want us
  to expedite, tell us.
- **Denial-of-service via unbounded input.** Build tools execute user-controlled
  configuration (POMs, plugin code, scripts). A build that hangs or exhausts memory on a
  hostile `pom.xml` or a malicious plugin is, in general, a bug rather than a
  vulnerability. We will fix bugs of this shape on the normal track. The exception is
  when an attacker can trigger the resource exhaustion **without** the victim explicitly
  invoking that build target (e.g. through a hook fired during dependency resolution of a
  benign target); those reports go through this policy.
- **Social engineering** of project maintainers, the project's accounts on third-party
  services, or contributors. Please report to the platform involved.
- **Self-XSS, missing security headers on non-sensitive docs pages,** and other
  low-impact web findings on the project's marketing or documentation sites, unless
  they materially affect users of the build tool itself.
- **Reports generated solely by automated scanners** with no exploitation analysis. We
  appreciate scanner output, but please include a description of how the finding is
  reachable in practice.

If you are unsure whether something is in scope, err on the side of reporting it — we would
rather triage one extra report than miss a real issue.

[contrib-sca]: CONTRIBUTING.md#dependency-scanning

## Recognition

Barista does not currently operate a paid bug bounty. (This may change after `1.0.0`; the
v0.1 line is research and stabilization, not a funded reward program.)

We do offer **public recognition** for reporters who follow this policy and contribute
materially to a fix:

- **Advisory credit.** Every published GitHub Security Advisory names its reporter (or
  the reporter's chosen alias) unless the reporter asks to remain anonymous.
- **Changelog credit.** The corresponding entry in [`CHANGELOG.md`](CHANGELOG.md) names
  the reporter for advisories with a CVE.
- **Hall of fame.** Once there is a first recipient, the repository will publish a
  `SECURITY-HALL-OF-FAME.md` file listing reporters who have opted into public credit,
  with the advisory ID and a short description of the contribution. The file does not
  exist yet; it will be created with the first credited disclosure.

Credit is opt-in. If you prefer to remain anonymous, tell us in your initial report and we
will treat your identity as confidential.

## Cryptographic verification of releases

Release artifacts published by the project are signed. Verification instructions, including
the public-key fingerprint(s) used to sign each artifact type and the recommended
verification command, are published with each release on the
[GitHub releases page](https://github.com/buildwithbarista/barista/releases).

Until the first signed release ships, this section is informational only.

## Related policies and references

- [`CONTRIBUTING.md`](CONTRIBUTING.md) — general contribution process, including the local
  security checks (`gitleaks`, `semgrep`, `cargo-deny`, `cargo-audit`) every contributor is
  asked to run before pushing.
- [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md) — community standards and the separate
  conduct-reporting channel (do **not** use the conduct channel for security issues).
- [`docs/ci/secret-scan-allowlist.md`](docs/ci/secret-scan-allowlist.md) — operational
  policy for `.gitleaksignore` waivers (how false-positive secret-scan findings are handled
  before a real credential ever has to enter the allowlist).

## Policy revisions

This policy is a living document. Material changes — supported-version ranges, embargo
defaults, contact channels — will be announced in `CHANGELOG.md` and (once the project is
public) on the project's release blog. Editorial changes (wording, formatting, link fixes)
will land without a separate announcement.
