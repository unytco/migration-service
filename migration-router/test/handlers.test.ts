import { describe, expect, it } from "vitest";
import { Registry, type RawRegistry } from "../src/registry";
import {
  migrate,
  migrationOptions,
  shuffled,
  updateCheck,
  type MigrateBody,
} from "../src/handlers";
import type { Env, FetchLike } from "../src/notary";
import type { Build, CacheLike } from "../src/builds";

const v01 = "uhC0k_v01";
const v02 = "uhC0k_v02";
const v03 = "uhC0k_v03";
const AGENT = "uhCAk_agent";

const ENV: Env = { MIGRATION_NOTARY_BEARER_TOKEN: "test-token" };

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
        notaries: [
          { url: "https://n1a", api: "v1" },
          { url: "https://n1b", api: "v1" },
        ],
      },
      {
        dna_hash: v02,
        version: "alliance-v0.2.0",
        upgrades_from: v01,
        upgrade_targets: [v03],
        release_url:
          "https://github.com/unytco/unyt-sandbox/releases/tag/v0.2.0",
        published: true,
        notaries: [{ url: "https://n2", api: "v1" }],
      },
      {
        dna_hash: v03,
        version: "alliance-v0.3.0",
        upgrades_from: v02,
        published: true,
        notaries: [{ url: "https://n3", api: "v1" }],
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

/** A canned daemon 200: the three-field closing-summary package, bound from
 * `sourceDna` to `targetDna` (the close's single-landing target). */
const packageResp = (sourceDna: string, targetDna: string, marker: string) =>
  jsonResp(200, {
    payload: {
      source_dna_hash: sourceDna,
      target_dna_hash: targetDna,
      closing_state: {},
    },
    notary_signatures: [{ notary: "uhCAk_notary", signature: marker }],
    close_action: `close-${marker}`,
  });

const jsonResp = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });

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

describe("migrationOptions", () => {
  it("returns the immediate predecessor mid-chain", () => {
    const resp = migrationOptions(registry(), v03);
    expect(resp.status).toBe(200);
  });

  it("v0.3 returns all sources that reach it (skip: v0.1 and v0.2)", async () => {
    const b = await body(migrationOptions(registry(), v03));
    expect(b.options).toEqual([
      { from_dna_hash: v01, from_version: "alliance-v0.1.0" },
      { from_dna_hash: v02, from_version: "alliance-v0.2.0" },
    ]);
  });

  it("chain root (registered, no sources) returns empty options", async () => {
    const resp = migrationOptions(registry(), v01);
    expect(resp.status).toBe(200);
    expect((await body(resp)).options).toEqual([]);
  });

  it("unknown DNA errors like migrate (400 unknown_to_dna, not empty options)", async () => {
    const resp = migrationOptions(registry(), "uhC0k_unknown");
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unknown_to_dna");
  });

  it("missing to_dna_hash errors", async () => {
    const resp = migrationOptions(registry(), null);
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unknown_to_dna");
  });
});

