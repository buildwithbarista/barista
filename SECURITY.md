# Security policy

## Reporting a vulnerability

Please report security vulnerabilities by email to **security@buildwithbarista.io**.

**Do not file a public GitHub issue for security reports.** Public issues are not
appropriate for vulnerabilities that have not yet been disclosed.

You should expect an acknowledgement within **72 hours** of your initial report. If you do
not receive a reply within that window, please follow up — your message may have been
filtered.

> The contact address above is a placeholder and will be confirmed before the v0.1 GA
> release. A PGP key for encrypted reports will be published at the same time.

## What to include

A useful report typically contains:

- Affected version (commit SHA, branch, or release tag if applicable).
- A clear description of the issue and its impact.
- Reproduction steps, a proof-of-concept, or relevant logs.
- Your assessment of severity if you have one.
- Your contact information.
- Whether you would like to be credited publicly when the issue is disclosed.

## Supported versions

Barista is pre-release. Only the most recent `0.1.x-alpha` build is supported for
security fixes. Once `0.1.0` ships, a supported-versions table will be added here and
published advisories will list affected versions.

## Disclosure policy

Barista follows **coordinated disclosure**. After receiving a report, the maintainers
will:

1. Acknowledge receipt within 72 hours.
2. Investigate and confirm the issue.
3. Develop and test a fix on a private branch.
4. Agree on a public disclosure timeline with the reporter — typically within 90 days of
   the initial report, sooner for actively exploited issues.
5. Publish a GitHub Security Advisory, release the fix, and add a `CHANGELOG.md` entry.

If you would like credit, the advisory will name you (or an alias of your choice).

## PGP key

A PGP key for encrypted reports will be published before the v0.1 GA release. Until then,
please use the email address above; if your report contains sensitive material, contact a
maintainer first to arrange an alternative encrypted channel.
