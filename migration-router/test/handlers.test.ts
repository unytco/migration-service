import { describe, expect, it, vi } from "vitest";
import { Registry, type RawRegistry } from "../src/registry";
import { migrationOptions, shuffled, updateCheck } from "../src/handlers";
import type { Env, FetchLike } from "../src/notary";
import type { Build, CacheLike } from "../src/builds";
import { jsonResp, releasesResp } from "./github-fixtures";

const v01 = "uhC0k_v01";
const v02 = "uhC0k_v02";
const v03 = "uhC0k_v03";

const ENV: Env = {
  MIGRATION_NOTARY_BEARER_TOKEN: "test-token",
  GITHUB_RELEASES_URL:
    "https://api.github.com/repos/unytco/unyt-sandbox/releases",
};

// Seeded `rand` values for the two-candidate dispatch ([n1a, n1b]): Fisher–Yates
// with i=1 swaps when floor(rand()*2) === 0, so a high value keeps the registry
// order and a low value reverses it.
const KEEP_ORDER = () => 0.99; // [n1a, n1b]
const REVERSE = () => 0.0; // [n1b, n1a]

function registry(): Registry {
  const raw: RawRegistry = {
    version: 1,
    dnas: [
      {
        dna_hash: v01,
        version: "alliance-v0.1.0",
        upgrade_targets: [v02, v03],
        closing_threshold: 1,
        notaries: [
          { url: "https://n1a", api: "v2" },
          { url: "https://n1b", api: "v2" },
        ],
      },
      {
        dna_hash: v02,
        version: "alliance-v0.2.0",
        upgrades_from: v01,
        upgrade_targets: [v03],
        closing_threshold: 1,
        release_url:
          "https://github.com/unytco/unyt-sandbox/releases/tag/v0.2.0",
        published: true,
        notaries: [{ url: "https://n2", api: "v2" }],
      },
      {
        dna_hash: v03,
        version: "alliance-v0.3.0",
        upgrades_from: v02,
        published: true,
        notaries: [{ url: "https://n3", api: "v2" }],
      },
    ],
  };
  return Registry.load(raw);
}

/** Build a mock fetch keyed by daemon origin → canned Response. */
function mockFetch(byOrigin: Record<string, () => Response>): FetchLike {
  return (async (input: RequestInfo | URL) => {
    const url = typeof input === "string" ? input : input.toString();
    for (const [origin, make] of Object.entries(byOrigin)) {
      if (url.startsWith(origin)) return make();
    }
    throw new TypeError(`network error: no mock for ${url}`);
  }) as FetchLike;
}

async function body(resp: Response): Promise<any> {
  return resp.json();
}

describe("shuffled", () => {
  it("is deterministic under a seeded rand and permutes the input", () => {
    const xs = ["a", "b", "c", "d"];
    // A fixed sequence drives a fixed permutation.
    const seq = [0.1, 0.9, 0.4];
    let i = 0;
    const rand = () => seq[i++ % seq.length];
    const once = shuffled(xs, rand);
    i = 0;
    const twice = shuffled(xs, rand);
    expect(once).toEqual(twice);
    expect([...once].sort()).toEqual([...xs].sort());
    expect(xs).toEqual(["a", "b", "c", "d"]); // input untouched
  });

  it("different seeds produce different orders (the load spread)", () => {
    const xs = ["a", "b"];
    expect(shuffled(xs, KEEP_ORDER)).toEqual(["a", "b"]);
    expect(shuffled(xs, REVERSE)).toEqual(["b", "a"]);
  });
});

