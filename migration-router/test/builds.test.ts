import { describe, expect, it, vi } from "vitest";
import {
  cfCache,
  compareVersions,
  lineageOf,
  lineageOfReleaseUrl,
  newestOnLineage,
  publishedBuilds,
  type Build,
  type CacheLike,
} from "../src/builds";
import type { Env, FetchLike } from "../src/notary";
import { jsonResp, releasesResp } from "./github-fixtures";

const ENV: Env = {
  MIGRATION_NOTARY_BEARER_TOKEN: "test-token",
  GITHUB_RELEASES_URL:
    "https://api.github.com/repos/unytco/unyt-sandbox/releases",
};

function mapCache(): CacheLike & { store: Map<string, Build[]> } {
  const store = new Map<string, Build[]>();
  return {
    store,
    async get(k) {
      return store.get(k) ?? null;
    },
    async set(k, v) {
      store.set(k, v);
    },
  };
}

async function warnsOf<T>(fragment: string, run: () => Promise<T>): Promise<T> {
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  try {
    const result = await run();
    expect(warn).toHaveBeenCalledWith(expect.stringContaining(fragment));
    return result;
  } finally {
    warn.mockRestore();
  }
}

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
  it("keeps only published, anchored tags (drops draft, pre-release, dev, and garbage)", async () => {
    const fetch = ghFetch(() =>
      releasesResp([
        { tag: "v0.3.4" },
        { tag: "v0.3.3", draft: true },
        { tag: "v0.3.2", prerelease: true },
        { tag: "v0.3.1-dev.2" },
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

  it("reads no releases when GITHUB_RELEASES_URL is unset, and says so", async () => {
    let calls = 0;
    const fetch = (async () => {
      calls++;
      return releasesResp([{ tag: "v0.93.1" }]);
    }) as FetchLike;
    const cache = mapCache();
    const builds = await warnsOf("GITHUB_RELEASES_URL is unset", () =>
      publishedBuilds(fetch, { ...ENV, GITHUB_RELEASES_URL: undefined }, cache),
    );
    expect(builds).toEqual([]);
    expect(calls).toBe(0);
    expect(cache.store.size).toBe(0);
  });

  it("two releases URLs keep two Worker cache entries, so neither serves the other's builds", async () => {
    const sandbox = "https://api.github.com/repos/unytco/unyt-sandbox/releases";
    const unyt = "https://api.github.com/repos/unytco/unyt/releases";
    let calls = 0;
    const fetch = (async (input: RequestInfo | URL) => {
      calls++;
      const u = typeof input === "string" ? input : input.toString();
      return releasesResp([
        { tag: u.startsWith(unyt) ? "v1.0.2" : "v0.109.3" },
      ]);
    }) as FetchLike;
    const stored = new Map<string, string>();
    const cache = cfCache({
      async match(req: Request) {
        const hit = stored.get(req.url);
        return hit === undefined ? undefined : new Response(hit);
      },
      async put(req: Request, resp: Response) {
        stored.set(req.url, await resp.text());
      },
    } as unknown as Cache);
    const versions = async (url: string) =>
      (
        await publishedBuilds(
          fetch,
          { ...ENV, GITHUB_RELEASES_URL: url },
          cache,
        )
      ).map((b) => b.version);
    for (let round = 0; round < 2; round++) {
      expect(await versions(sandbox)).toEqual(["0.109.3"]);
      expect(await versions(unyt)).toEqual(["1.0.2"]);
    }
    expect(calls).toBe(2);
    expect(stored.size).toBe(2);
    for (const key of stored.keys()) expect(new URL(key).search).toBe("");
  });

  it("reads the releases at GITHUB_RELEASES_URL, such as a local artifact server", async () => {
    const seen: string[] = [];
    const fetch = (async (input: RequestInfo | URL) => {
      seen.push(typeof input === "string" ? input : input.toString());
      return releasesResp([{ tag: "v0.93.1" }]);
    }) as FetchLike;
    const builds = await publishedBuilds(fetch, {
      ...ENV,
      GITHUB_RELEASES_URL: "http://localhost:8788/releases",
    });
    expect(seen[0]).toMatch(/^http:\/\/localhost:8788\/releases\?/);
    expect(builds).toEqual([
      {
        version: "0.93.1",
        release_url:
          "https://github.com/unytco/unyt-sandbox/releases/tag/v0.93.1",
        assets: [],
      },
    ]);
  });

  it("returns [] on a non-2xx upstream (never throws), and logs the URL and status", async () => {
    const builds = await warnsOf(
      `${ENV.GITHUB_RELEASES_URL} failed on page 1 (HTTP 404); latest_build is omitted`,
      () =>
        publishedBuilds(
          ghFetch(() => jsonResp(404, {})),
          ENV,
        ),
    );
    expect(builds).toEqual([]);
  });

  it("returns [] on a body that is not a JSON array, and logs it", async () => {
    const builds = await warnsOf("failed on page 1 (not a JSON array)", () =>
      publishedBuilds(
        ghFetch(() => jsonResp(200, { message: "moved" })),
        ENV,
      ),
    );
    expect(builds).toEqual([]);
  });

  it("negative-caches [] briefly on a total failure so a GitHub outage isn't re-hit every poll", async () => {
    let calls = 0;
    const fetch = (async () => {
      calls++;
      return jsonResp(503, {});
    }) as FetchLike;
    const cache = mapCache();
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
    const cache = mapCache();
    const first = await warnsOf(
      "failed on page 2 (HTTP 503); latest_build is partial",
      () => publishedBuilds(fetch, ENV, cache),
    );
    expect(newestOnLineage(first, "0.95")?.version).toBe("0.95.99");
    const afterFirst = calls; // page 1 + page 2
    const second = await publishedBuilds(fetch, ENV, cache); // served from the partial cache
    expect(second).toEqual(first);
    expect(calls).toBe(afterFirst); // no re-fetch — earlier pages not re-requested
  });

  it("skips listing entries that are not objects (never throws)", async () => {
    const builds = await publishedBuilds(
      ghFetch(() =>
        jsonResp(200, [
          null,
          7,
          "v0.93.1",
          {
            tag_name: "v0.93.2",
            html_url:
              "https://github.com/unytco/unyt-sandbox/releases/tag/v0.93.2",
          },
        ]),
      ),
      ENV,
    );
    expect(builds.map((b) => b.version)).toEqual(["0.93.2"]);
  });

  it("returns [] when the upstream fetch throws", async () => {
    const boom = (async () => {
      throw new TypeError("down");
    }) as FetchLike;
    expect(
      await warnsOf("failed on page 1 (TypeError: down)", () =>
        publishedBuilds(boom, ENV),
      ),
    ).toEqual([]);
  });

  it("serves a cache hit without fetching, and populates the cache on a miss", async () => {
    let calls = 0;
    const fetch = (async () => {
      calls++;
      return releasesResp([{ tag: "v0.3.1" }]);
    }) as FetchLike;
    const cache = mapCache();
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
