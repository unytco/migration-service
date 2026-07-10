// Build-axis resolution for /v1/update-check: the newest PUBLISHED desktop build per
// lineage, read from the public GitHub releases of unytco/unyt-sandbox. GitHub is the
// SOLE source of truth for the build axis and the pre-release flag is the only control
// (promote / de-promote) — the router adds no override, so nothing has to be kept
// aligned. Kept free of the Worker runtime (injected fetch + cache) so it is unit-testable
// exactly like the notary client.

import type { Env, FetchLike } from "./notary";

const RELEASES_URL = "https://api.github.com/repos/unytco/unyt-sandbox/releases?per_page=100";
/** Synthetic cache key (the Cache API keys on a URL). One listing covers every lineage, so a
 * single fetch per period serves them all — comfortably inside GitHub's unauthenticated limit. */
const CACHE_KEY = "https://migration-router.internal/gh/unyt-sandbox/releases";
const CACHE_TTL_SECONDS = 600; // 10-min shield; Cache API is per-colo, so this bound is per-datacenter.
const FETCH_TIMEOUT_MS = 10_000;

/** A published build on a lineage. `version` is bare semver ("0.93.2"); `release_url` is the
 * release page — always under the release repo, so it passes the app's download allowlist. */
export interface Build {
  version: string;
  release_url: string;
}

/** Rate-shield seam over the Worker Cache API: production wraps `caches.default` (`cfCache`),
 * tests pass a Map-backed fake or omit it to disable caching. Kept out of the handler so the
 * Cache global never touches the unit-tested path. */
export interface CacheLike {
  get(key: string): Promise<Build[] | null>;
  set(key: string, value: Build[], ttlSeconds: number): Promise<void>;
}

/** A build tag is exactly `vMAJOR.MINOR.PATCH` — anchored, so `-rc.*` and test tags never match. */
const TAG_RE = /^v(\d+)\.(\d+)\.(\d+)$/;

/** major.minor of a version-ish string ("0.93.0" | "0.93" | "v0.93.1" → "0.93"); null if unparseable. */
export function lineageOf(version: string | null | undefined): string | null {
  if (!version) return null;
  const m = /^v?(\d+)\.(\d+)/.exec(version.trim());
  return m ? `${m[1]}.${m[2]}` : null;
}

/** major.minor parsed from a registry release-tag URL (".../releases/tag/v0.2.0" → "0.2"); null if none. */
export function lineageOfReleaseUrl(url: string | null | undefined): string | null {
  if (!url) return null;
  const m = /\/releases\/tag\/(v\d+\.\d+\.\d+)/.exec(url);
  return m ? lineageOf(m[1]) : null;
}

/** Signed compare of two bare semvers "a.b.c"; > 0 iff x is newer than y. */
export function compareVersions(x: string, y: string): number {
  const px = x.split(".").map((n) => Number(n));
  const py = y.split(".").map((n) => Number(n));
  for (let i = 0; i < 3; i++) {
    const d = (px[i] ?? 0) - (py[i] ?? 0);
    if (d !== 0) return d;
  }
  return 0;
}

interface GhRelease {
  tag_name?: string;
  draft?: boolean;
  prerelease?: boolean;
  html_url?: string;
}

/** Fetch + filter GitHub's releases to PUBLISHED builds (not draft, not pre-release, tag exactly
 * `vMAJOR.MINOR.PATCH`). Returns [] on any upstream failure — never throws, so the caller's
 * migration answer is always produced. Cached across requests when a cache is provided. */
export async function publishedBuilds(fetchImpl: FetchLike, env: Env, cache?: CacheLike): Promise<Build[]> {
  if (cache) {
    const hit = await cache.get(CACHE_KEY).catch(() => null);
    if (hit) return hit;
  }
  let raw: unknown;
  try {
    const headers: Record<string, string> = {
      accept: "application/vnd.github+json",
      "user-agent": "unyt-migration-router",
    };
    // Optional read-only token — the escape hatch that raises the rate ceiling if the live-lineage
    // count ever grows. Unauthenticated by default.
    if (env.GITHUB_TOKEN) headers.authorization = `Bearer ${env.GITHUB_TOKEN}`;
    const resp = await fetchImpl(RELEASES_URL, { headers, signal: AbortSignal.timeout(FETCH_TIMEOUT_MS) });
    if (!resp.ok) return [];
    raw = await resp.json();
  } catch {
    return [];
  }
  if (!Array.isArray(raw)) return [];
  const builds: Build[] = [];
  for (const r of raw as GhRelease[]) {
    // Pre-release filtered explicitly; a draft is absent from an unauthenticated listing by
    // construction, and filtered here too so a read-only token can't leak one.
    if (r.draft === true || r.prerelease === true) continue;
    const tag = typeof r.tag_name === "string" ? r.tag_name : "";
    if (!TAG_RE.test(tag)) continue;
    const url = typeof r.html_url === "string" ? r.html_url : "";
    if (!url) continue;
    builds.push({ version: tag.slice(1), release_url: url });
  }
  if (cache) await cache.set(CACHE_KEY, builds, CACHE_TTL_SECONDS).catch(() => {});
  return builds;
}

/** Newest published build on `lineage` (major.minor), or null when none qualifies. */
export function newestOnLineage(builds: readonly Build[], lineage: string | null): Build | null {
  if (!lineage) return null;
  let best: Build | null = null;
  for (const b of builds) {
    if (lineageOf(b.version) !== lineage) continue;
    if (best === null || compareVersions(b.version, best.version) > 0) best = b;
  }
  return best;
}

/** Production `CacheLike` over the Worker Cache API. Referenced only from the Worker entry
 * (`index.ts`) — never from the handler — so `caches` stays out of the unit-tested path. */
export function cfCache(cache: Cache): CacheLike {
  return {
    async get(key) {
      const hit = await cache.match(new Request(key));
      if (!hit) return null;
      try {
        return (await hit.json()) as Build[];
      } catch {
        return null;
      }
    },
    async set(key, value, ttlSeconds) {
      const resp = new Response(JSON.stringify(value), {
        headers: { "content-type": "application/json", "cache-control": `max-age=${ttlSeconds}` },
      });
      await cache.put(new Request(key), resp);
    },
  };
}
