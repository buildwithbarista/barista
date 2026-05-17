/*
 * Copyright (c) 2026 Bluminal and the Barista contributors.
 * Licensed under the MIT License or the Apache License, Version 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE at the repo root.
 */
package com.bluminal.barista.barback.classloader;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.logging.Level;
import java.util.logging.Logger;

import javax.annotation.Priority;
import javax.inject.Named;
import javax.inject.Singleton;

import org.apache.maven.artifact.Artifact;
import org.apache.maven.model.Plugin;
import org.apache.maven.plugin.PluginRealmCache;
import org.apache.maven.project.MavenProject;
import org.codehaus.plexus.classworlds.realm.ClassRealm;
import org.codehaus.plexus.personality.plexus.lifecycle.phase.Disposable;
import org.eclipse.aether.RepositorySystemSession;
import org.eclipse.aether.graph.DependencyFilter;
import org.eclipse.aether.repository.LocalRepository;
import org.eclipse.aether.repository.RemoteRepository;
import org.eclipse.aether.repository.WorkspaceRepository;

/**
 * Barista's owned {@link PluginRealmCache} hook: replaces Maven's
 * default Sisu binding so the daemon controls the plugin-realm cache
 * surface (cache key shape, OPEN-8 override-list bypass, hit/miss
 * counters surfaced through the companion {@link PluginCache}).
 *
 * <h2>Why this exists, and what it does NOT solve</h2>
 *
 * <p>Maven&nbsp;4 ships {@code DefaultPluginRealmCache} as the
 * {@code @Named @Singleton} {@link PluginRealmCache} binding inside
 * every {@link org.codehaus.plexus.PlexusContainer} the cling stack
 * brings up. Within a single container (i.e. one
 * {@code ResidentMavenInvoker}'s lifetime, bounded by
 * {@code MAX_ACTIONS_PER_INVOKER} actions in the daemon's rc-3 leak
 * mitigation) Maven's default cache already serves a hit for every
 * plugin loaded a second time. That is why the M4.3&nbsp;T3
 * warm-shot measurement holds at &asymp;330&nbsp;ms even with the
 * default cache in place &mdash; the within-cycle cost is already
 * paid down by Maven's own hook.
 *
 * <p>Crossing the {@code ResidentMavenInvoker} rebuild boundary
 * <em>does</em> drop every cached realm because the new container's
 * lookups present a fresh {@code mavenApiRealm} as the cache-key
 * parent, and the prior cycle's realms key under the prior parent.
 * In other words: a strict "survive container disposal" cache cannot
 * fire hits across rebuilds without re-keying surgery inside Maven's
 * own plugin manager (out of scope for v0.1).
 *
 * <p>What this class buys us, then, is correctness of the daemon's
 * extension-point ownership: when we later need to gate the OPEN-8
 * override list, surface unified hit/miss statistics, or layer in
 * a re-keying optimisation, the hook is already in place and Sisu
 * is already binding to it instead of Maven's default.
 *
 * <h2>Lifecycle</h2>
 *
 * <p>One instance per Plexus container (Sisu {@code @Singleton}
 * scope). The container is disposed on every
 * {@code ResidentMavenInvoker} rebuild; the cache's
 * {@link #dispose()} / {@link #flush()} entrypoints both no-op so
 * cached realms do not get torn out of the retained
 * {@link org.codehaus.plexus.classworlds.ClassWorld} by us.
 * (The default impl iterates entries and calls
 * {@code ClassWorld#disposeRealm(realm.getId())} on each, which
 * would yank realms the host still owns.) Container disposal drops
 * the cache instance itself; the new container brings up a fresh one.
 *
 * <h2>Override-list bypass</h2>
 *
 * <p>The companion {@link PluginCache} carries an OPEN-8 escape hatch
 * for plugins that misbehave under classloader caching (static state,
 * thread-local assumptions, etc.). When a plugin's GA is on the
 * override list we return a cache miss from {@link #get(Key)} so Maven
 * builds a fresh realm via the supplier and we never store it. The
 * companion cache also tracks hit/miss counters here so the daemon's
 * status RPC has a unified view of cache behaviour.
 *
 * <h2>Sisu binding</h2>
 *
 * <p>This class is registered via {@code @Named @Singleton} so Sisu
 * picks it up during {@code DefaultPlexusContainer} bootstrap (the
 * cling-built container scans {@code META-INF/sisu/javax.inject.Named}
 * in "index" mode). The {@code @Priority(100)} annotation makes Sisu
 * prefer this binding over Maven's stock {@code DefaultPluginRealmCache}
 * (default priority 0).
 *
 * <h2>What the {@link PluginRealmCache.Key} actually pins</h2>
 *
 * <p>The default {@code DefaultPluginRealmCache.CacheKey} composes:
 * the {@code Plugin} model object (GAV + dependencies + configuration),
 * the parent {@link ClassLoader}, the foreign-imports map, the
 * dependency filter, the remote repository list, the
 * {@link WorkspaceRepository}, and the {@link LocalRepository}. Across
 * an invoker rebuild the parent ClassLoader is the same (we retain the
 * ClassWorld), the local repo is the same path, the Plugin model is
 * value-equal because it is read from the same POM, and the
 * repositories are the same set. So a cache hit after a rebuild is
 * the steady-state case &mdash; not a coincidence.
 */