// Every 200 below asserts the echoed `to_dna_hash`. The spec § Test expectations
// pins it ("every answer echoes the queried `to_dna_hash`") because the field is
// load-bearing, not decorative: the app rejects an answer echoing a different hash
// (an intermediary caching this response across query strings), and a rejected
// answer lands on the same retry card as an unreachable router.
describe("migrationOptions", () => {
  it("returns the immediate predecessor mid-chain", () => {
    const resp = migrationOptions(registry(), v03);
    expect(resp.status).toBe(200);
  });

  it("v0.3 returns all sources that reach it (skip: v0.1 and v0.2)", async () => {
    const b = await body(migrationOptions(registry(), v03));
    expect(b.to_dna_hash).toBe(v03);
    expect(b.options).toEqual([
      { from_dna_hash: v01, from_version: "alliance-v0.1.0" },
      { from_dna_hash: v02, from_version: "alliance-v0.2.0" },
    ]);
  });

  it("chain root (registered, no sources) returns empty options", async () => {
    const resp = migrationOptions(registry(), v01);
    expect(resp.status).toBe(200);
    const b = await body(resp);
    expect(b.to_dna_hash).toBe(v01);
    expect(b.options).toEqual([]);
  });

  // Source of truth: the version-migration `migration-router.md` spec § Endpoints —
  // "Chain root / unknown DNA → `{ options: [] }` (not an error; an empty array is
  // the definitive 'no predecessor' the app is allowed to join fresh on)". The app
  // reads any non-2xx here as "router unreachable" and shows a retry card, so a 4xx
  // would strand a fresh installer.
  it("unknown DNA returns 200 empty options, never an error (fresh-install contract)", async () => {
    const resp = migrationOptions(registry(), "uhC0k_unknown");
    expect(resp.status).toBe(200);
    const b = await body(resp);
    expect(b.to_dna_hash).toBe("uhC0k_unknown");
    expect(b.options).toEqual([]);
    expect(b.error).toBeUndefined();
  });

  it("missing to_dna_hash errors", async () => {
    const resp = migrationOptions(registry(), null);
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unknown_to_dna");
  });
});

/** A fetch that answers only api.github.com (via `make`) and throws for anything else. */
function ghFetch(make: () => Response): FetchLike {
  return (async (input: RequestInfo | URL) => {
    const url = typeof input === "string" ? input : input.toString();
    if (url.startsWith("https://api.github.com")) return make();
    throw new TypeError(`network error: no mock for ${url}`);
  }) as FetchLike;
}

describe("updateCheck — migration axis (no app_version → unchanged, no GitHub call)", () => {
  const noGh = mockFetch({}); // throws on any call → proves the no-version path reaches nobody

  it("returns the FURTHEST target (skips past v0.2 to v0.3), omitting release_url when absent", async () => {
    const b = await body(await updateCheck(registry(), v01, null, noGh, ENV));
    expect(b).toEqual({
      current_dna_hash: v01,
      has_upgrade: true,
      target: { to_dna_hash: v03, to_version: "alliance-v0.3.0" }, // v0.3 has no release_url
    });
  });

  it("includes release_url when the furthest target has one", async () => {
    const a = "uhC0k_a";
    const bDna = "uhC0k_b";
    const r = Registry.load({
      version: 1,
      dnas: [
        {
          dna_hash: a,
          version: "a",
          upgrade_targets: [bDna],
          closing_threshold: 1,
          notaries: [{ url: "https://na", api: "v2" }],
        },
        {
          dna_hash: bDna,
          version: "b",
          upgrades_from: a,
          release_url: "https://example/b",
          published: true,
          notaries: [{ url: "https://nb", api: "v2" }],
        },
      ],
    });
    const resp = await body(await updateCheck(r, a, null, noGh, ENV));
    expect(resp.target).toEqual({
      to_dna_hash: bDna,
      to_version: "b",
      release_url: "https://example/b",
    });
  });

  it("chain tip has no upgrade", async () => {
    const b = await body(await updateCheck(registry(), v03, null, noGh, ENV));
    expect(b).toEqual({ current_dna_hash: v03, has_upgrade: false });
  });

  it("unknown current DNA has no upgrade", async () => {
    const b = await body(
      await updateCheck(registry(), "uhC0k_unknown", null, noGh, ENV),
    );
    expect(b).toEqual({
      current_dna_hash: "uhC0k_unknown",
      has_upgrade: false,
    });
  });

  it("missing current_dna_hash errors", async () => {
    const resp = await updateCheck(registry(), null, null, noGh, ENV);
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unknown_current_dna");
  });

  it("an unparseable app_version is treated as absent (no GitHub call, no build axis)", async () => {
    const b = await body(
      await updateCheck(registry(), v03, "nightly", noGh, ENV),
    );
    expect(b).toEqual({ current_dna_hash: v03, has_upgrade: false });
  });
});

