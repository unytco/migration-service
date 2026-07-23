// Worker-level integration test for the default export (the Cloudflare Worker
// fetch handler) in src/index.ts. The pure handlers are unit-tested in
// handlers.test.ts; this exercises routing, CORS, and the bundled-registry wiring.
//
// index.ts loads the BUNDLED registry.json at module load (not injectable), so
// these tests assert against whatever registry.json currently contains: a single
// chain-root placeholder DNA.

import { describe, expect, it } from "vitest";
import worker from "../src/index";
import type { Env } from "../src/notary";

// The single placeholder DNA hash currently shipped in registry.json. It is a
// chain root (no upgrades_from), i.e. a chain tip with no successor.
const PLACEHOLDER_DNA = "uhC0kREPLACE_WITH_v0_1_DNA_HASH";

const ENV: Env = { MIGRATION_NOTARY_BEARER_TOKEN: "t" };

const BASE = "https://router.example";

function get(path: string): Promise<Response> {
  return worker.fetch(new Request(`${BASE}${path}`, { method: "GET" }), ENV);
}

async function body(resp: Response): Promise<any> {
  return resp.json();
}

describe("worker.fetch — GET /healthz", () => {
  it("returns 200 with status:ok and the version arrays", async () => {
    const resp = await get("/healthz");
    expect(resp.status).toBe(200);
    const b = await body(resp);
    expect(b.status).toBe("ok");
    expect(b.api_versions).toEqual(["v1"]);
    expect(b.protocol_versions).toEqual(["v0_1"]);
  });

  it("carries the CORS allow-origin header on a successful response", async () => {
    const resp = await get("/healthz");
    expect(resp.headers.get("access-control-allow-origin")).toBe("*");
  });
});

describe("worker.fetch — GET /v1/update-check", () => {
  it("placeholder (chain tip) has no upgrade", async () => {
    const resp = await get(
      `/v1/update-check?current_dna_hash=${encodeURIComponent(PLACEHOLDER_DNA)}`,
    );
    expect(resp.status).toBe(200);
    const b = await body(resp);
    expect(b.has_upgrade).toBe(false);
    expect(b.current_dna_hash).toBe(PLACEHOLDER_DNA);
  });

  it("missing current_dna_hash → 400 unknown_current_dna", async () => {
    const resp = await get("/v1/update-check");
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("unknown_current_dna");
  });
});

describe("worker.fetch — GET /v1/migration-options", () => {
  it("still routes (regression): placeholder root → 200 with empty options", async () => {
    const resp = await get(
      `/v1/migration-options?to_dna_hash=${encodeURIComponent(PLACEHOLDER_DNA)}`,
    );
    expect(resp.status).toBe(200);
    const b = await body(resp);
    expect(b.to_dna_hash).toBe(PLACEHOLDER_DNA);
    expect(b.options).toEqual([]);
  });

  // The app's fresh-install path hits THIS boundary with a DNA the deployed
  // registry has never heard of, and reads any non-2xx as "router unreachable"
  // (retry card). The spec contracts empty options, not an error — asserted at
  // the worker boundary, not only on the pure handler.
  it("unregistered target → 200 with empty options, not a 4xx", async () => {
    const resp = await get(
      "/v1/migration-options?to_dna_hash=uhC0k_never_registered",
    );
    expect(resp.status).toBe(200);
    const b = await body(resp);
    expect(b.to_dna_hash).toBe("uhC0k_never_registered");
    expect(b.options).toEqual([]);
    expect(b.error).toBeUndefined();
  });
});

describe("worker.fetch — CORS preflight", () => {
  it("OPTIONS on any path → 204 with CORS headers", async () => {
    const resp = await worker.fetch(
      new Request(`${BASE}/anything`, { method: "OPTIONS" }),
      ENV,
    );
    expect(resp.status).toBe(204);
    expect(resp.headers.get("access-control-allow-origin")).toBe("*");
    expect(resp.headers.get("access-control-allow-methods")).toBe("GET, POST, OPTIONS");
    expect(resp.headers.get("access-control-allow-headers")).toBe("content-type");
  });
});

describe("worker.fetch — unknown route", () => {
  it("GET /nope → 404 bad_request (client error, not internal)", async () => {
    const resp = await get("/nope");
    expect(resp.status).toBe(404);
    expect((await body(resp)).error.code).toBe("bad_request");
  });

  it("404 response also carries CORS", async () => {
    const resp = await get("/nope");
    expect(resp.headers.get("access-control-allow-origin")).toBe("*");
  });
});

describe("worker.fetch — POST /v1/migrate with a malformed body", () => {
  it("non-JSON body → 400 bad_request (client error, not internal)", async () => {
    const resp = await worker.fetch(
      new Request(`${BASE}/v1/migrate`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: "not json{",
      }),
      ENV,
    );
    expect(resp.status).toBe(400);
    expect((await body(resp)).error.code).toBe("bad_request");
  });
});