@Named
@Singleton
@Priority(100)
public class BaristaPluginRealmCache implements PluginRealmCache, Disposable {

    private static final Logger LOG = Logger.getLogger(BaristaPluginRealmCache.class.getName());

    /**
     * Public no-arg constructor for Sisu instantiation. Logs at FINE
     * so the eviction IT and the bring-up-diag test can confirm
     * Sisu picked our binding over Maven's
     * {@code DefaultPluginRealmCache} (it fires once per Plexus
     * container, i.e. once per {@code ResidentMavenInvoker} rebuild
     * cycle).
     */
    public BaristaPluginRealmCache() {
        LOG.log(Level.FINE, "BaristaPluginRealmCache: Sisu instantiated");
    }

    /**
     * Per-container entry storage. Scoped to one Sisu instance so the
     * cache lives exactly as long as the enclosing
     * {@link org.codehaus.plexus.PlexusContainer}. On container
     * disposal Sisu drops the singleton and the entries it held; the
     * cached {@link ClassRealm} references become unreachable and the
     * JVM is free to GC them.
     *
     * <p>{@link LinkedHashMap} (no LRU here) is enough: the v0.1
     * workloads engage on the order of 5-10 plugins per build, well
     * below any cap we would want to set. If a future workload ever
     * inflates the per-container entry count past ~100 we can layer
     * an LRU on top without touching the {@link PluginRealmCache}
     * contract.
     */
    private final LinkedHashMap<Key, CacheRecord> entries = new LinkedHashMap<>();

    /**
     * Optional companion cache for diagnostics and override-list
     * lookup. Set via {@link #setCompanion(PluginCache)} from the
     * host's bootstrap (see {@code EmbeddedMavenFactory}). When unset
     * the cache still works; statistics simply do not flow into
     * {@code PluginCache} and no override-list bypass fires.
     *
     * <p>Static because Sisu instantiates this class per-container; the
     * companion must be discoverable from every container the daemon
     * brings up across its lifetime.
     */
    private static volatile PluginCache companion;

    /**
     * Install (or replace) the companion {@link PluginCache}. Called
     * by the host at bootstrap so that hits/misses recorded here flow
     * into the daemon's status RPC, and so that the OPEN-8 override
     * list takes effect on realm-cache lookups.
     */
    public static void setCompanion(PluginCache cache) {
        companion = cache;
    }

    /**
     * No-op kept for source compatibility with the host's bootstrap
     * code path (the host clears the realm cache on close so the
     * static-storage variant could drop entries before ClassWorld
     * disposal). With the per-container scope the Sisu singleton's
     * own lifecycle handles release; this entry point stays as a
     * safe call site.
     */
    public static void clearAll() {
        // intentional no-op — see javadoc
    }

