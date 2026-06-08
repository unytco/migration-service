import { describe, expect, it } from "vitest";
import { Registry, type RawRegistry } from "../src/registry";
import { migrate, migrationOptions, updateCheck, type MigrateBody } from "../src/handlers";
import type { Env, FetchLike } from "../src/notary";

const v01 = "uhC0k_v01";
const v02 = "uhC0k_v02";
const v03 = "uhC0k_v03";
const AGENT = "uhCAk_agent";

const ENV: Env = { MIGRATION_NOTARY_BEARER_TOKEN: "test-token" };

function registry(): Registry {
  const raw: RawRegistry = {
    version: 1,
    dnas: [
      { dna_hash: v01, version: "alliance-v0.1.0", notaries: [{ url: "https://n1a" }, { url: "https://n1b" }] },
      {
        dna_hash: v02,
        version: "alliance-v0.2.0",
        upgrades_from: v01,
        release_url: "https://github.com/unytco/unyt-sandbox/releases/tag/v0.2.0",
        notaries: [{ url: "https://n2" }],
      },
      { dna_hash: v03, version: "alliance-v0.3.0", upgrades_from: v02, notaries: [{ url: "https://n3" }] },
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

const jsonResp = (status: number, body: unknown) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

async function body(resp: Response): Promise<any> {
  return resp.json();
}

describe("migrationOptions", () => {
  it("returns the immediate predecessor mid-chain", () => {
    const resp = migrationOptions(registry(), v03);
    expect(resp.status).toBe(200);
  });

  it("v0.3 returns v0.2 (not v0.1)", async () => {
    const b = await body(migrationOptions(registry(), v03));
    expect(b.options).toEqual([{ from_dna_hash: v02, from_version: "alliance-v0.2.0" }]);
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
  it("returns the successor + release_url for the current DNA", async () => {
    const b = await body(updateCheck(registry(), v01));
    expect(b).toEqual({
      current_dna_hash: v01,
      has_upgrade: true,
      successor: {
        to_dna_hash: v02,
        to_version: "alliance-v0.2.0",
        release_url: "https://github.com/unytco/unyt-sandbox/releases/tag/v0.2.0",
      },
    });
  });

  it("omits release_url when the successor has none", async () => {
    const b = await body(updateCheck(registry(), v02)); // v03 successor has no release_url
    expect(b.has_upgrade).toBe(true);
    expect(b.successor.to_dna_hash).toBe(v03);
    expect("release_url" in b.successor).toBe(false);
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

  it("rejects a non-predecessor (skip-version)", async () => {
    const resp = await migrate(
      registry(),
      { from_dna_hash: v01, to_dna_hash: v03, agent_pubkey: AGENT },
      ENV,
      noFetch,
    );
    const b = await body(resp);
    expect(resp.status).toBe(400);
    expect(b.error.code).toBe("not_registered_predecessor");
    expect(b.error.details.expected_from_dna_hash).toBe(v02);
  });
});

describe("migrate — notary dispatch + failover", () => {
  const goodPair = { from_dna_hash: v01, to_dna_hash: v02, agent_pubkey: AGENT };

  it("returns payload + signature from the first healthy notary", async () => {
    const f = mockFetch({
      "https://n1a": () => jsonResp(200, { payload: { dna_hash: v01, closing_state: {} }, signature: "sig" }),
    });
    const resp = await migrate(registry(), goodPair, ENV, f);
    const b = await body(resp);
    expect(resp.status).toBe(200);
    expect(b.signature).toBe("sig");
  });

  it("fails over to the second notary when the first errors transiently", async () => {
    const f = mockFetch({
      "https://n1a": () => jsonResp(500, { error: { code: "unable_to_verify", message: "x" } }),
      "https://n1b": () => jsonResp(200, { payload: { dna_hash: v01 }, signature: "sig2" }),
    });
    const b = await body(await migrate(registry(), goodPair, ENV, f));
    expect(b.signature).toBe("sig2");
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
        return jsonResp(200, { payload: {}, signature: "should-not-be-used" });
      },
    });
    const resp = await migrate(registry(), goodPair, ENV, f);
    expect(resp.status).toBe(404);
    expect((await body(resp)).error.code).toBe("no_close_found");
    expect(n1bCalled).toBe(false);
  });
});