// The customers-last gate on /v1/update-check. /v1/migrate serves the same unpublished entry
// (test/migrate.test.ts), so the headless server open works before customers see the banner.
describe("updateCheck: the published (customers-last) gate", () => {
  const noGh = mockFetch({}); // update-check's no-version path must reach no network

  /** A single-step chain v01 → v02, with v02's customer-visibility parameterised. */
  function gated(published: boolean): Registry {
    return Registry.load({
      version: 1,
      dnas: [
        {
          dna_hash: v01,
          version: "alliance-v0.1.0",
          upgrade_targets: [v02],
          closing_threshold: 1,
          notaries: [{ url: "https://n1", api: "v2" }],
        },
        {
          dna_hash: v02,
          version: "alliance-v0.2.0",
          upgrades_from: v01,
          release_url:
            "https://github.com/unytco/unyt-sandbox/releases/tag/v0.2.0",
          published,
          notaries: [{ url: "https://n2", api: "v2" }],
        },
      ],
    });
  }

  it("update-check HIDES an unpublished successor (no banner, has_upgrade:false)", async () => {
    const b = await body(await updateCheck(gated(false), v01, null, noGh, ENV));
    expect(b).toEqual({ current_dna_hash: v01, has_upgrade: false });
  });

  it("publishing the successor FLIPS the banner on (has_upgrade:true) — nothing else changed", async () => {
    const b = await body(await updateCheck(gated(true), v01, null, noGh, ENV));
    expect(b).toEqual({
      current_dna_hash: v01,
      has_upgrade: true,
      target: {
        to_dna_hash: v02,
        to_version: "alliance-v0.2.0",
        release_url:
          "https://github.com/unytco/unyt-sandbox/releases/tag/v0.2.0",
      },
    });
  });
});