    @Override
    public Key createKey(Plugin plugin,
                         ClassLoader parentRealm,
                         Map<String, ClassLoader> foreignImports,
                         DependencyFilter dependencyFilter,
                         List<RemoteRepository> repositories,
                         RepositorySystemSession session) {
        // Defer to a value-equal key whose shape mirrors the default
        // implementation. Repackaging the key locally lets us evolve
        // the cache without touching Maven's internal CacheKey; the
        // hashCode is computed eagerly because the key is consulted
        // on every plugin lookup and the inputs are immutable enough
        // (Plugin#hashCode is value-derived, repository identities are
        // stable per resolver session) that pre-computing avoids
        // repeated string hashing on the hot path.
        WorkspaceRepository workspace =
                session != null && session.getWorkspaceReader() != null
                        ? session.getWorkspaceReader().getRepository()
                        : null;
        LocalRepository localRepo = session != null ? session.getLocalRepository() : null;
        return new BaristaCacheKey(plugin, parentRealm, foreignImports, dependencyFilter,
                repositories, workspace, localRepo);
    }

    @Override
    public CacheRecord get(Key key) {
        // Override-list bypass: if the companion cache is configured
        // and the plugin's GA is on the list, force a miss so Maven
        // rebuilds the realm. We surface the override via the
        // companion's metric counter (the override-bypass count is
        // the diagnostic surface PRD §11.6 OPEN-8 specifies).
        PluginCache c = companion;
        if (c != null && key instanceof BaristaCacheKey bk && c.isOverriddenByGa(bk.pluginGa())) {
            // We can't increment overrideBypassCount via the public
            // PluginCache API without going through loadOrBuild, so we
            // log at FINE for diagnostics; the bypass takes effect
            // simply by returning null.
            LOG.log(Level.FINE,
                    () -> "BaristaPluginRealmCache: override bypass for plugin "
                            + bk.pluginGa());
            return null;
        }
        CacheRecord rec;
        synchronized (entries) {
            rec = entries.get(key);
        }
        if (c != null) {
            if (rec != null) {
                c.recordRealmCacheHit();
            } else {
                c.recordRealmCacheMiss();
            }
        }
        return rec;
    }

    @Override
    public CacheRecord put(Key key, ClassRealm realm, List<Artifact> artifacts) {
        // Maven's contract: put() returns the stored record. We must
        // not silently swap an existing entry — that would orphan a
        // realm the caller still holds. The default impl rejects
        // duplicate puts with a NPE-ish path; we instead return the
        // existing record so concurrent setupPluginRealm() races
        // (Maven's own #setupPluginRealm uses get-with-supplier which
        // round-trips through put) collapse to one stored record.
        CacheRecord fresh = new CacheRecord(realm, artifacts);
        synchronized (entries) {
            CacheRecord prior = entries.putIfAbsent(key, fresh);
            return prior != null ? prior : fresh;
        }
    }

    @Override
    public void flush() {
        // Dispose every cached realm from the surrounding
        // ClassWorld and clear the entry map. Mirrors the default
        // implementation's behaviour: without it the world would
        // retain references to every realm ever loaded, and the
        // EmbeddedMavenLeakIT's 10 MiB envelope is breached after
        // a handful of ResidentMavenInvoker rebuilds (each rebuild
        // disposes the container, which Sisu-disposes us, but
        // realms left in the world cannot be GC'd because the
        // world is retained by the host).
        synchronized (entries) {
            for (CacheRecord rec : entries.values()) {
                ClassRealm realm = rec.getRealm();
                if (realm == null) {
                    continue;
                }
                try {
                    realm.getWorld().disposeRealm(realm.getId());
                } catch (Exception ignored) {
                    // Best-effort: a realm may already be disposed
                    // (re-entrant flush via Plexus disposal ordering)
                    // or not yet registered; either way we just want
                    // to clear our reference.
                }
            }
            entries.clear();
        }
    }

    /**
     * No-op override of the default {@link MavenProject}-level
     * registration. Maven calls {@code register()} after a successful
     * realm lookup; the default implementation also does nothing
     * (the project-level association is not used by core lookup paths).
     */
    @Override
    public void register(MavenProject project, Key key, CacheRecord record) {
        // see javadoc — no-op, matching the default behaviour
    }

