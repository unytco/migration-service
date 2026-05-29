import { describe, expect, it } from "vitest";
import { Registry, type RawRegistry } from "../src/registry";

const v01 = "uhC0k_v01";
const v02 = "uhC0k_v02";
const v03 = "uhC0k_v03";

function chain(): RawRegistry {
  return {
    version: 1,
    dnas: [
      { dna_hash: v01, version: "alliance-v0.1.0", notaries: [{ url: "https://n1" }] },
      { dna_hash: v02, version: "alliance-v0.2.0", upgrades_from: v01, notaries: [{ url: "https://n2" }] },
      { dna_hash: v03, version: "alliance-v0.3.0", upgrades_from: v02, notaries: [{ url: "https://n3" }] },
    ],
  };
}

describe("Registry.load", () => {
  it("loads a valid linear chain", () => {
    const r = Registry.load(chain());
    expect(r.get(v02)?.version).toBe("alliance-v0.2.0");
  });

  it("rejects an unsupported schema version", () => {
    expect(() => Registry.load({ ...chain(), version: 99 })).toThrow(/unsupported registry version/);
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

  it("predecessorOf walks one step", () => {
    const r = Registry.load(chain());
    expect(r.predecessorOf(v03)?.dna_hash).toBe(v02); // not v01
    expect(r.predecessorOf(v01)).toBeUndefined(); // chain root
    expect(r.predecessorOf("unknown")).toBeUndefined();
  });
});
