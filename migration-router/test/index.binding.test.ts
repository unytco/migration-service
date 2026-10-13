// Worker-level test for src/index.binding.ts: the entry point that serves the
// registry in the REGISTRY_JSON binding and bundles none.

import { describe, expect, it } from "vitest";
import worker from "../src/index.binding";
import type { RawRegistry } from "../src/registry";
import type { Env } from "../src/notary";

const BASE = "https://router.example";

const v01 = "uhC0k_v01";
const v02 = "uhC0k_v02";

function registryJson(notaryUrl = "https://notary-1.example"): string {
  const raw: RawRegistry = {
    version: 1,
    dnas: [
      {
        dna_hash: v01,
        version: "alliance-v0.1.0",
        upgrade_targets: [v02],
        closing_threshold: 1,
        notaries: [{ url: notaryUrl, api: "v2" }],
      },
      {
        dna_hash: v02,
        version: "alliance-v0.2.0",
        upgrades_from: v01,
        notaries: [{ url: "https://notary-2.example", api: "v2" }],
        published: true,
      },
    ],
  };
  return JSON.stringify(raw);
}

function env(registry?: string): Env {
  return { MIGRATION_NOTARY_BEARER_TOKEN: "t", REGISTRY_JSON: registry };
}

function get(path: string, e: Env): Promise<Response> {
  return worker.fetch(new Request(`${BASE}${path}`, { method: "GET" }), e);
}

async function errorCode(resp: Response): Promise<string> {
  return ((await resp.json()) as any).error.code;
}

describe("index.binding — a bound registry", () => {
  it("is the registry requests are served from", async () => {
    const resp = await get(`/v1/migration-options?to_dna_hash=${v02}`, env(registryJson()));
    expect(resp.status).toBe(200);
    expect(((await resp.json()) as any).options).toEqual([
      { from_dna_hash: v01, from_version: "alliance-v0.1.0" },
    ]);
  });

  it("answers /healthz", async () => {
    expect((await get("/healthz", env(registryJson()))).status).toBe(200);
  });

  it("is reloaded when the binding's text changes", async () => {
    const before = await get(`/v1/migration-options?to_dna_hash=${v02}`, env(registryJson()));
    expect(((await before.json()) as any).options).toHaveLength(1);

    const empty = JSON.stringify({ version: 1, dnas: [] });
    const after = await get(`/v1/migration-options?to_dna_hash=${v02}`, env(empty));
    expect(((await after.json()) as any).options).toEqual([]);
  });
});

// Each of these is a deployment that must look broken rather than serve some
// other registry: /healthz is the check that has to fail.
describe("index.binding — no valid registry fails every request", () => {
  for (const [name, registry] of [
    ["an unset binding", undefined],
    ["an empty binding", ""],
    ["text that is not JSON", "{"],
    ["a registry the strict validator rejects", registryJson("http://notary-1.example")],
    ["an unsupported registry version", JSON.stringify({ version: 2, dnas: [] })],
  ] as const) {
    it(`${name} → /healthz 500 internal`, async () => {
      const resp = await get("/healthz", env(registry));
      expect(resp.status).toBe(500);
      expect(await errorCode(resp)).toBe("internal");
      expect(resp.headers.get("access-control-allow-origin")).toBe("*");
    });
  }

  it("a binding fixed after a bad one is served", async () => {
    expect((await get("/healthz", env("{"))).status).toBe(500);
    expect((await get("/healthz", env(registryJson()))).status).toBe(200);
  });
});
