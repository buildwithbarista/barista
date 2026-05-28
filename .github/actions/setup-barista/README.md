# setup-barista

A GitHub Action that downloads, verifies, and installs the [Barista](https://barista.build)
CLI so later steps can call `barista` directly.

It resolves the right release archive for the runner (Linux and macOS, x86-64
and arm64), verifies the archive's `sha256` against the release's signed build
manifest, extracts it with the bundled Maven 4 and barback worker intact, and
adds `barista` to `PATH`.

## Usage

```yaml
steps:
  - uses: actions/checkout@v6

  # Barista runs builds on the JVM, so a JDK 17+ must be present.
  - uses: actions/setup-java@v5
    with:
      distribution: temurin
      java-version: "21"

  - uses: buildwithbarista/barista/.github/actions/setup-barista@v1
    with:
      version: latest # or a pinned version, e.g. 0.1.0-alpha.1

  - run: barista verify
```

This action installs only the CLI. Pair it with `actions/setup-java` (as above)
for any command that runs a build (`verify`, `package`, …); `barista --version`
needs no JDK.

## Inputs

| Input          | Default                   | Description                                                                 |
| -------------- | ------------------------- | --------------------------------------------------------------------------- |
| `version`      | `latest`                  | Release version without the leading `v` (e.g. `0.1.0-alpha.1`), or `latest` (prereleases included). |
| `repository`   | `buildwithbarista/barista`| `owner/repo` to fetch releases from.                                        |
| `github-token` | `${{ github.token }}`     | Token for the releases API call (avoids unauthenticated rate limits).       |

## Outputs

| Output        | Description                                                  |
| ------------- | ------------------------------------------------------------ |
| `version`     | The resolved Barista version that was installed.             |
| `install-dir` | Directory Barista was installed into (contains `bin/`, `share/`). |

## Supply-chain verification

Every archive's `sha256` is checked against the release's `build-manifest.json`
before extraction. For full signature verification, the release also ships
Sigstore `cosign` bundles and SLSA provenance — verify those with `cosign` /
`slsa-verifier` in a dedicated step if your threat model requires it.

## Notes

- Windows runners are not supported yet (the v0.1 release ships Windows
  archives, but this action handles the `tar.gz` Linux/macOS targets for now).
- The CLI finds its bundled Maven 4 (`share/barista/maven-4`) and barback
  (`share/barista/barback-uber.jar`) relative to its own location, so the
  install tree is kept intact — do not move `bin/barista` away from `share/`.
