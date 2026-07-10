// Cloudflare Worker entry point. Wires the bundled registry + env into the pure
// handlers, adds CORS, and routes by method + path.
//
// Rate limiting for POST /v1/migrate is enforced at the Cloudflare zone level
// (a rate-limiting rule on the route), not in Worker code — so no Durable Object
// is needed. See unyt's internal migration-router.md spec § Auth / rate-limit.

import registryJson from "../registry.json";
import { Registry, type RawRegistry } from "./registry";
import { errorJson } from "./errors";
import type { Env } from "./notary";
import { healthz, migrate, migrationOptions, updateCheck, type MigrateBody } from "./handlers";
import { cfCache } from "./builds";

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

// B7: the shipped registry still carries the placeholder DNA hash + stub notary
// URL. Log an un-provisioned line at startup so a mis-deploy of the placeholder
// to a real environment is obvious in the logs.
const REGISTRY_PLACEHOLDER = "uhC0kREPLACE_WITH_v0_1_DNA_HASH";
if ((registryJson as RawRegistry).dnas.some((d) => d.dna_hash === REGISTRY_PLACEHOLDER)) {
  console.warn(
    "migration-router: registry.json is UN-PROVISIONED — still contains the " +
      `placeholder DNA hash "${REGISTRY_PLACEHOLDER}". /v1/migrate will not work ` +
      "until registry.json is provisioned with live DNA hashes + notary entries.",
  );
}

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
        // The Cache API lives only in the Worker runtime; in the node unit env it's absent, so the
        // shield is simply skipped (correctness is unchanged — it only bounds GitHub traffic).
        const buildCache = typeof caches !== "undefined" ? cfCache(caches.default) : undefined;
        return withCors(
          await updateCheck(
            registry,
            url.searchParams.get("current_dna_hash"),
            url.searchParams.get("app_version"),
            fetch,
            env,
            buildCache,
          ),
        );
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
