#!/usr/bin/env bash
# Maven 4 embedding spike — build + run harness (M4.0 T2).
#
# Idempotent: extracts the pinned Maven 4.0.0-rc-3 distribution if absent,
# compiles EmbedSpike.java against its boot/+lib/ classpath, runs it against
# sample-project/, and asserts the prototype compile path runs green.
#
# Exit code: 0 if every mode (EMBED-COLD, EMBED-WARM*N, SUBPROC) returns 0
# AND the final line is "OVERALL OK". Non-zero otherwise.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

MAVEN_VERSION="4.0.0-rc-3"
MAVEN_DIST_TARBALL="/tmp/maven-${MAVEN_VERSION}.tar.gz"
# Source of the tarball: https://archive.apache.org/dist/maven/maven-4/${MAVEN_VERSION}/binaries/
# Pre-staged by the spike harness; do not re-download from inside this script.
MAVEN_HOME="${SCRIPT_DIR}/apache-maven-${MAVEN_VERSION}"
FIXTURE="${SCRIPT_DIR}/sample-project"
WARM_ITERS="${WARM_ITERS:-2}"

if [[ ! -d "$MAVEN_HOME" ]]; then
  if [[ ! -f "$MAVEN_DIST_TARBALL" ]]; then
    echo "FATAL: $MAVEN_DIST_TARBALL not found." >&2
    echo "       Expected pre-staged tarball from https://archive.apache.org/dist/maven/maven-4/${MAVEN_VERSION}/binaries/" >&2
    exit 2
  fi
  echo "extracting $MAVEN_DIST_TARBALL -> $MAVEN_HOME"
  tar -C "$SCRIPT_DIR" -xzf "$MAVEN_DIST_TARBALL"
fi

# Verify the expected core JARs are present.
for jar in maven-core-${MAVEN_VERSION}.jar \
           maven-embedder-${MAVEN_VERSION}.jar \
           maven-api-cli-${MAVEN_VERSION}.jar \
           maven-cli-${MAVEN_VERSION}.jar; do
  [[ -f "${MAVEN_HOME}/lib/${jar}" ]] || { echo "FATAL: missing ${jar}" >&2; exit 2; }
done
[[ -f "${MAVEN_HOME}/boot/plexus-classworlds-2.8.0.jar" ]] || { echo "FATAL: missing classworlds boot jar" >&2; exit 2; }

# Build the spike.
CP="${MAVEN_HOME}/boot/*:${MAVEN_HOME}/lib/*"
echo "compiling EmbedSpike.java"
javac --release 21 -classpath "$CP" -d "$SCRIPT_DIR/classes" "$SCRIPT_DIR/EmbedSpike.java"

# Run the spike. We pass --enable-native-access=ALL-UNNAMED to match bin/mvn's
# launcher invocation; Maven 4 uses native syscalls via JLine and would warn
# loudly otherwise.
echo "running EmbedSpike against $FIXTURE (warm iters=$WARM_ITERS)"
exec java \
  --enable-native-access=ALL-UNNAMED \
  -classpath "${SCRIPT_DIR}/classes:${CP}" \
  -Dmaven.home="${MAVEN_HOME}" \
  -Dmaven.installation.conf="${MAVEN_HOME}/conf" \
  -Dmaven.multiModuleProjectDirectory="${FIXTURE}" \
  -Dmaven.mainClass=org.apache.maven.cling.MavenCling \
  -Dlibrary.jline.path="${MAVEN_HOME}/lib/jline-native" \
  EmbedSpike "$MAVEN_HOME" "$FIXTURE" "$WARM_ITERS"
