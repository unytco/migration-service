import { describe, expect, it } from "vitest";
import {
  compareVersions,
  lineageOf,
  lineageOfReleaseUrl,
  newestOnLineage,
  publishedBuilds,
  type Build,
  type CacheLike,
} from "../src/builds";
import type { Env, FetchLike } from "../src/notary";

const ENV: Env = { MIGRATION_NOTARY_BEARER_TOKEN: "test-token" };

const jsonResp = (status: number, b: unknown) =>
  new Response(JSON.stringify(b), {
    status,
    headers: { "content-type": "application/json" },
  });

const releasesResp = (
  rels: Array<{
    tag: string;
    draft?: boolean;
    prerelease?: boolean;
    assets?: Array<{ name: string; url: string; digest?: string }>;
  }>,
) =>
  jsonResp(
    200,
    rels.map((r) => ({
      tag_name: r.tag,
      draft: r.draft ?? false,
      prerelease: r.prerelease ?? false,
      html_url: `https://github.com/unytco/unyt-sandbox/releases/tag/${r.tag}`,
      assets: (r.assets ?? []).map((a) => ({
        name: a.name,
        browser_download_url: a.url,
        ...(a.digest ? { digest: a.digest } : {}),
      })),
    })),
  );

const ghFetch = (make: () => Response): FetchLike =>
  (async () => make()) as FetchLike;

describe("lineageOf", () => {
  it("takes major.minor from full, short, and v-prefixed versions", () => {
    expect(lineageOf("0.93.0")).toBe("0.93");
    expect(lineageOf("0.93")).toBe("0.93");
    expect(lineageOf("v0.93.1")).toBe("0.93");
    expect(lineageOf(" 1.4.2 ")).toBe("1.4");
  });
  it("is null for unparseable / empty / nullish", () => {
    expect(lineageOf("nightly")).toBeNull();
    expect(lineageOf("")).toBeNull();
    expect(lineageOf(null)).toBeNull();
    expect(lineageOf(undefined)).toBeNull();
  });
});

describe("lineageOfReleaseUrl", () => {
  it("parses the lineage from a release-tag URL", () => {
    expect(
      lineageOfReleaseUrl(
        "https://github.com/unytco/unyt-sandbox/releases/tag/v0.2.0",
      ),
    ).toBe("0.2");
  });
  it("is null for a non-tag URL or nullish", () => {
    expect(lineageOfReleaseUrl("https://example/b")).toBeNull();
    expect(lineageOfReleaseUrl(undefined)).toBeNull();
  });
});

describe("compareVersions", () => {
  it("orders by major, then minor, then patch", () => {
    expect(compareVersions("0.3.2", "0.3.1")).toBeGreaterThan(0);
    expect(compareVersions("0.3.1", "0.3.2")).toBeLessThan(0);
    expect(compareVersions("1.0.0", "0.9.9")).toBeGreaterThan(0);
    expect(compareVersions("0.3.0", "0.3.0")).toBe(0);
  });
});

describe("newestOnLineage", () => {
  const builds: Build[] = [
    { version: "0.3.1", release_url: "u1" },
    { version: "0.3.4", release_url: "u2" },
    { version: "0.2.9", release_url: "u3" },
  ];
  it("returns the highest patch on the lineage", () => {
    expect(newestOnLineage(builds, "0.3")).toEqual({
      version: "0.3.4",
      release_url: "u2",
    });
  });
  it("is null when nothing is on the lineage or the lineage is null", () => {
    expect(newestOnLineage(builds, "0.9")).toBeNull();
    expect(newestOnLineage(builds, null)).toBeNull();
  });
});

