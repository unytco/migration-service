// The release itself (draft, pre-release, tag) decides which builds /v1/update-check offers;
// the router keeps no list of its own.

import type { Env, FetchLike } from "./notary";

const PER_PAGE = 100;
/** Beyond this bound an older lineage's newest build could be missed. */
const MAX_PAGES = 10;
/** How long a failed scan's result is held, so an outage of the releases URL isn't re-hit on every
 * poll. */
const FAIL_CACHE_TTL_SECONDS = 30;
const CACHE_TTL_SECONDS = 600; // The Cache API is per colo, so each datacenter reads once per TTL.
const FETCH_TIMEOUT_MS = 10_000;

/** Routers on one zone share the Cache API, so the key names the releases URL. The Cache API can
 * drop a query string from its key, so the URL rides in the path. */
function cacheKey(releasesUrl: string): string {
  return `https://migration-router.internal/releases/${encodeURIComponent(releasesUrl)}`;
}

/** A downloadable installer attached to a release, for the in-app same-lineage updater. `url` is
 * GitHub's `browser_download_url`, under the release repo so it passes the app's download allowlist;
 * the app matches its platform's installer by `name`. The router stays platform-agnostic.
 *
 * `digest` (GitHub's `"sha256:<hex>"`, absent on older releases) lets the app catch a truncated or
 * corrupt download before handing an installer to the OS. It is NOT a defense against a compromised
 * release: it rides this same response, so whoever can swap the asset can swap the digest. Its value is
 * on Linux (`.deb`/`.AppImage`), where no OS signature check sits behind us. */
export interface BuildAsset {
  name: string;
  url: string;
  digest?: string;
}

/** A published build on a lineage. `version` is bare semver ("0.93.2"); `release_url` is the
 * release page, always under the release repo, so it passes the app's download allowlist.
 * `assets` are the release's downloadable installers. */
export interface Build {
  version: string;
  release_url: string;
  assets?: BuildAsset[];
}

/** Rate-shield seam over the Worker Cache API: production wraps `caches.default` (`cfCache`),
 * tests pass a Map-backed fake or omit it to disable caching. Kept out of the handler so the
 * Cache global never touches the unit-tested path. */
export interface CacheLike {
  get(key: string): Promise<Build[] | null>;
  set(key: string, value: Build[], ttlSeconds: number): Promise<void>;
}

/** Anchored, so `-dev.*` and test tags never match. */
const TAG_RE = /^v(\d+)\.(\d+)\.(\d+)$/;

/** major.minor of a version-ish string ("0.93.0" | "0.93" | "v0.93.1" → "0.93"); null if unparseable. */
export function lineageOf(version: string | null | undefined): string | null {
  if (!version) return null;
  const m = /^v?(\d+)\.(\d+)/.exec(version.trim());
  return m ? `${m[1]}.${m[2]}` : null;
}

/** major.minor parsed from a registry release-tag URL (".../releases/tag/v0.2.0" → "0.2"); null if none. */
export function lineageOfReleaseUrl(
  url: string | null | undefined,
): string | null {
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
  assets?: { name?: string; browser_download_url?: string; digest?: string }[];
}

/** Never throws, so the caller's migration answer is always produced. A failed first page gives
 * [], a failed later page the builds read before it. */
export async function publishedBuilds(
  fetchImpl: FetchLike,
  env: Env,
  cache?: CacheLike,
): Promise<Build[]> {
  const releasesUrl = env.GITHUB_RELEASES_URL;
  if (!releasesUrl) {
    console.warn(
      "migration-router: GITHUB_RELEASES_URL is unset, so update checks carry no latest_build",
    );
    return [];
  }
  const key = cacheKey(releasesUrl);
  if (cache) {
    const hit = await cache.get(key).catch(() => null);
    if (hit) return hit;
  }
  const headers: Record<string, string> = {
    accept: "application/vnd.github+json",
    "user-agent": "unyt-migration-router",
  };
  if (env.GITHUB_TOKEN) headers.authorization = `Bearer ${env.GITHUB_TOKEN}`;

  const builds: Build[] = [];
  const bail = async (page: number, cause: string): Promise<Build[]> => {
    console.warn(
      `migration-router: releases at ${releasesUrl} failed on page ${page} (${cause}); latest_build is ${page === 1 ? "omitted" : "partial"}`,
    );
    const result = page === 1 ? [] : builds;
    if (cache)
      await cache.set(key, result, FAIL_CACHE_TTL_SECONDS).catch(() => {});
    return result;
  };
  // GitHub lists releases newest first across every lineage, so an older lineage's newest build
  // can sit past page 1.
  for (let page = 1; page <= MAX_PAGES; page++) {
    let batch: unknown;
    try {
      const resp = await fetchImpl(
        `${releasesUrl}?per_page=${PER_PAGE}&page=${page}`,
        {
          headers,
          signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
        },
      );
      if (!resp.ok) return bail(page, `HTTP ${resp.status}`);
      batch = await resp.json();
    } catch (err) {
      return bail(page, String(err));
    }
    if (!Array.isArray(batch)) return bail(page, "not a JSON array");
    const releases: unknown[] = batch;
    for (const entry of releases) {
      if (typeof entry !== "object" || entry === null) continue;
      const r = entry as GhRelease;
      // A listing read with a token that has push access includes drafts.
      if (r.draft === true || r.prerelease === true) continue;
      const tag = typeof r.tag_name === "string" ? r.tag_name : "";
      if (!TAG_RE.test(tag)) continue;
      const url = typeof r.html_url === "string" ? r.html_url : "";
      if (!url) continue;
      const assets: BuildAsset[] = Array.isArray(r.assets)
        ? r.assets.flatMap((a) =>
            typeof a?.name === "string" &&
            typeof a?.browser_download_url === "string"
              ? [
                  {
                    name: a.name,
                    url: a.browser_download_url,
                    ...(typeof a.digest === "string" && a.digest
                      ? { digest: a.digest }
                      : {}),
                  },
                ]
              : [],
          )
        : [];
      builds.push({ version: tag.slice(1), release_url: url, assets });
    }
    if (releases.length < PER_PAGE) break;
    if (page === MAX_PAGES) {
      console.warn(
        `migration-router: GitHub release scan hit the ${MAX_PAGES}-page cap; an older lineage's latest_build may be missing`,
      );
    }
  }
  if (cache) await cache.set(key, builds, CACHE_TTL_SECONDS).catch(() => {});
  return builds;
}

/** Newest published build on `lineage` (major.minor), or null when none qualifies. */
export function newestOnLineage(
  builds: readonly Build[],
  lineage: string | null,
): Build | null {
  if (!lineage) return null;
  let best: Build | null = null;
  for (const b of builds) {
    if (lineageOf(b.version) !== lineage) continue;
    if (best === null || compareVersions(b.version, best.version) > 0) best = b;
  }
  return best;
}

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
        headers: {
          "content-type": "application/json",
          "cache-control": `max-age=${ttlSeconds}`,
        },
      });
      await cache.put(new Request(key), resp);
    },
  };
}
