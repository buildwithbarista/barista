#!/usr/bin/env bash
#
# m40-t1 daemon-justification spike (Q10 / R2)
# ---------------------------------------------
# 1. Measure baseline `mvn install` wall time (cold + warm) on the 5-plugin
#    sample project — this is the "no daemon, no classloader cache" baseline.
# 2. Resolve each of the 5 pinned plugin coords + transitive deps into
#    .plugin-cache/ via `mvn dependency:copy-dependencies`.
# 3. Build and run PluginCacheBench.java which times two scenarios on the
#    cached jars:
#      (a) cold:  fresh URLClassLoader per iteration
#      (b) warm:  cached URLClassLoader map keyed by plugin coord
#    5 iterations each; drop iter 0 as warmup; average the rest.
# 4. Emit a plain-text results block. The ADR consumes these numbers.
#
# Run from anywhere; paths are resolved relative to this script.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Pin the toolchain across subshells that run in tmpdirs (e.g. the per-plugin
# resolver pom below). The values mirror this repo's .tool-versions and let
# asdf shims pick the right binary regardless of cwd.
export ASDF_JAVA_VERSION="${ASDF_JAVA_VERSION:-temurin-21.0.4+7.0.LTS}"
export ASDF_MAVEN_VERSION="${ASDF_MAVEN_VERSION:-3.9.9}"

SAMPLE="$SCRIPT_DIR/sample-project"
PCACHE="$SCRIPT_DIR/.plugin-cache"
RESULTS="$SCRIPT_DIR/results.txt"

# Pinned plugin coords (must match sample-project/pom.xml).
PLUGINS=(
  "org.apache.maven.plugins:maven-resources-plugin:3.3.1"
  "org.apache.maven.plugins:maven-compiler-plugin:3.13.0"
  "org.apache.maven.plugins:maven-surefire-plugin:3.5.5"
  "org.apache.maven.plugins:maven-jar-plugin:3.4.2"
  "org.apache.maven.plugins:maven-install-plugin:3.1.4"
)

log() { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }

# ---------------------------------------------------------------------------
# Step 1: baseline mvn install wall times.
# ---------------------------------------------------------------------------
log "Step 1: baseline mvn timings"

time_ms() {
  local t0 t1
  t0=$(python3 -c 'import time; print(time.time_ns())')
  ( cd "$SAMPLE" && mvn -q -B install >/dev/null )
  t1=$(python3 -c 'import time; print(time.time_ns())')
  printf '%d' $(( (t1 - t0) / 1000000 ))
}

# Three cold + three warm iterations to get a stable median.
# "Cold" = fresh `mvn clean` target, JVM cold every invocation (no daemon).
# "Warm" = artifacts already installed; mvn still spawns a fresh JVM per
# invocation (no mvnd, no barback), which is the *exact* scenario the
# daemon optimisation targets.
log "  baseline cold (3 iterations of clean + install)"
COLDS=()
for i in 1 2 3; do
  ( cd "$SAMPLE" && mvn -q -B clean >/dev/null )
  ms=$(time_ms)
  COLDS+=("$ms")
  log "    cold[$i]=${ms}ms"
done

log "  baseline warm (3 iterations of install on built tree)"
WARMS=()
for i in 1 2 3; do
  ms=$(time_ms)
  WARMS+=("$ms")
  log "    warm[$i]=${ms}ms"
done

median3() {
  local a=$1 b=$2 c=$3
  printf '%s\n%s\n%s\n' "$a" "$b" "$c" | sort -n | sed -n '2p'
}
COLD_MS=$(median3 "${COLDS[0]}" "${COLDS[1]}" "${COLDS[2]}")
WARM_MS=$(median3 "${WARMS[0]}" "${WARMS[1]}" "${WARMS[2]}")

# ---------------------------------------------------------------------------
# Step 2: populate plugin cache with each plugin + transitive deps.
# ---------------------------------------------------------------------------
log "Step 2: resolving plugin jars into .plugin-cache/"
mkdir -p "$PCACHE"

