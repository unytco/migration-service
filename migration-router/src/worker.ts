import {
  Registry,
  type RawRegistry,
  type RegistryLoadOptions,
} from "./registry";
import { errorJson } from "./errors";
import type { Env } from "./notary";
import {
  healthz,
  migrate,
  migrationOptions,
  updateCheck,
  type MigrateBody,
} from "./handlers";
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

const REGISTRY_PLACEHOLDER = "uhC0kREPLACE_WITH_v0_1_DNA_HASH";

function warnIfUnprovisioned(raw: RawRegistry): void {
  if (raw.dnas.some((d) => d.dna_hash === REGISTRY_PLACEHOLDER)) {
    console.warn(
      "migration-router: registry.json is UN-PROVISIONED — still contains the " +
        `placeholder DNA hash "${REGISTRY_PLACEHOLDER}". /v1/migrate will not work ` +
        "until registry.json is provisioned with live DNA hashes + notary entries.",
    );
  }
}

/** Call it at module load, so a bad registry fails the Worker before it serves. */
export function workerFor(raw: RawRegistry, opts?: RegistryLoadOptions) {
  const registry = Registry.load(raw, opts);
  warnIfUnprovisioned(raw);
  return createWorker(registry);
}

function createWorker(registry: Registry) {
  return {
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
          return withCors(
            migrationOptions(registry, url.searchParams.get("to_dna_hash")),
          );
        }
        if (request.method === "GET" && pathname === "/v1/update-check") {
          const buildCache =
            typeof caches !== "undefined" ? cfCache(caches.default) : undefined;
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
        // POST /v1/migrate has no rate limit in Worker code. Each router's route needs a
        // Cloudflare zone rate-limiting rule (unyt's internal migration-router.md spec
        // § Auth / rate-limit).
        if (request.method === "POST" && pathname === "/v1/migrate") {
          let body: MigrateBody;
          try {
            body = (await request.json()) as MigrateBody;
          } catch {
            return withCors(
              errorJson(400, "bad_request", "request body must be JSON"),
            );
          }
          return withCors(await migrate(registry, body, env, fetch));
        }
        return withCors(
          errorJson(
            404,
            "bad_request",
            `no route for ${request.method} ${pathname}`,
          ),
        );
      } catch (err) {
        return withCors(
          errorJson(500, "internal", `unexpected error: ${String(err)}`),
        );
      }
    },
  };
}
