// Cloudflare Worker entry point. Wires the bundled registry + env into the pure
// handlers, adds CORS, and routes by method + path.
//
// Rate limiting for POST /v1/migrate is enforced at the Cloudflare zone level
// (a rate-limiting rule on the route), not in Worker code — so no Durable Object
// is needed. See service-migration-service.md § Auth/rate-limit.

import registryJson from "../registry.json";
import { Registry, type RawRegistry } from "./registry";
import { errorJson } from "./errors";
import type { Env } from "./notary";
import { healthz, migrate, migrationOptions, updateCheck, type MigrateBody } from "./handlers";

const CORS_HEADERS: Record<string, string> = {
  "access-control-allow-origin": "*",
  "access-control-allow-methods": "GET, POST, OPTIONS",
  "access-control-allow-headers": "content-type",
};

function withCors(resp: Response): Response {
  const headers = new Headers(resp.headers);
  for (const [k, v] of Object.entries(CORS_HEADERS)) headers.set(k, v);
  return new Response(resp.body, { status: resp.status, headers });
}

// Parse + validate the registry once at module load. A bad registry throws here,
// so the Worker fails fast (and /healthz never returns ok).
const registry = Registry.load(registryJson as RawRegistry);

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: CORS_HEADERS });
    }

    const url = new URL(request.url);
    const { pathname } = url;

    try {
      if (request.method === "GET" && pathname === "/healthz") {
        return withCors(healthz());
      }
      if (request.method === "GET" && pathname === "/v1/migration-options") {
        return withCors(migrationOptions(registry, url.searchParams.get("to_dna_hash")));
      }
      if (request.method === "GET" && pathname === "/v1/update-check") {
        return withCors(updateCheck(registry, url.searchParams.get("current_dna_hash")));
      }
      if (request.method === "POST" && pathname === "/v1/migrate") {
        let body: MigrateBody;
        try {
          body = (await request.json()) as MigrateBody;
        } catch {
          return withCors(errorJson(400, "internal", "request body must be JSON"));
        }
        return withCors(await migrate(registry, body, env, fetch));
      }
      return withCors(errorJson(404, "internal", `no route for ${request.method} ${pathname}`));
    } catch (err) {
      return withCors(errorJson(500, "internal", `unexpected error: ${String(err)}`));
    }
  },
};