// A GitHub releases listing (newest-first). draft/prerelease/rc entries prove the filter.
const ghReleases = (
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
          notaries: [{ url: "https://na", api: "v1" }],
        },
        {
          dna_hash: bDna,
          version: "b",
          upgrades_from: a,
          release_url: "https://example/b",
          published: true,
          notaries: [{ url: "https://nb", api: "v1" }],
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

// The customers-last split, end to end through the handlers: the SAME registry entry is served by
// /v1/migrate (the headless server open) while being invisible to /v1/update-check (the customer
// banner), until the publish phase flips `published`. This is the whole point of the visibility gate.
describe("updateCheck ⟂ migrate — the published (customers-last) split", () => {
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
          notaries: [{ url: "https://n1", api: "v1" }],
        },
        {
          dna_hash: v02,
          version: "alliance-v0.2.0",
          upgrades_from: v01,
          release_url:
            "https://github.com/unytco/unyt-sandbox/releases/tag/v0.2.0",
          published,
          notaries: [{ url: "https://n2", api: "v1" }],
        },
      ],
    });
  }

  it("update-check HIDES an unpublished successor (no banner, has_upgrade:false)", async () => {
    const b = await body(await updateCheck(gated(false), v01, null, noGh, ENV));
    expect(b).toEqual({ current_dna_hash: v01, has_upgrade: false });
  });

  it("migrate SERVES that same unpublished successor's close package (server open works pre-publish)", async () => {
    const f = mockFetch({ "https://n1": () => packageResp(v01, v02, "srv") });
    const resp = await migrate(
      gated(false),
      { from_dna_hash: v01, to_dna_hash: v02, agent_pubkey: AGENT },
      ENV,
      f,
    );
    expect(resp.status).toBe(200);
    expect(await body(resp)).toEqual({
      payload: { source_dna_hash: v01, target_dna_hash: v02, closing_state: {} },
      notary_signatures: [{ notary: "uhCAk_notary", signature: "srv" }],
      close_action: "close-srv",
    });
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
      ghReleases([{ tag: "v0.3.2" }, { tag: "v0.3.1" }, { tag: "v0.2.9" }]),
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
      ghReleases([{ tag: "v0.3.2", assets: [dmg, deb] }, { tag: "v0.3.1" }]),
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
      ghReleases([
        { tag: "v0.3.9", draft: true },
        { tag: "v0.3.8", prerelease: true },
        { tag: "v0.3.7-rc.1" },
        { tag: "v0.3.5" },
      ]),
    );
    const b = await body(
      await updateCheck(registry(), v03, "0.3.0", fetch, ENV),
    );
    expect(b.latest_build.version).toBe("0.3.5");
  });

  it("omits latest_build when the caller's lineage has no published build (never falsy)", async () => {
    const fetch = ghFetch(() => ghReleases([{ tag: "v0.9.0" }]));
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
          notaries: [{ url: "https://nf", api: "v1" }],
        },
        {
          dna_hash: to,
          version: "to",
          upgrades_from: from,
          release_url:
            "https://github.com/unytco/unyt-sandbox/releases/tag/v0.5.0",
          published: true,
          notaries: [{ url: "https://nt", api: "v1" }],
        },
      ],
    });
    const fetch = ghFetch(() =>
      ghReleases([{ tag: "v0.5.3" }, { tag: "v0.5.0" }]),
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
          notaries: [{ url: "https://nf", api: "v1" }],
        },
        {
          dna_hash: to,
          version: "to",
          upgrades_from: from,
          release_url:
            "https://github.com/unytco/unyt-sandbox/releases/tag/v0.5.0",
          published: true,
          notaries: [{ url: "https://nt", api: "v1" }],
        },
      ],
    });
    const fetch = ghFetch(() => ghReleases([{ tag: "v0.9.0" }])); // nothing on 0.5
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
        return ghReleases([{ tag: "v0.3.1" }]);
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

describe("migrate — pair validation", () => {
  const noFetch = mockFetch({});

  async function migrateWith(b: MigrateBody) {
    const resp = await migrate(registry(), b, ENV, noFetch);
    return { status: resp.status, code: (await body(resp)).error?.code };
  }

  it("rejects unknown to_dna", async () => {
    expect(
      await migrateWith({
        from_dna_hash: v01,
        to_dna_hash: "x",
        agent_pubkey: AGENT,
      }),
    ).toEqual({
      status: 400,
      code: "unknown_to_dna",
    });
  });

  it("rejects unknown from_dna", async () => {
    expect(
      await migrateWith({
        from_dna_hash: "x",
        to_dna_hash: v02,
        agent_pubkey: AGENT,
      }),
    ).toEqual({
      status: 400,
      code: "unknown_from_dna",
    });
  });

  it("rejects chain root as to_dna", async () => {
    expect(
      await migrateWith({
        from_dna_hash: v01,
        to_dna_hash: v01,
        agent_pubkey: AGENT,
      }),
    ).toEqual({
      status: 400,
      code: "to_is_chain_root",
    });
  });

  it("rejects a target not in the source's upgrade_targets (unreachable)", async () => {
    // x proves a path only to y, not to z (z is a real later version on the chain).
    const x = "uhC0k_x";
    const y = "uhC0k_y";
    const z = "uhC0k_z";
    const r = Registry.load({
      version: 1,
      dnas: [
        { dna_hash: x, version: "x", upgrade_targets: [y], notaries: [] },
        {
          dna_hash: y,
          version: "y",
          upgrades_from: x,
          upgrade_targets: [z],
          notaries: [],
        },
        { dna_hash: z, version: "z", upgrades_from: y, notaries: [] },
      ],
    });
    const resp = await migrate(
      r,
      { from_dna_hash: x, to_dna_hash: z, agent_pubkey: AGENT },
      ENV,
      noFetch,
    );
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unreachable_target");
  });
});

