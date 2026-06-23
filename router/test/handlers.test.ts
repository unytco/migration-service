import { describe, expect, it } from "vitest";
import { Registry, type RawRegistry } from "../src/registry";
import { migrate, migrationOptions, shuffled, updateCheck, type MigrateBody } from "../src/handlers";
import type { Env, FetchLike } from "../src/notary";

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
        release_url: "https://github.com/unytco/unyt-sandbox/releases/tag/v0.2.0",
        notaries: [{ url: "https://n2", api: "v1" }],
      },
      {
        dna_hash: v03,
        version: "alliance-v0.3.0",
        upgrades_from: v02,
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
    payload: { source_dna_hash: sourceDna, target_dna_hash: targetDna, closing_state: {} },
    notary_signatures: [{ notary: "uhCAk_notary", signature: marker }],
    close_action: `close-${marker}`,
  });

const jsonResp = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

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

  it("chain root returns empty options", async () => {
    const b = await body(migrationOptions(registry(), v01));
    expect(b.options).toEqual([]);
  });

  it("unknown DNA returns empty options", async () => {
    const b = await body(migrationOptions(registry(), "uhC0k_unknown"));
    expect(b.options).toEqual([]);
  });

  it("missing to_dna_hash errors", async () => {
    const resp = migrationOptions(registry(), null);
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unknown_to_dna");
  });
});

describe("updateCheck", () => {
  it("returns the FURTHEST target (skips past v0.2 to v0.3), omitting release_url when absent", async () => {
    const b = await body(updateCheck(registry(), v01));
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
        { dna_hash: a, version: "a", upgrade_targets: [bDna], notaries: [{ url: "https://na", api: "v1" }] },
        {
          dna_hash: bDna,
          version: "b",
          upgrades_from: a,
          release_url: "https://example/b",
          notaries: [{ url: "https://nb", api: "v1" }],
        },
      ],
    });
    const resp = await body(updateCheck(r, a));
    expect(resp.target).toEqual({ to_dna_hash: bDna, to_version: "b", release_url: "https://example/b" });
  });

  it("chain tip has no upgrade", async () => {
    const b = await body(updateCheck(registry(), v03));
    expect(b).toEqual({ current_dna_hash: v03, has_upgrade: false });
  });

  it("unknown current DNA has no upgrade", async () => {
    const b = await body(updateCheck(registry(), "uhC0k_unknown"));
    expect(b).toEqual({ current_dna_hash: "uhC0k_unknown", has_upgrade: false });
  });

  it("missing current_dna_hash errors", async () => {
    const resp = updateCheck(registry(), null);
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unknown_current_dna");
  });
});

describe("migrate — pair validation", () => {
  const noFetch = mockFetch({});

  async function migrateWith(b: MigrateBody) {
    const resp = await migrate(registry(), b, ENV, noFetch);
    return { status: resp.status, code: (await body(resp)).error?.code };
  }

  it("rejects unknown to_dna", async () => {
    expect(await migrateWith({ from_dna_hash: v01, to_dna_hash: "x", agent_pubkey: AGENT })).toEqual({
      status: 400,
      code: "unknown_to_dna",
    });
  });

  it("rejects unknown from_dna", async () => {
    expect(await migrateWith({ from_dna_hash: "x", to_dna_hash: v02, agent_pubkey: AGENT })).toEqual({
      status: 400,
      code: "unknown_from_dna",
    });
  });

  it("rejects chain root as to_dna", async () => {
    expect(await migrateWith({ from_dna_hash: v01, to_dna_hash: v01, agent_pubkey: AGENT })).toEqual({
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
        { dna_hash: y, version: "y", upgrades_from: x, upgrade_targets: [z], notaries: [] },
        { dna_hash: z, version: "z", upgrades_from: y, notaries: [] },
      ],
    });
    const resp = await migrate(r, { from_dna_hash: x, to_dna_hash: z, agent_pubkey: AGENT }, ENV, noFetch);
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unreachable_target");
  });
});

