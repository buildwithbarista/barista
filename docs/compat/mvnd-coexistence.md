<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Barista and mvnd: coexistence and positioning

This document explains how Barista relates to the Maven daemon (`mvnd`):
whether they can be installed side by side (yes), how they share state on
disk, where Barista borrows `mvnd`'s ideas, and how to choose between them.

## TL;DR

- **They coexist cleanly.** Barista and `mvnd` are different binaries with
  different daemons; installing one does not interfere with the other.
- **They share `~/.m2/repository` safely.** Artifacts `mvnd` (or plain `mvn`)
  downloads land in `~/.m2/repository` as usual; the next Barista invocation
  that touches them ingests them into Barista's content-addressed cache with
  checksum verification. Artifacts Barista resolves are written back to
  `~/.m2/repository` so `mvnd`/`mvn` see them too.
- **You do not have to choose.** Keep `mvnd` installed; reach for whichever
  fits the moment. Barista is a `mvn`-compatible drop-in, so the same project
  builds under `mvn`, `mvnd`, and `barista` without project changes.

## What `mvnd` is, and what Barista shares with it

`mvnd` (the Maven Daemon) keeps a long-lived JVM warm across builds so you
don't pay cold-JVM startup, Plexus container init, and classloader rebuild on
every invocation. Its measured wins come from JIT warmup and from caching
plugin classloaders between builds.

Barista's **barback** daemon adopts the same core idea — a long-lived JVM with
a warm worker pool and a plugin-classloader cache — but with one structural
difference: barback is a **worker for an external orchestrator** (the Barista
CLI, written in Rust) rather than a complete build tool. The slow,
JVM-unfriendly work that dominates a Maven invocation — dependency resolution,
cache management, lockfile handling, the network layer — runs in the Rust CLI,
outside the JVM entirely. The JVM is reserved for what genuinely needs Maven's
embedded core: executing mojos.

The practical consequence: Barista is faster than `mvnd` on the parts `mvnd`
still pays JVM cost for (resolution, cache, network), and comparable-to-faster
on warm execution because barback holds workers warm the way `mvnd` does.

## How they share disk state

| State | `mvn` / `mvnd` | Barista | Interaction |
|---|---|---|---|
| Local repository | `~/.m2/repository` | Content-addressed cache (separate) **+** writes resolved artifacts back to `~/.m2/repository` | Bidirectional and safe: `mvnd`-fetched artifacts are ingested into Barista's CAS (checksum-verified) on next touch; Barista-resolved artifacts populate `~/.m2/repository` for `mvnd`/`mvn`. |
| `settings.xml` | `~/.m2/settings.xml` | Read for mirrors/servers/proxies | Shared; Barista honors the same `settings.xml`. |
| Daemon | `mvnd` daemon process | barback daemon process | Independent processes; no shared socket, no contention. |
| Lockfile | none | `barista.lock` (Barista-specific) | `mvnd`/`mvn` ignore `barista.lock`; it is inert to them. |

Because the local repository is shared and Barista verifies checksums on
ingest, a mixed workflow (resolve under `mvnd` today, build under Barista
tomorrow, or vice-versa) does not corrupt either tool's view of the world. A
poisoned or mismatched artifact arriving via `mvnd` is caught by Barista's
content-addressed verification rather than silently trusted (see the
[threat model](../arch/threat-model.md), cache-poisoning section).

## When to use which

- **Use Barista** when you want the fastest cold-cache resolution and the most
  network/compute-efficient builds, a verified lockfile (`barista.lock` +
  `--frozen` for reproducible CI), or the shared remote cache (roastery). It is
  a `mvn` drop-in, so this is the default recommendation for day-to-day builds
  and CI.
- **Keep `mvnd`** if your team already standardizes on it, if you depend on
  `mvnd`-specific behavior, or while you are evaluating Barista. Nothing forces
  a cutover — both can stay installed indefinitely.
- **Plain `mvn`** remains the ground-truth reference; Barista's compatibility
  is defined against it, and every Barista release is benchmarked against
  `mvn 3.9.x`, `mvn 4.0.x`, and `mvnd 2.x`.

## What Barista does NOT do

- It does not replace, wrap, or shim `mvnd`. It is a separate tool that happens
  to be `mvn`-compatible.
- It does not modify `mvnd`'s configuration or daemon state.
- It does not compete with build-output caching (e.g. Develocity's Universal
  Cache) — that is goal-output caching, a different layer. Barista's cache is
  the **artifact** cache. The two are complementary.

## Migration is incremental, never all-or-nothing

Because the project builds identically under all three tools and the local
repository is shared, adoption is per-developer and per-pipeline:

1. Install Barista alongside your existing `mvn`/`mvnd`.
2. Run `barista verify` (or the relevant `mvn`-vocabulary command) on a project
   that already builds clean — the output should match.
3. Adopt Barista where its speed/efficiency/lockfile wins matter (CI cold
   caches, large reactors), and leave `mvnd` in place everywhere else.
4. Commit `barista.lock` when you want reproducible, integrity-checked CI; it
   is invisible to `mvn`/`mvnd`.

There is no flag day, no repository migration, and no lock-in: uninstalling
Barista leaves `~/.m2/repository` and your `mvnd` setup exactly as they were.