describe("migrate — notary dispatch + failover", () => {
  const goodPair = {
    from_dna_hash: v01,
    to_dna_hash: v02,
    agent_pubkey: AGENT,
  };

  it("returns the three-field package verbatim from a healthy notary", async () => {
    const f = mockFetch({
      "https://n1a": () => packageResp(v01, v02, "sig-a"),
    });
    // n1a is the only mock: whichever order the shuffle picks, the unmocked
    // candidate fails transiently and the loop lands on n1a.
    const resp = await migrate(registry(), goodPair, ENV, f);
    const b = await body(resp);
    expect(resp.status).toBe(200);
    expect(b).toEqual({
      payload: {
        source_dna_hash: v01,
        target_dna_hash: v02,
        closing_state: {},
      },
      notary_signatures: [{ notary: "uhCAk_notary", signature: "sig-a" }],
      close_action: "close-sig-a",
    });
  });

  it("calls the daemon at /{api}/fetch-close", async () => {
    const urls: string[] = [];
    const f = (async (input: RequestInfo | URL) => {
      urls.push(typeof input === "string" ? input : input.toString());
      return packageResp(v01, v02, "sig");
    }) as FetchLike;
    await migrate(registry(), goodPair, ENV, f, KEEP_ORDER);
    expect(urls[0]).toBe("https://n1a/v1/fetch-close");
  });

  it("spreads load: different seeds hit different first daemons", async () => {
    const first: string[] = [];
    const f = (async (input: RequestInfo | URL) => {
      first.push(typeof input === "string" ? input : input.toString());
      return packageResp(v01, v02, "sig");
    }) as FetchLike;
    await migrate(registry(), goodPair, ENV, f, KEEP_ORDER);
    await migrate(registry(), goodPair, ENV, f, REVERSE);
    expect(first[0].startsWith("https://n1a")).toBe(true);
    expect(first[1].startsWith("https://n1b")).toBe(true);
  });

  it("fails over to the next notary when the first errors transiently", async () => {
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(500, { error: { code: "unable_to_verify", message: "x" } }),
      "https://n1b": () => packageResp(v01, v02, "sig-b"),
    });
    const b = await body(
      await migrate(registry(), goodPair, ENV, f, KEEP_ORDER),
    );
    expect(b.notary_signatures[0].signature).toBe("sig-b");
  });

  it("all notaries unable_to_verify → 503 unable_to_verify", async () => {
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(500, { error: { code: "unable_to_verify", message: "x" } }),
      "https://n1b": () =>
        jsonResp(500, { error: { code: "unable_to_verify", message: "x" } }),
    });
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(503);
    expect((await body(resp)).error.code).toBe("unable_to_verify");
  });

  it("all notaries unreachable → 503 all_orgs_unhealthy", async () => {
    const f = mockFetch({}); // every fetch throws (transport failure)
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(503);
    expect((await body(resp)).error.code).toBe("all_orgs_unhealthy");
  });

  it("hard stop (no_close_found) propagates immediately without trying the next", async () => {
    let n1bCalled = false;
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(404, { error: { code: "no_close_found", message: "x" } }),
      "https://n1b": () => {
        n1bCalled = true;
        return packageResp(v01, v02, "should-not-be-used");
      },
    });
    const resp = await migrate(registry(), goodPair, ENV, f, KEEP_ORDER);
    expect(resp.status).toBe(404);
    expect((await body(resp)).error.code).toBe("no_close_found");
    expect(n1bCalled).toBe(false);
  });

  it("hard stop (warranted) propagates immediately without trying the next", async () => {
    let n1bCalled = false;
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(422, { error: { code: "warranted", message: "x" } }),
      "https://n1b": () => {
        n1bCalled = true;
        return packageResp(v01, v02, "should-not-be-used");
      },
    });
    const resp = await migrate(registry(), goodPair, ENV, f, KEEP_ORDER);
    expect(resp.status).toBe(422);
    expect((await body(resp)).error.code).toBe("warranted");
    expect(n1bCalled).toBe(false);
  });

  // C2 — failover aggregation: a config/auth/rate-limit fault is surfaced
  // distinctly (auth_failed → 502, rate_limited → 503) rather than collapsed
  // into a generic outage, and a single unable_to_verify wins the aggregate.
  it("all notaries auth_failed → 502 auth_failed (distinct, not unable_to_verify)", async () => {
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(401, { error: { code: "auth_failed", message: "x" } }),
      "https://n1b": () =>
        jsonResp(401, { error: { code: "auth_failed", message: "x" } }),
    });
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(502);
    expect((await body(resp)).error.code).toBe("auth_failed");
  });

  it("all notaries rate_limited → 503 rate_limited (distinct, not unable_to_verify)", async () => {
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(429, { error: { code: "rate_limited", message: "x" } }),
      "https://n1b": () =>
        jsonResp(429, { error: { code: "rate_limited", message: "x" } }),
    });
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(503);
    expect((await body(resp)).error.code).toBe("rate_limited");
  });

  it("auth_failed then unable_to_verify aggregates to 503 unable_to_verify", async () => {
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(401, { error: { code: "auth_failed", message: "x" } }),
      "https://n1b": () =>
        jsonResp(503, { error: { code: "unable_to_verify", message: "x" } }),
    });
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(503);
    expect((await body(resp)).error.code).toBe("unable_to_verify");
  });

  // B5 — a daemon bad_request is a client-side error every notary rejects the
  // same way: hard-stop the loop (4xx) instead of fanning out as unhealthy.
  it("daemon bad_request hard-stops without trying the next notary", async () => {
    let n1bCalled = false;
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(400, {
          error: { code: "bad_request", message: "bad pubkey" },
        }),
      "https://n1b": () => {
        n1bCalled = true;
        return packageResp(v01, v02, "should-not-be-used");
      },
    });
    const resp = await migrate(registry(), goodPair, ENV, f, KEEP_ORDER);
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("bad_request");
    expect(n1bCalled).toBe(false);
  });

  // B2 — a notary that returns a payload for the wrong source DNA is misconfigured;
  // reject as internal rather than handing a wrong-DNA package to the app.
  it("wrong-source-DNA payload (source_dna_hash mismatch) → 500 internal", async () => {
    const f = mockFetch({
      "https://n1a": () => packageResp(v02, v02, "sig"),
      "https://n1b": () => packageResp(v02, v02, "sig"),
    });
    const resp = await migrate(registry(), goodPair, ENV, f);
    const b = await body(resp);
    expect(resp.status).toBe(500);
    expect(b.error.code).toBe("internal");
    expect(b.error.details.expected_dna_hash).toBe(v01);
    expect(b.error.details.got_dna_hash).toBe(v02);
  });

  // Fail-closed: a well-formed close ALWAYS binds a source_dna_hash (rave_engine's
  // SummaryStatePayload.source_dna_hash is a required field), so a package that omits
  // it is unbound and must NOT be forwarded — reject as internal, surfacing got=null.
  it("a package missing source_dna_hash → 500 internal (unbound, not forwarded)", async () => {
    const noSource = () =>
      jsonResp(200, {
        payload: { target_dna_hash: v02, closing_state: {} }, // no source_dna_hash
        notary_signatures: [{ notary: "x", signature: "s" }],
        close_action: "c",
      });
    const f = mockFetch({ "https://n1a": noSource, "https://n1b": noSource });
    const resp = await migrate(registry(), goodPair, ENV, f);
    const b = await body(resp);
    expect(resp.status).toBe(500);
    expect(b.error.code).toBe("internal");
    expect(b.error.details.expected_dna_hash).toBe(v01);
    expect(b.error.details.got_dna_hash).toBe(null);
  });

  // A malformed source_dna_hash (not a 39-byte HoloHash array) normalizes to
  // undefined and is rejected the same way — never forwarded.
  it("a malformed byte-array source_dna_hash → 500 internal (not forwarded)", async () => {
    const badArray = () =>
      jsonResp(200, {
        payload: {
          source_dna_hash: [1, 2, 3], // too short to be a 39-byte HoloHash
          target_dna_hash: v02,
          closing_state: {},
        },
        notary_signatures: [{ notary: "x", signature: "s" }],
        close_action: "c",
      });
    const f = mockFetch({ "https://n1a": badArray, "https://n1b": badArray });
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(500);
    expect((await body(resp)).error.code).toBe("internal");
  });

  // The raw-byte-array hash path still works end to end after the 39-byte tightening:
  // a source delivered as a 39-byte HoloHash array normalizes to the registry b64 and
  // is accepted. Node's base64url is an independent oracle for the expected b64.
  it("accepts a source_dna_hash delivered as a raw 39-byte HoloHash array", async () => {
    const srcBytes = [0x84, 0x2d, 0x24, ...Array(32).fill(7), 0, 0, 0, 0]; // 3+32+4 = 39
    const srcB64 = "u" + Buffer.from(srcBytes).toString("base64url");
    const to = "uhC0k_to_bytes";
    const r = Registry.load({
      version: 1,
      dnas: [
        {
          dna_hash: srcB64,
          version: "src",
          upgrade_targets: [to],
          notaries: [{ url: "https://nb", api: "v1" }],
        },
        {
          dna_hash: to,
          version: "to",
          upgrades_from: srcB64,
          notaries: [{ url: "https://nt", api: "v1" }],
        },
      ],
    });
    const f = mockFetch({
      "https://nb": () =>
        jsonResp(200, {
          payload: {
            source_dna_hash: srcBytes,
            target_dna_hash: to,
            closing_state: {},
          },
          notary_signatures: [{ notary: "x", signature: "bytes-ok" }],
          close_action: "c",
        }),
    });
    const resp = await migrate(
      r,
      { from_dna_hash: srcB64, to_dna_hash: to, agent_pubkey: AGENT },
      ENV,
      f,
    );
    expect(resp.status).toBe(200);
    expect((await body(resp)).notary_signatures[0].signature).toBe("bytes-ok");
  });

  // A source-specific fault must NOT abort discovery of the OTHER candidate sources
  // (a single misconfigured/erroring notary used to propagate and strand a real migrate).
  it("discovery: a misconfigured source (internal) does NOT abort — a sibling source still yields the close", async () => {
    // from OMITTED → discover sources reaching v03 = [v01, v02]. v01's daemons serve the
    // wrong cell (source_dna_hash v02 != v01 → internal); v02's daemon holds the real close
    // bound to v03. The bad source must not abort discovery before v02 is tried.
    const f = mockFetch({
      "https://n1a": () => packageResp(v02, v03, "wrong-cell"),
      "https://n1b": () => packageResp(v02, v03, "wrong-cell"),
      "https://n2": () => packageResp(v02, v03, "real"),
    });
    const resp = await migrate(
      registry(),
      { to_dna_hash: v03, agent_pubkey: AGENT },
      ENV,
      f,
    );
    expect(resp.status).toBe(200);
    expect((await body(resp)).notary_signatures[0].signature).toBe("real");
  });

  // A captured wrong-cell `internal` keeps its diagnostic details when surfaced at the end.
  it("wrong-source-DNA across ALL candidate sources → 500 internal, details preserved", async () => {
    const f = mockFetch({
      "https://n1a": () => packageResp(v02, v03, "x"), // queried as v01 → mismatch
      "https://n1b": () => packageResp(v02, v03, "x"),
      "https://n2": () => packageResp(v03, v03, "x"), // queried as v02 → mismatch
    });
    const resp = await migrate(
      registry(),
      { to_dna_hash: v03, agent_pubkey: AGENT },
      ENV,
      f,
    );
    expect(resp.status).toBe(500);
    const b = await body(resp);
    expect(b.error.code).toBe("internal");
    expect(b.error.details.got_dna_hash).toBe(v02); // the first captured fault's details
  });

  // A registered source with zero notaries is OUR registry misconfiguration — a 5xx
  // config fault, never folded into a 503 transient the agent would retry forever.
  it("a candidate source with zero registered notaries → 500 internal (config fault)", async () => {
    const raw: RawRegistry = {
      version: 1,
      dnas: [
        { dna_hash: v01, version: "v1", upgrade_targets: [v02], notaries: [] },
        {
          dna_hash: v02,
          version: "v2",
          upgrades_from: v01,
          notaries: [{ url: "https://n2", api: "v1" }],
        },
      ],
    };
    const resp = await migrate(
      Registry.load(raw),
      { to_dna_hash: v02, agent_pubkey: AGENT },
      ENV,
      mockFetch({}),
    );
    expect(resp.status).toBe(500);
    expect((await body(resp)).error.code).toBe("internal");
  });

  // A daemon internal error must surface AS `internal`, not be masked as an outage.
  it("all notaries return a daemon internal error → 500 internal (not all_orgs_unhealthy)", async () => {
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(500, { error: { code: "internal", message: "daemon boom" } }),
      "https://n1b": () =>
        jsonResp(500, { error: { code: "internal", message: "daemon boom" } }),
    });
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(500);
    expect((await body(resp)).error.code).toBe("internal");
  });

  // A complete package carrying NO target_dna_hash is a malformed/old-shape package
  // (a daemon fault) — surfaced, not silently skipped as "no close".
  it("a complete package missing target_dna_hash → 500 internal (not a silent 404)", async () => {
    const noTarget = () =>
      jsonResp(200, {
        payload: { source_dna_hash: v01, closing_state: {} }, // no target_dna_hash
        notary_signatures: [{ notary: "x", signature: "s" }],
        close_action: "c",
      });
    const f = mockFetch({ "https://n1a": noTarget, "https://n1b": noTarget });
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(500);
    expect((await body(resp)).error.code).toBe("internal");
  });

  // A config FAULT must outrank a co-occurring transient — a captured wrong-cell internal
  // is surfaced (with its details), never masked as the sibling source's 503.
  it("a wrong-cell internal on one source is NOT masked by a transient on another (500 internal, details kept)", async () => {
    const f = mockFetch({
      "https://n1a": () => packageResp(v02, v03, "x"), // v01 queried → source mismatch → internal
      "https://n1b": () => packageResp(v02, v03, "x"),
      "https://n2": () =>
        jsonResp(503, { error: { code: "unable_to_verify", message: "x" } }), // v02 transient
    });
    const resp = await migrate(
      registry(),
      { to_dna_hash: v03, agent_pubkey: AGENT },
      ENV,
      f,
    );
    expect(resp.status).toBe(500);
    const b = await body(resp);
    expect(b.error.code).toBe("internal");
    expect(b.error.details.got_dna_hash).toBe(v02);
  });

  // One misconfigured daemon of a source must not abandon a healthy SIBLING daemon on the
  // same source that holds the real close.
  it("a wrong-cell daemon does not abandon its healthy sibling on the same source", async () => {
    const f = mockFetch({
      "https://n1a": () => packageResp(v02, v02, "wrong-cell"), // v01 queried → mismatch → internal
      "https://n1b": () => packageResp(v01, v02, "real"), // healthy sibling holds the close
    });
    // KEEP_ORDER tries n1a (wrong-cell) first; it must fall through to n1b, not abort.
    const resp = await migrate(registry(), goodPair, ENV, f, KEEP_ORDER);
    expect(resp.status).toBe(200);
    expect((await body(resp)).notary_signatures[0].signature).toBe("real");
  });

  // B3 — a healthy-status notary with a malformed (non-JSON) success body must
  // not throw out of the loop; it fails over to the next candidate.
  it("malformed 200 body fails over to the next notary", async () => {
    const f = mockFetch({
      "https://n1a": () =>
        new Response("not json{", {
          status: 200,
          headers: { "content-type": "application/json" },
        }),
      "https://n1b": () => packageResp(v01, v02, "sig-b"),
    });
    const b = await body(
      await migrate(registry(), goodPair, ENV, f, KEEP_ORDER),
    );
    expect(b.notary_signatures[0].signature).toBe("sig-b");
  });

  // A 200 missing a package field (truncated proxy response, buggy daemon) is
  // as malformed as non-JSON: fail over instead of forwarding an incomplete
  // package to the app.
  it("200 body missing a package field fails over to the next notary", async () => {
    const f = mockFetch({
      "https://n1a": () => jsonResp(200, { payload: { dna_hash: v01 } }), // no signatures/close_action
      "https://n1b": () => packageResp(v01, v02, "sig-b"),
    });
    const b = await body(
      await migrate(registry(), goodPair, ENV, f, KEEP_ORDER),
    );
    expect(b.notary_signatures[0].signature).toBe("sig-b");
  });

  it("200 body with null package fields fails over to the next notary", async () => {
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(200, {
          payload: null,
          notary_signatures: null,
          close_action: null,
        }),
      "https://n1b": () => packageResp(v01, v02, "sig-b"),
    });
    const b = await body(
      await migrate(registry(), goodPair, ENV, f, KEEP_ORDER),
    );
    expect(b.notary_signatures[0].signature).toBe("sig-b");
  });
});