# We use dependency:copy-dependencies against a tiny throwaway pom so we get
# all transitive deps of each plugin into one flat directory per plugin.
copy_plugin() {
  local coord=$1
  local g a v
  IFS=: read -r g a v <<<"$coord"
  local outdir="$PCACHE/$a-$v"
  mkdir -p "$outdir"
  if [ -f "$outdir/.done" ]; then
    log "    cache hit: $coord"
    return
  fi
  log "    copying: $coord -> $outdir"
  local tmp="$(mktemp -d)"
  cat > "$tmp/pom.xml" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>spike.m40t1.resolver</groupId>
  <artifactId>resolver-$a</artifactId>
  <version>1.0.0</version>
  <packaging>pom</packaging>
  <dependencies>
    <dependency>
      <groupId>$g</groupId>
      <artifactId>$a</artifactId>
      <version>$v</version>
    </dependency>
  </dependencies>
</project>
EOF
  ( cd "$tmp" && mvn -q -B \
      dependency:copy-dependencies \
      -DincludeScope=runtime \
      -DoutputDirectory="$outdir" \
      -DoverWriteSnapshots=false \
      -DoverWriteReleases=false ) >/dev/null
  # Copy the plugin's own main jar (copy-dependencies only ships *deps*).
  local plugin_jar
  plugin_jar=$(find ~/.m2/repository -path "*/$g/$a/$v/$a-$v.jar" -type f 2>/dev/null | head -1)
  if [ -z "$plugin_jar" ]; then
    # ~/.m2 layout uses $g converted from dots to slashes
    local gpath=${g//./\/}
    plugin_jar=$(ls ~/.m2/repository/$gpath/$a/$v/$a-$v.jar 2>/dev/null | head -1 || true)
  fi
  if [ -z "$plugin_jar" ] || [ ! -f "$plugin_jar" ]; then
    log "    ERROR: could not locate main plugin jar for $coord"
    exit 1
  fi
  cp "$plugin_jar" "$outdir/"
  rm -rf "$tmp"
  touch "$outdir/.done"
}

for p in "${PLUGINS[@]}"; do
  copy_plugin "$p"
done

# Stage a shared "core API" classpath that every plugin realm sees as parent.
# This mirrors what real Maven puts on the core realm: maven-plugin-api,
# maven-core, maven-model, etc. Without these, every Mojo class fails to link
# (NoClassDefFoundError on AbstractMojo etc.) and the bench can't measure a
# linkage cost. Versions match what mvn 3.9.9 ships with internally.
CORE="$PCACHE/_core"
mkdir -p "$CORE"
if [ ! -f "$CORE/.done" ]; then
  log "  staging core API jars"
  CORE_TMP="$(mktemp -d)"
  cat > "$CORE_TMP/pom.xml" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>spike.m40t1.core</groupId>
  <artifactId>core-resolver</artifactId>
  <version>1.0.0</version>
  <packaging>pom</packaging>
  <dependencies>
    <!-- The Mojo API + supporting model classes Mojos extend. -->
    <dependency>
      <groupId>org.apache.maven</groupId>
      <artifactId>maven-plugin-api</artifactId>
      <version>3.9.9</version>
    </dependency>
    <dependency>
      <groupId>org.apache.maven</groupId>
      <artifactId>maven-core</artifactId>
      <version>3.9.9</version>
    </dependency>
    <dependency>
      <groupId>org.apache.maven</groupId>
      <artifactId>maven-model</artifactId>
      <version>3.9.9</version>
    </dependency>
    <dependency>
      <groupId>org.apache.maven</groupId>
      <artifactId>maven-artifact</artifactId>
      <version>3.9.9</version>
    </dependency>
    <dependency>
      <groupId>org.apache.maven.plugin-tools</groupId>
      <artifactId>maven-plugin-annotations</artifactId>
      <version>3.13.1</version>
    </dependency>
  </dependencies>
</project>
EOF
  ( cd "$CORE_TMP" && mvn -q -B \
      dependency:copy-dependencies \
      -DincludeScope=compile \
      -DoutputDirectory="$CORE" \
      -DoverWriteSnapshots=false \
      -DoverWriteReleases=false ) >/dev/null
  rm -rf "$CORE_TMP"
  touch "$CORE/.done"
fi
log "  core API jars staged: $(find "$CORE" -name '*.jar' -type f | wc -l | tr -d ' ') jars"

# Emit a manifest the Java bench reads. Each line lists the plugin's coord,
# then the plugin-local jars, then the core jars. The Java bench treats them
# uniformly as a flat URL list.
MANIFEST="$PCACHE/manifest.txt"
: > "$MANIFEST"
CORE_JARS=$(find "$CORE" -name '*.jar' -type f | sort | tr '\n' ' ')
for p in "${PLUGINS[@]}"; do
  IFS=: read -r g a v <<<"$p"
  outdir="$PCACHE/$a-$v"
  jars=$(find "$outdir" -name '*.jar' -type f | sort | tr '\n' ' ')
  printf '%s\t%s%s\n' "$p" "$jars" "$CORE_JARS" >> "$MANIFEST"
done
log "  manifest written: $MANIFEST"

# ---------------------------------------------------------------------------
# Step 3: compile and run PluginCacheBench.
# ---------------------------------------------------------------------------
log "Step 3: compiling and running PluginCacheBench"
javac -d "$SCRIPT_DIR/build" "$SCRIPT_DIR/PluginCacheBench.java"

BENCH_OUT=$(java -cp "$SCRIPT_DIR/build" \
  com.bluminal.barista.barback.spike.PluginCacheBench "$MANIFEST")

# ---------------------------------------------------------------------------
# Step 4: print combined results.
# ---------------------------------------------------------------------------
log "Step 4: results"
{
  echo "# m40-t1 spike results"
  echo "# Q10 (plugin classloader cache speedup) / R2 (daemon scope)"
  echo "# generated $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "#"
  echo "# Environment"
  echo "host.os=$(uname -srm)"
  echo "host.cpu=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)"
  echo "host.cores=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo unknown)"
  echo "host.mem_bytes=$(sysctl -n hw.memsize 2>/dev/null || echo unknown)"
  echo "java.version=$(java -version 2>&1 | head -1)"
  echo "maven.version=$(mvn -v 2>&1 | head -1)"
  echo
  echo "# Baseline (mvn install on 5-plugin sample, 3 modules, fresh JVM per invocation)"
  echo "baseline.cold_iters_ms=[${COLDS[0]}, ${COLDS[1]}, ${COLDS[2]}]"
  echo "baseline.warm_iters_ms=[${WARMS[0]}, ${WARMS[1]}, ${WARMS[2]}]"
  echo "baseline.cold_install_median_ms=$COLD_MS"
  echo "baseline.warm_install_median_ms=$WARM_MS"
  echo
  echo "# Plugin classloader bench (5 plugins, 5 iterations, iter 0 = warmup)"
  echo "$BENCH_OUT"
} | tee "$RESULTS"

log "done. results.txt written."