describe("migrate — notary dispatch + failover", () => {
  const goodPair = { from_dna_hash: v01, to_dna_hash: v02, agent_pubkey: AGENT };

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
      payload: { source_dna_hash: v01, target_dna_hash: v02, closing_state: {} },
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
      "https://n1a": () => jsonResp(500, { error: { code: "unable_to_verify", message: "x" } }),
      "https://n1b": () => packageResp(v01, v02, "sig-b"),
    });
    const b = await body(await migrate(registry(), goodPair, ENV, f, KEEP_ORDER));
    expect(b.notary_signatures[0].signature).toBe("sig-b");
  });

  it("all notaries unable_to_verify → 503 unable_to_verify", async () => {
    const f = mockFetch({
      "https://n1a": () => jsonResp(500, { error: { code: "unable_to_verify", message: "x" } }),
      "https://n1b": () => jsonResp(500, { error: { code: "unable_to_verify", message: "x" } }),
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
      "https://n1a": () => jsonResp(404, { error: { code: "no_close_found", message: "x" } }),
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
      "https://n1a": () => jsonResp(422, { error: { code: "warranted", message: "x" } }),
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
      "https://n1a": () => jsonResp(401, { error: { code: "auth_failed", message: "x" } }),
      "https://n1b": () => jsonResp(401, { error: { code: "auth_failed", message: "x" } }),
    });
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(502);
    expect((await body(resp)).error.code).toBe("auth_failed");
  });

  it("all notaries rate_limited → 503 rate_limited (distinct, not unable_to_verify)", async () => {
    const f = mockFetch({
      "https://n1a": () => jsonResp(429, { error: { code: "rate_limited", message: "x" } }),
      "https://n1b": () => jsonResp(429, { error: { code: "rate_limited", message: "x" } }),
    });
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(503);
    expect((await body(resp)).error.code).toBe("rate_limited");
  });

  it("auth_failed then unable_to_verify aggregates to 503 unable_to_verify", async () => {
    const f = mockFetch({
      "https://n1a": () => jsonResp(401, { error: { code: "auth_failed", message: "x" } }),
      "https://n1b": () => jsonResp(503, { error: { code: "unable_to_verify", message: "x" } }),
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
      "https://n1a": () => jsonResp(400, { error: { code: "bad_request", message: "bad pubkey" } }),
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
    const resp = await migrate(registry(), { to_dna_hash: v03, agent_pubkey: AGENT }, ENV, f);
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
    const resp = await migrate(registry(), { to_dna_hash: v03, agent_pubkey: AGENT }, ENV, f);
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
        { dna_hash: v02, version: "v2", upgrades_from: v01, notaries: [{ url: "https://n2", api: "v1" }] },
      ],
    };
    const resp = await migrate(Registry.load(raw), { to_dna_hash: v02, agent_pubkey: AGENT }, ENV, mockFetch({}));
    expect(resp.status).toBe(500);
    expect((await body(resp)).error.code).toBe("internal");
  });

  // A daemon internal error must surface AS `internal`, not be masked as an outage.
  it("all notaries return a daemon internal error → 500 internal (not all_orgs_unhealthy)", async () => {
    const f = mockFetch({
      "https://n1a": () => jsonResp(500, { error: { code: "internal", message: "daemon boom" } }),
      "https://n1b": () => jsonResp(500, { error: { code: "internal", message: "daemon boom" } }),
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

  // B3 — a healthy-status notary with a malformed (non-JSON) success body must
  // not throw out of the loop; it fails over to the next candidate.
  it("malformed 200 body fails over to the next notary", async () => {
    const f = mockFetch({
      "https://n1a": () =>
        new Response("not json{", { status: 200, headers: { "content-type": "application/json" } }),
      "https://n1b": () => packageResp(v01, v02, "sig-b"),
    });
    const b = await body(await migrate(registry(), goodPair, ENV, f, KEEP_ORDER));
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
    const b = await body(await migrate(registry(), goodPair, ENV, f, KEEP_ORDER));
    expect(b.notary_signatures[0].signature).toBe("sig-b");
  });

  it("200 body with null package fields fails over to the next notary", async () => {
    const f = mockFetch({
      "https://n1a": () =>
        jsonResp(200, { payload: null, notary_signatures: null, close_action: null }),
      "https://n1b": () => packageResp(v01, v02, "sig-b"),
    });
    const b = await body(await migrate(registry(), goodPair, ENV, f, KEEP_ORDER));
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
      "https://n2": () => jsonResp(404, { error: { code: "no_close_found", message: "x" } }),
    });
    const resp = await migrate(registry(), skipPair(undefined, v03), ENV, f, KEEP_ORDER);
    expect(resp.status).toBe(200);
    expect((await body(resp)).close_action).toBe("close-found");
  });

  it("ignores a stale close bound to a different target during discovery", async () => {
    // The only close (on v0.1) is bound to v0.2 — stale for a v0.3 migrate; no
    // source yields a close bound to v0.3 → no_close_found.
    const f = mockFetch({
      "https://n1a": () => packageResp(v01, v02, "stale"),
      "https://n1b": () => packageResp(v01, v02, "stale"),
      "https://n2": () => jsonResp(404, { error: { code: "no_close_found", message: "x" } }),
    });
    const resp = await migrate(registry(), skipPair(undefined, v03), ENV, f, KEEP_ORDER);
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
        { dna_hash: island, version: "island", upgrades_from: base, notaries: [] },
      ],
    });
    const resp = await migrate(r, skipPair(undefined, island), ENV, mockFetch({}));
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unreachable_target");
  });
});

describe("migrate — request validation (B6)", () => {
  it("missing required fields → 400 bad_request", async () => {
    const resp = await migrate(registry(), { from_dna_hash: v01 }, ENV, mockFetch({}));
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("bad_request");
  });
});