describe("updateCheck — build axis (app_version present)", () => {
  it("reports the newest published build on the caller's lineage as latest_build", async () => {
    const fetch = ghFetch(() =>
      releasesResp([{ tag: "v0.3.2" }, { tag: "v0.3.1" }, { tag: "v0.2.9" }]),
    );
    const b = await body(
      await updateCheck(registry(), v03, "0.3.0", fetch, ENV),
    );
    expect(b.has_upgrade).toBe(false); // v03 is the chain tip
    expect(b.latest_build).toEqual({
      version: "0.3.2",
      release_url: "https://github.com/unytco/unyt-sandbox/releases/tag/v0.3.2",
      assets: [],
    });
  });

  it("carries the release's installers (name + url + digest) through latest_build on the wire", async () => {
    // Pins the cross-service contract end to end, not just in the parser: this response IS what the app's
    // in-app updater consumes to pick its platform's installer, so the wire shape is asserted here.
    const dmg = {
      name: "unyt_0.3.2_full-arc_aarch64_darwin.dmg",
      url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.3.2/unyt_0.3.2_full-arc_aarch64_darwin.dmg",
      digest: "sha256:abc123",
    };
    const deb = {
      name: "unyt_0.3.2_full-arc_x86_64_linux.deb",
      url: "https://github.com/unytco/unyt-sandbox/releases/download/v0.3.2/unyt_0.3.2_full-arc_x86_64_linux.deb",
    };
    const fetch = ghFetch(() =>
      releasesResp([{ tag: "v0.3.2", assets: [dmg, deb] }, { tag: "v0.3.1" }]),
    );
    const b = await body(
      await updateCheck(registry(), v03, "0.3.0", fetch, ENV),
    );
    expect(b.latest_build).toEqual({
      version: "0.3.2",
      release_url: "https://github.com/unytco/unyt-sandbox/releases/tag/v0.3.2",
      // Platform-agnostic: every installer is passed through, and the app selects its own.
      assets: [dmg, deb],
    });
  });

  it("excludes drafts, pre-releases and non-anchored tags", async () => {
    const fetch = ghFetch(() =>
      releasesResp([
        { tag: "v0.3.9", draft: true },
        { tag: "v0.3.8", prerelease: true },
        { tag: "v0.3.7-dev.1" },
        { tag: "v0.3.5" },
      ]),
    );
    const b = await body(
      await updateCheck(registry(), v03, "0.3.0", fetch, ENV),
    );
    expect(b.latest_build.version).toBe("0.3.5");
  });

  it("omits latest_build when the caller's lineage has no published build (never falsy)", async () => {
    const fetch = ghFetch(() => releasesResp([{ tag: "v0.9.0" }]));
    const b = await body(
      await updateCheck(registry(), v03, "0.3.0", fetch, ENV),
    );
    expect(b.has_upgrade).toBe(false);
    expect("latest_build" in b).toBe(false);
  });

  it("resolves a migration target's link to the newest build of the TARGET lineage, overriding the recorded tag", async () => {
    const from = "uhC0k_from";
    const to = "uhC0k_to";
    const r = Registry.load({
      version: 1,
      dnas: [
        {
          dna_hash: from,
          version: "from",
          upgrade_targets: [to],
          closing_threshold: 1,
          notaries: [{ url: "https://nf", api: "v2" }],
        },
        {
          dna_hash: to,
          version: "to",
          upgrades_from: from,
          release_url:
            "https://github.com/unytco/unyt-sandbox/releases/tag/v0.5.0",
          published: true,
          notaries: [{ url: "https://nt", api: "v2" }],
        },
      ],
    });
    const fetch = ghFetch(() =>
      releasesResp([{ tag: "v0.5.3" }, { tag: "v0.5.0" }]),
    );
    const b = await body(await updateCheck(r, from, "0.5.0", fetch, ENV));
    expect(b.has_upgrade).toBe(true);
    expect(b.target.release_url).toBe(
      "https://github.com/unytco/unyt-sandbox/releases/tag/v0.5.3",
    );
    expect(b.latest_build.version).toBe("0.5.3"); // caller is also on 0.5
  });

  it("falls back to the recorded target link when GitHub can't resolve the target lineage", async () => {
    const from = "uhC0k_from2";
    const to = "uhC0k_to2";
    const r = Registry.load({
      version: 1,
      dnas: [
        {
          dna_hash: from,
          version: "from",
          upgrade_targets: [to],
          closing_threshold: 1,
          notaries: [{ url: "https://nf", api: "v2" }],
        },
        {
          dna_hash: to,
          version: "to",
          upgrades_from: from,
          release_url:
            "https://github.com/unytco/unyt-sandbox/releases/tag/v0.5.0",
          published: true,
          notaries: [{ url: "https://nt", api: "v2" }],
        },
      ],
    });
    const fetch = ghFetch(() => releasesResp([{ tag: "v0.9.0" }])); // nothing on 0.5
    const b = await body(await updateCheck(r, from, "0.9.0", fetch, ENV));
    expect(b.target.release_url).toBe(
      "https://github.com/unytco/unyt-sandbox/releases/tag/v0.5.0",
    );
  });

  it("keeps the migration answer and omits latest_build when GitHub is unreachable (still 2xx)", async () => {
    const boom = (async () => {
      throw new TypeError("network down");
    }) as FetchLike;
    const resp = await updateCheck(registry(), v01, "0.3.0", boom, ENV);
    expect(resp.status).toBe(200);
    const b = await body(resp);
    expect(b.has_upgrade).toBe(true);
    expect(b.target.to_dna_hash).toBe(v03);
    expect("latest_build" in b).toBe(false);
  });

  it("keeps the migration answer and omits latest_build, reading no releases, when GITHUB_RELEASES_URL is unset", async () => {
    let calls = 0;
    const fetch = (async () => {
      calls++;
      return releasesResp([{ tag: "v0.3.2" }]);
    }) as FetchLike;
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    let resp: Response;
    try {
      resp = await updateCheck(registry(), v01, "0.3.0", fetch, {
        ...ENV,
        GITHUB_RELEASES_URL: undefined,
      });
    } finally {
      warn.mockRestore();
    }
    expect(resp.status).toBe(200);
    expect(await body(resp)).toEqual({
      current_dna_hash: v01,
      has_upgrade: true,
      target: { to_dna_hash: v03, to_version: "alliance-v0.3.0" },
    });
    expect(calls).toBe(0);
  });

  it("treats a rate-limited / erroring upstream as no builds (still 2xx, migration intact)", async () => {
    const fetch = ghFetch(() => jsonResp(429, { message: "rate limited" }));
    const resp = await updateCheck(registry(), v01, "0.3.0", fetch, ENV);
    expect(resp.status).toBe(200);
    expect((await body(resp)).has_upgrade).toBe(true);
  });

  it("fetches the GitHub listing at most once across calls when a cache is provided", async () => {
    let calls = 0;
    const fetch = (async (input: RequestInfo | URL) => {
      const url = typeof input === "string" ? input : input.toString();
      if (url.startsWith("https://api.github.com")) {
        calls++;
        return releasesResp([{ tag: "v0.3.1" }]);
      }
      throw new TypeError(`no mock for ${url}`);
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
    await updateCheck(registry(), v03, "0.3.0", fetch, ENV, cache);
    await updateCheck(registry(), v03, "0.3.0", fetch, ENV, cache);
    expect(calls).toBe(1);
  });
});
