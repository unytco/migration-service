import { afterEach, describe, expect, it, vi } from "vitest";
import worker from "../src/index.mainnet";
import type { Env } from "../src/notary";
import testnet from "../registry.json";
import { releasesResp } from "./github-fixtures";

const UNYT_RELEASES =
  "https://api.github.com/repos/unytco/unyt-release/releases";

const ENV = { GITHUB_RELEASES_URL: UNYT_RELEASES } as Env;

const DNA = "uhC0k_mainnet_dna";

function get(path: string): Promise<Response> {
  return worker.fetch(
    new Request(`https://router.example${path}`, { method: "GET" }),
    ENV,
  );
}

function stubReleases(tags: string[]): string[] {
  const seen: string[] = [];
  vi.stubGlobal("fetch", async (input: RequestInfo | URL) => {
    seen.push(typeof input === "string" ? input : input.toString());
    return releasesResp(
      tags.map((tag) => ({ tag })),
      "unytco/unyt-release",
    );
  });
  return seen;
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("MainNet router, empty registry", () => {
  it("update check: no upgrade, and the newest Unyt build on the caller's lineage", async () => {
    const seen = stubReleases(["v1.2.0", "v1.1.5", "v1.1.3"]);
    const resp = await get(
      `/v1/update-check?current_dna_hash=${DNA}&app_version=1.1.3`,
    );
    expect(resp.status).toBe(200);
    expect(await resp.json()).toEqual({
      current_dna_hash: DNA,
      has_upgrade: false,
      latest_build: {
        version: "1.1.5",
        release_url:
          "https://github.com/unytco/unyt-release/releases/tag/v1.1.5",
        assets: [],
      },
    });
    expect(seen.length).toBeGreaterThan(0);
    expect(seen.every((u) => u.startsWith(`${UNYT_RELEASES}?`))).toBe(true);
  });

  it("migration options: an unregistered network gets no options", async () => {
    const seen = stubReleases([]);
    const resp = await get(`/v1/migration-options?to_dna_hash=${DNA}`);
    expect(resp.status).toBe(200);
    expect(await resp.json()).toEqual({ to_dna_hash: DNA, options: [] });
    expect(seen).toEqual([]);
  });

  it("migrate: knows none of the TestNet DNAs, and asks no notary", async () => {
    const seen = stubReleases([]);
    expect(testnet.dnas.length).toBeGreaterThan(0);
    for (const { dna_hash } of testnet.dnas) {
      const resp = await worker.fetch(
        new Request("https://router.example/v1/migrate", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({
            to_dna_hash: dna_hash,
            agent_pubkey: "uhCAk_agent",
          }),
        }),
        ENV,
      );
      expect(resp.status).toBe(400);
      expect(await resp.json()).toMatchObject({
        error: { code: "unknown_to_dna" },
      });
    }
    expect(seen).toEqual([]);
  });
});