describe("migrate — skip routing + discovery", () => {
  const skipPair = (from: string | undefined, to: string) => ({
    ...(from ? { from_dna_hash: from } : {}),
    to_dna_hash: to,
    agent_pubkey: AGENT,
  });

  it("a supplied source reaching the target via skip (v0.1 → v0.3) succeeds", async () => {
    const f = mockFetch({
      "https://n1a": () => packageResp(v01, v03, "skip"),
      "https://n1b": () => packageResp(v01, v03, "skip"),
    });
    const resp = await migrate(registry(), skipPair(v01, v03), ENV, f);
    expect(resp.status).toBe(200);
    expect((await body(resp)).close_action).toBe("close-skip");
  });

  it("discovers the source when from is omitted (close bound to the target)", async () => {
    // sources reaching v0.3 are [v0.1, v0.2]; the agent closed on v0.1 bound to v0.3.
    const f = mockFetch({
      "https://n1a": () => packageResp(v01, v03, "found"),
      "https://n1b": () => packageResp(v01, v03, "found"),
      "https://n2": () =>
        jsonResp(404, { error: { code: "no_close_found", message: "x" } }),
    });
    const resp = await migrate(
      registry(),
      skipPair(undefined, v03),
      ENV,
      f,
      KEEP_ORDER,
    );
    expect(resp.status).toBe(200);
    expect((await body(resp)).close_action).toBe("close-found");
  });

  it("ignores a stale close bound to a different target during discovery", async () => {
    // The only close (on v0.1) is bound to v0.2 — stale for a v0.3 migrate; no
    // source yields a close bound to v0.3 → no_close_found.
    const f = mockFetch({
      "https://n1a": () => packageResp(v01, v02, "stale"),
      "https://n1b": () => packageResp(v01, v02, "stale"),
      "https://n2": () =>
        jsonResp(404, { error: { code: "no_close_found", message: "x" } }),
    });
    const resp = await migrate(
      registry(),
      skipPair(undefined, v03),
      ENV,
      f,
      KEEP_ORDER,
    );
    expect(resp.status).toBe(404);
    expect((await body(resp)).error.code).toBe("no_close_found");
  });

  it("from omitted + no registered source reaches the target → unreachable_target", async () => {
    const base = "uhC0k_base";
    const island = "uhC0k_island";
    const r = Registry.load({
      version: 1,
      dnas: [
        { dna_hash: base, version: "base", notaries: [] },
        {
          dna_hash: island,
          version: "island",
          upgrades_from: base,
          notaries: [],
        },
      ],
    });
    const resp = await migrate(
      r,
      skipPair(undefined, island),
      ENV,
      mockFetch({}),
    );
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unreachable_target");
  });
});

describe("migrate — request validation (B6)", () => {
  it("missing required fields → 400 bad_request", async () => {
    const resp = await migrate(
      registry(),
      { from_dna_hash: v01 },
      ENV,
      mockFetch({}),
    );
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("bad_request");
  });
});