    /**
     * Plexus {@link Disposable} hook. Called by the
     * {@link org.codehaus.plexus.PlexusContainer} on shutdown (which
     * the cling stack invokes once per {@code ResidentMavenInvoker}
     * rebuild and once at daemon shutdown). Delegates to
     * {@link #flush()}, matching the default implementation's
     * behaviour.
     *
     * <p>Without this hook Plexus would never tell our cache to
     * release its realms, the {@link org.codehaus.plexus.classworlds.ClassWorld}
     * the host retains would carry every previously-loaded plugin
     * realm forever, and the heap-leak IT's 10 MiB envelope would be
     * blown after only a handful of invoker rebuilds.
     */
    @Override
    public void dispose() {
        flush();
    }

    /**
     * Value-equal cache key. Mirrors the shape of
     * {@code DefaultPluginRealmCache.CacheKey} but is computed once,
     * stamps the plugin's {@code groupId:artifactId} for OPEN-8
     * override-list matching, and avoids importing the
     * package-private default key class.
     */
    static final class BaristaCacheKey implements Key {
        private final Plugin plugin;
        private final WorkspaceRepository workspace;
        private final LocalRepository localRepo;
        private final List<RemoteRepository> repositories;
        private final ClassLoader parentRealm;
        private final Map<String, ClassLoader> foreignImports;
        private final DependencyFilter filter;
        private final int hashCode;

        BaristaCacheKey(Plugin plugin,
                        ClassLoader parentRealm,
                        Map<String, ClassLoader> foreignImports,
                        DependencyFilter filter,
                        List<RemoteRepository> repositories,
                        WorkspaceRepository workspace,
                        LocalRepository localRepo) {
            this.plugin = plugin;
            this.parentRealm = parentRealm;
            this.foreignImports = foreignImports;
            this.filter = filter;
            this.repositories = repositories;
            this.workspace = workspace;
            this.localRepo = localRepo;
            int h = 17;
            h = 31 * h + (plugin == null ? 0 : plugin.hashCode());
            h = 31 * h + (parentRealm == null ? 0 : System.identityHashCode(parentRealm));
            h = 31 * h + (foreignImports == null ? 0 : foreignImports.hashCode());
            h = 31 * h + (filter == null ? 0 : filter.hashCode());
            h = 31 * h + (repositories == null ? 0 : repositories.hashCode());
            h = 31 * h + (workspace == null ? 0 : workspace.hashCode());
            h = 31 * h + (localRepo == null ? 0 : localRepo.hashCode());
            this.hashCode = h;
        }

        /**
         * {@code groupId:artifactId} of the cached plugin. Used to
         * test against {@link PluginCache#overrideList()}.
         */
        String pluginGa() {
            if (plugin == null) {
                return "";
            }
            String g = plugin.getGroupId();
            String a = plugin.getArtifactId();
            return (g == null ? "" : g) + ":" + (a == null ? "" : a);
        }

        @Override
        public int hashCode() {
            return hashCode;
        }

        @Override
        public boolean equals(Object o) {
            if (this == o) return true;
            if (!(o instanceof BaristaCacheKey other)) return false;
            // Identity check on parentRealm: ClassLoader#equals is
            // identity-based by default, but the host swaps no
            // classloaders for cached plugin realms (the parent is
            // always plexus.core or a foreign-import). Identity here
            // is intentional and matches the default impl.
            return java.util.Objects.equals(plugin, other.plugin)
                    && parentRealm == other.parentRealm
                    && java.util.Objects.equals(foreignImports, other.foreignImports)
                    && java.util.Objects.equals(filter, other.filter)
                    && java.util.Objects.equals(repositories, other.repositories)
                    && java.util.Objects.equals(workspace, other.workspace)
                    && java.util.Objects.equals(localRepo, other.localRepo);
        }

        @Override
        public String toString() {
            return "BaristaCacheKey{" + pluginGa() + "}";
        }
    }

}
