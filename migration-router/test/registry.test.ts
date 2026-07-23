import { describe, expect, it } from "vitest";
import { Registry, type RawRegistry } from "../src/registry";

const v01 = "uhC0k_v01";
const v02 = "uhC0k_v02";
const v03 = "uhC0k_v03";

function chain(): RawRegistry {
  return {
    version: 1,
    dnas: [
      {
        dna_hash: v01,
        version: "alliance-v0.1.0",
        notaries: [{ url: "https://n1", api: "v1" }],
      },
      {
        dna_hash: v02,
        version: "alliance-v0.2.0",
        upgrades_from: v01,
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
}

/** A skip-enabled chain: v01 proves a path to both v02 and v03; v02 to v03.
 * v02 + v03 are `published` (customer-visible) so `furthestTargetOf` surfaces them — the
 * customers-last gate is exercised separately by the `published` tests below. */
function skipChain(): RawRegistry {
  return {
    version: 1,
    dnas: [
      {
        dna_hash: v01,
        version: "alliance-v0.1.0",
        upgrade_targets: [v02, v03],
        notaries: [{ url: "https://n1", api: "v1" }],
      },
      {
        dna_hash: v02,
        version: "alliance-v0.2.0",
        upgrades_from: v01,
        upgrade_targets: [v03],
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
}

describe("Registry.load", () => {
  it("loads a valid linear chain", () => {
    const r = Registry.load(chain());
    expect(r.get(v02)?.version).toBe("alliance-v0.2.0");
  });

  it("rejects an unsupported schema version", () => {
    expect(() => Registry.load({ ...chain(), version: 99 })).toThrow(
      /unsupported registry version/,
    );
  });

  it("rejects a duplicate dna_hash", () => {
    const raw = chain();
    raw.dnas.push({ dna_hash: v01, version: "dup", notaries: [] });
    expect(() => Registry.load(raw)).toThrow(/duplicate dna_hash/);
  });

  it("rejects a missing version field", () => {
    const raw = chain();
    // @ts-expect-error intentionally omit version
    raw.dnas[0].version = undefined;
    expect(() => Registry.load(raw)).toThrow(/missing version/);
  });

  it("rejects an unresolved upgrades_from", () => {
    const raw = chain();
    raw.dnas[1].upgrades_from = "uhC0k_missing";
    expect(() => Registry.load(raw)).toThrow(/does not resolve/);
  });

  // The router must know where to reach each daemon and which API to speak
  // from the registry alone — a deficient notary entry fails at startup,
  // never at request time.

  it("rejects a notary entry with a non-https url", () => {
    const raw = chain();
    raw.dnas[0].notaries[0].url = "http://n1";
    expect(() => Registry.load(raw)).toThrow(/notary url must be https/);
  });

  // Local-testnet mode (allowHttpNotaries — used only by the index.local.ts entry
  // point): plain-http daemons on container IPs are admitted, and NOTHING else is
  // relaxed. The deployed default must stay https-only even when an options object
  // is passed.

  it("local mode: admits an http:// notary url under allowHttpNotaries", () => {
    const raw = chain();
    raw.dnas[0].notaries[0].url = "http://10.87.0.12:8790";
    const reg = Registry.load(raw, { allowHttpNotaries: true });
    expect(reg.get(raw.dnas[0].dna_hash)?.notaries[0].url).toBe(
      "http://10.87.0.12:8790",
    );
  });

  it("local mode: an explicit options object WITHOUT the flag still rejects http", () => {
    const raw = chain();
    raw.dnas[0].notaries[0].url = "http://10.87.0.12:8790";
    expect(() => Registry.load(raw, {})).toThrow(/notary url must be https/);
    expect(() => Registry.load(raw, { allowHttpNotaries: false })).toThrow(
      /notary url must be https/,
    );
  });

  it("local mode: a non-http(s) scheme is still rejected with the flag on", () => {
    const raw = chain();
    raw.dnas[0].notaries[0].url = "ftp://10.87.0.12:8790";
    expect(() => Registry.load(raw, { allowHttpNotaries: true })).toThrow(
      /notary url must be https/,
    );
  });

  it("local mode: every other invariant is still enforced with the flag on", () => {
    const raw = chain();
    raw.dnas[0].notaries[0].api = "v9";
    expect(() => Registry.load(raw, { allowHttpNotaries: true })).toThrow(
      /unsupported notary api/,
    );
  });

  it("rejects a notary entry with a missing url", () => {
    const raw = chain();
    // @ts-expect-error intentionally omit url
    raw.dnas[0].notaries[0].url = undefined;
    expect(() => Registry.load(raw)).toThrow(/notary url must be https/);
  });

  it("rejects a notary entry with a missing api", () => {
    const raw = chain();
    // @ts-expect-error intentionally omit api
    raw.dnas[0].notaries[0].api = undefined;
    expect(() => Registry.load(raw)).toThrow(/unsupported notary api/);
  });

  it("rejects a notary entry with an unsupported api", () => {
    const raw = chain();
    raw.dnas[0].notaries[0].api = "v9";
    expect(() => Registry.load(raw)).toThrow(/unsupported notary api v9/);
  });

  it("rejects a cycle", () => {
    const raw: RawRegistry = {
      version: 1,
      dnas: [
        { dna_hash: "a", version: "a", upgrades_from: "b", notaries: [] },
        { dna_hash: "b", version: "b", upgrades_from: "a", notaries: [] },
      ],
    };
    expect(() => Registry.load(raw)).toThrow(/cycle/);
  });

  it("successorOf walks one step forward", () => {
    const r = Registry.load(chain());
    expect(r.successorOf(v01)?.dna_hash).toBe(v02); // not v03
    expect(r.successorOf(v03)).toBeUndefined(); // chain tip
    expect(r.successorOf("unknown")).toBeUndefined();
  });

  it("rejects a fork (two successors of one DNA)", () => {
    const raw = chain();
    raw.dnas.push({
      dna_hash: "uhC0k_v02b",
      version: "alliance-v0.2.0b",
      upgrades_from: v01,
      notaries: [],
    });
    expect(() => Registry.load(raw)).toThrow(/multiple successors/);
  });

  it("accepts upgrade_targets that are forward descendants", () => {
    const r = Registry.load(skipChain());
    expect(r.get(v01)?.upgrade_targets).toEqual([v02, v03]);
  });

  it("rejects an upgrade_target that does not resolve", () => {
    const raw = skipChain();
    raw.dnas[0].upgrade_targets = [v02, "uhC0k_missing"];
    expect(() => Registry.load(raw)).toThrow(
      /upgrade_target .* does not resolve/,
    );
  });

  it("rejects an upgrade_target that is not a forward descendant", () => {
    // v02 cannot target v01 (its predecessor) — only forward descendants are valid.
    const raw = skipChain();
    raw.dnas[1].upgrade_targets = [v01];
    expect(() => Registry.load(raw)).toThrow(/not a forward descendant/);
  });

  it("rejects a duplicate upgrade_target", () => {
    const raw = skipChain();
    raw.dnas[0].upgrade_targets = [v02, v02];
    expect(() => Registry.load(raw)).toThrow(/duplicate upgrade_target/);
  });

  it("furthestTargetOf returns the deepest proven descendant", () => {
    const r = Registry.load(skipChain());
    expect(r.furthestTargetOf(v01)?.dna_hash).toBe(v03); // [v02,v03] → furthest v03
    expect(r.furthestTargetOf(v02)?.dna_hash).toBe(v03); // [v03] → v03
    expect(r.furthestTargetOf(v03)).toBeUndefined(); // tip, no targets
    expect(r.furthestTargetOf("unknown")).toBeUndefined();
  });

  it("furthestTargetOf honours a nearer-only target list", () => {
    const raw = skipChain();
    raw.dnas[0].upgrade_targets = [v02]; // v01 proven only to v02, not v03
    const r = Registry.load(raw);
    expect(r.furthestTargetOf(v01)?.dna_hash).toBe(v02);
  });

  it("sourcesReaching returns every source listing the target", () => {
    const r = Registry.load(skipChain());
    expect(r.sourcesReaching(v03).map((d) => d.dna_hash)).toEqual([v01, v02]);
    expect(r.sourcesReaching(v02).map((d) => d.dna_hash)).toEqual([v01]);
    expect(r.sourcesReaching(v01)).toEqual([]);
  });

  it("reaches reflects the upgrade_targets list", () => {
    const r = Registry.load(skipChain());
    expect(r.reaches(v01, v03)).toBe(true);
    expect(r.reaches(v02, v03)).toBe(true);
    expect(r.reaches(v02, v01)).toBe(false);
    expect(r.reaches("unknown", v03)).toBe(false);
  });
});

// The customers-last visibility gate: `published` is HONORED by furthestTargetOf (the
// /v1/update-check banner) but IGNORED by reaches / sourcesReaching (the /v1/migrate close-package
// fetch), so a successor can be served to the headless server open BEFORE it is surfaced to
// customers. Absent `published` = unpublished (the safe default).
describe("Registry — published (customer-visibility) gate", () => {
  /** A single-step chain v01 → v02, with v02's published state parameterised. */
  function gated(published?: boolean): RawRegistry {
    const v02Entry: RawRegistry["dnas"][number] = {
      dna_hash: v02,
      version: "alliance-v0.2.0",
      upgrades_from: v01,
      notaries: [{ url: "https://n2", api: "v1" }],
    };
    if (published !== undefined) v02Entry.published = published;
    return {
      version: 1,
      dnas: [
        {
          dna_hash: v01,
          version: "alliance-v0.1.0",
          upgrade_targets: [v02],
          notaries: [{ url: "https://n1", api: "v1" }],
        },
        v02Entry,
      ],
    };
  }

  it("furthestTargetOf hides an unpublished target (published:false → no banner)", () => {
    const r = Registry.load(gated(false));
    expect(r.furthestTargetOf(v01)).toBeUndefined();
  });

  it("furthestTargetOf hides a target with NO published field (absent = unpublished)", () => {
    const r = Registry.load(gated(undefined));
    expect(r.furthestTargetOf(v01)).toBeUndefined();
  });

  it("furthestTargetOf surfaces the target once it is published:true", () => {
    const r = Registry.load(gated(true));
    expect(r.furthestTargetOf(v01)?.dna_hash).toBe(v02);
  });

  it("furthestTargetOf skips an unpublished furthest target and falls back to the nearest published one", () => {
    // v01 proves [v02(published), v03(NOT published)] — the customers-last window where v03's
    // routing edge exists (server open can fetch it) but v03 is not yet customer-visible.
    const raw: RawRegistry = {
      version: 1,
      dnas: [
        {
          dna_hash: v01,
          version: "alliance-v0.1.0",
          upgrade_targets: [v02, v03],
          notaries: [{ url: "https://n1", api: "v1" }],
        },
        {
          dna_hash: v02,
          version: "alliance-v0.2.0",
          upgrades_from: v01,
          upgrade_targets: [v03],
          published: true,
          notaries: [{ url: "https://n2", api: "v1" }],
        },
        {
          dna_hash: v03,
          version: "alliance-v0.3.0",
          upgrades_from: v02,
          published: false,
          notaries: [{ url: "https://n3", api: "v1" }],
        },
      ],
    };
    const r = Registry.load(raw);
    // Falls back past the unpublished v03 to the published v02 (not undefined, not v03).
    expect(r.furthestTargetOf(v01)?.dna_hash).toBe(v02);
    // And v02's only proven target (v03) is unpublished → v02 sees no upgrade.
    expect(r.furthestTargetOf(v02)).toBeUndefined();
  });

  it("reaches IGNORES published — an unpublished target is still reachable (migrate serves it)", () => {
    const r = Registry.load(gated(false));
    expect(r.reaches(v01, v02)).toBe(true);
  });

  it("sourcesReaching IGNORES published — an unpublished target still lists its sources", () => {
    const r = Registry.load(gated(false));
    expect(r.sourcesReaching(v02).map((d) => d.dna_hash)).toEqual([v01]);
  });

  it("load accepts published:true and published:false", () => {
    expect(Registry.load(gated(true)).get(v02)?.published).toBe(true);
    expect(Registry.load(gated(false)).get(v02)?.published).toBe(false);
  });

  it("load rejects a non-boolean published", () => {
    const raw = gated(true);
    // @ts-expect-error intentionally wrong type
    raw.dnas[1].published = "yes";
    expect(() => Registry.load(raw)).toThrow(/published must be a boolean/);
  });
});