describe("publishedBuilds", () => {
  it("keeps only published, anchored tags (drops draft, pre-release, rc, and garbage)", async () => {
    const fetch = ghFetch(() =>
      releasesResp([
        { tag: "v0.3.4" },
        { tag: "v0.3.3", draft: true },
        { tag: "v0.3.2", prerelease: true },
        { tag: "v0.3.1-rc.2" },
        { tag: "nightly" },
      ]),
    );
    const builds = await publishedBuilds(fetch, ENV);
    expect(builds).toEqual([
      {
        version: "0.3.4",
        release_url:
          "https://github.com/unytco/unyt-sandbox/releases/tag/v0.3.4",
        assets: [],
      },
    ]);
  });

  it("paginates past the first page so an older lineage's newest build isn't lost", async () => {
    // Page 1 = 100 newest releases, all on lineage 0.95 (a full page → a page 2 exists). Page 2 =
    // the older 0.93 lineage's builds. Reading only page 1 would drop 0.93's latest_build entirely.
    const page1 = Array.from({ length: 100 }, (_, i) => ({
      tag: `v0.95.${i}`,
    }));
    const page2 = [{ tag: "v0.93.7" }, { tag: "v0.93.6" }];
    const fetch = (async (input: RequestInfo | URL) => {
      const u = typeof input === "string" ? input : input.toString();
      const page = /[?&]page=(\d+)/.exec(u)?.[1] ?? "1";
      return releasesResp(page === "1" ? page1 : page === "2" ? page2 : []);
    }) as FetchLike;
    const builds = await publishedBuilds(fetch, ENV);
    expect(newestOnLineage(builds, "0.95")?.version).toBe("0.95.99");
    expect(newestOnLineage(builds, "0.93")?.version).toBe("0.93.7"); // found on page 2
  });

  it("returns [] on a non-2xx upstream (never throws)", async () => {
    expect(
      await publishedBuilds(
        ghFetch(() => jsonResp(403, {})),
        ENV,
      ),
    ).toEqual([]);
  });

  it("negative-caches [] briefly on a total failure so a GitHub outage isn't re-hit every poll", async () => {
    let calls = 0;
    const fetch = (async () => {
      calls++;
      return jsonResp(503, {});
    }) as FetchLike;
    const store = new Map<string, Build[]>();
    const cache: CacheLike = {
      async get(k) {
        return store.get(k) ?? null;
      },
      async set(k, v) {
        store.set(k, v);
      },
    };
    expect(await publishedBuilds(fetch, ENV, cache)).toEqual([]);
    expect(await publishedBuilds(fetch, ENV, cache)).toEqual([]); // served from the negative cache
    expect(calls).toBe(1); // GitHub hit once, not on every call
  });

  it("brief-caches the PARTIAL scan on a persistent later-page failure (earlier pages not re-hit)", async () => {
    let calls = 0;
    const page1 = Array.from({ length: 100 }, (_, i) => ({
      tag: `v0.95.${i}`,
    }));
    const fetch = (async (input: RequestInfo | URL) => {
      calls++;
      const u = typeof input === "string" ? input : input.toString();
      const page = /[?&]page=(\d+)/.exec(u)?.[1] ?? "1";
      return page === "1" ? releasesResp(page1) : jsonResp(503, {}); // page 2 persistently fails
    }) as FetchLike;
    const store = new Map<string, Build[]>();
    const cache: CacheLike = {
      async get(k) {
        return store.get(k) ?? null;
      },
      async set(k, v) {
        store.set(k, v);
      },
    };
    const first = await publishedBuilds(fetch, ENV, cache); // page 1 ok, page 2 fails → partial
    expect(newestOnLineage(first, "0.95")?.version).toBe("0.95.99");
    const afterFirst = calls; // page 1 + page 2
    const second = await publishedBuilds(fetch, ENV, cache); // served from the partial cache
    expect(second).toEqual(first);
    expect(calls).toBe(afterFirst); // no re-fetch — earlier pages not re-requested
  });

  it("returns [] when the upstream fetch throws", async () => {
    const boom = (async () => {
      throw new TypeError("down");
    }) as FetchLike;
    expect(await publishedBuilds(boom, ENV)).toEqual([]);
  });

  it("serves a cache hit without fetching, and populates the cache on a miss", async () => {
    let calls = 0;
    const fetch = (async () => {
      calls++;
      return releasesResp([{ tag: "v0.3.1" }]);
    }) as FetchLike;
    const store = new Map<string, Build[]>();
    const cache: CacheLike = {
      async get(k) {
        return store.get(k) ?? null;
      },
      async set(k, v) {
        store.set(k, v);
      },
    };
    const first = await publishedBuilds(fetch, ENV, cache);
    const second = await publishedBuilds(fetch, ENV, cache);
    expect(calls).toBe(1);
    expect(second).toEqual(first);
  });

  it("carries a release's downloadable installer assets (name + browser_download_url → name + url)", async () => {
    const builds = await publishedBuilds(
      ghFetch(() =>
        releasesResp([
          {
            tag: "v0.93.2",
            assets: [
              {
                name: "unyt_0.93.2_amd64.deb",
                url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/unyt_0.93.2_amd64.deb",
              },
              {
                name: "unyt_0.93.2_x64.dmg",
                url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/unyt_0.93.2_x64.dmg",
              },
            ],
          },
        ]),
      ),
      ENV,
    );
    expect(builds).toHaveLength(1);
    expect(builds[0].assets).toEqual([
      {
        name: "unyt_0.93.2_amd64.deb",
        url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/unyt_0.93.2_amd64.deb",
      },
      {
        name: "unyt_0.93.2_x64.dmg",
        url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/unyt_0.93.2_x64.dmg",
      },
    ]);
  });

  it("carries GitHub's asset digest when present, and omits it when absent", async () => {
    const builds = await publishedBuilds(
      ghFetch(() =>
        releasesResp([
          {
            tag: "v0.93.2",
            assets: [
              {
                name: "signed.deb",
                url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/signed.deb",
                digest: "sha256:deadbeef",
              },
              {
                name: "older.deb",
                url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/older.deb",
              },
            ],
          },
        ]),
      ),
      ENV,
    );
    expect(builds[0].assets).toEqual([
      {
        name: "signed.deb",
        url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/signed.deb",
        digest: "sha256:deadbeef",
      },
      // No digest on the release → the field is simply absent (the app then skips verification).
      {
        name: "older.deb",
        url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/older.deb",
      },
    ]);
  });

  it("a release with no assets yields an empty asset list", async () => {
    const builds = await publishedBuilds(
      ghFetch(() => releasesResp([{ tag: "v0.93.2" }])),
      ENV,
    );
    expect(builds[0].assets).toEqual([]);
  });

  it("drops malformed assets (missing name or download url), keeping the well-formed ones", async () => {
    const raw = jsonResp(200, [
      {
        tag_name: "v0.93.2",
        draft: false,
        prerelease: false,
        html_url: "https://github.com/unytco/unyt-sandbox/releases/tag/v0.93.2",
        assets: [
          {
            name: "good.deb",
            browser_download_url:
              "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/good.deb",
          },
          { name: "no-url.deb" },
          {
            browser_download_url:
              "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/no-name",
          },
        ],
      },
    ]);
    const builds = await publishedBuilds(
      ghFetch(() => raw),
      ENV,
    );
    expect(builds[0].assets).toEqual([
      {
        name: "good.deb",
        url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.93.2/good.deb",
      },
    ]);
  });
});
