// Route handlers. Kept free of the Worker runtime so they're unit-testable with
// an injected registry + fetch.

import { errorJson, ok } from "./errors";
import { Registry } from "./registry";
import { fetchClose, type Env, type FetchLike } from "./notary";

export const API_VERSIONS = ["v1"] as const;
export const PROTOCOL_VERSIONS = ["v0_1"] as const;

export function healthz(): Response {
  return ok({ status: "ok", api_versions: API_VERSIONS, protocol_versions: PROTOCOL_VERSIONS });
}

/** GET /v1/migration-options?to_dna_hash= */
export function migrationOptions(registry: Registry, toDnaHash: string | null): Response {
  if (!toDnaHash) {
    return errorJson(400, "unknown_to_dna", "to_dna_hash query parameter is required");
  }
  const predecessor = registry.predecessorOf(toDnaHash);
  const options = predecessor
    ? [{ from_dna_hash: predecessor.dna_hash, from_version: predecessor.version }]
    : [];
  return ok({ to_dna_hash: toDnaHash, options });
}

/** GET /v1/update-check?current_dna_hash=
 * Forward lookup: given the app's currently-installed DNA, is there a newer one to
 * migrate to, and where to download it. Detection-only — no auth, no notary calls. */
export function updateCheck(registry: Registry, currentDnaHash: string | null): Response {
  if (!currentDnaHash) {
    return errorJson(400, "unknown_current_dna", "current_dna_hash query parameter is required");
  }
  const successor = registry.successorOf(currentDnaHash);
  if (!successor) {
    // Unknown DNA or chain tip — either way, nothing to upgrade to.
    return ok({ current_dna_hash: currentDnaHash, has_upgrade: false });
  }
  return ok({
    current_dna_hash: currentDnaHash,
    has_upgrade: true,
    successor: {
      to_dna_hash: successor.dna_hash,
      to_version: successor.version,
      ...(successor.release_url !== undefined ? { release_url: successor.release_url } : {}),
    },
  });
}

export interface MigrateBody {
  from_dna_hash?: string;
  to_dna_hash?: string;
  agent_pubkey?: string;
}

/** Fisher–Yates on a copy. `rand` is injectable so tests can seed the order;
 * production uses `Math.random`. */
export function shuffled<T>(xs: readonly T[], rand: () => number = Math.random): T[] {
  const out = xs.slice();
  for (let i = out.length - 1; i > 0; i--) {
    const j = Math.floor(rand() * (i + 1));
    [out[i], out[j]] = [out[j], out[i]];
  }
  return out;
}

/** POST /v1/migrate */
export async function migrate(
  registry: Registry,
  body: MigrateBody,
  env: Env,
  fetchImpl: FetchLike,
  rand: () => number = Math.random,
): Promise<Response> {
  const { from_dna_hash, to_dna_hash, agent_pubkey } = body;
  if (!from_dna_hash || !to_dna_hash || !agent_pubkey) {
    // B6: missing required fields is a client error — a 4xx-classed `bad_request`,
    // not the 5xx-classed `internal` (which the envelope reserves for our faults).
    return errorJson(400, "bad_request", "from_dna_hash, to_dna_hash and agent_pubkey are required");
  }

  // 1. Validate the pair against the upgrades_from chain.
  const toEntry = registry.get(to_dna_hash);
  if (!toEntry) return errorJson(400, "unknown_to_dna", `unknown to_dna_hash ${to_dna_hash}`);
  const fromEntry = registry.get(from_dna_hash);
  if (!fromEntry) {
    return errorJson(400, "unknown_from_dna", `unknown from_dna_hash ${from_dna_hash}`);
  }
  if (!toEntry.upgrades_from) {
    return errorJson(400, "to_is_chain_root", `${to_dna_hash} is a chain root (no predecessor)`);
  }
  if (toEntry.upgrades_from !== from_dna_hash) {
    return errorJson(
      400,
      "not_registered_predecessor",
      `${from_dna_hash} is not the registered predecessor of ${to_dna_hash}`,
      { expected_from_dna_hash: toEntry.upgrades_from },
    );
  }

  // 2. Candidate notaries serving the from-DNA (1..N).
  const candidates = fromEntry.notaries;
  if (candidates.length === 0) {
    return errorJson(500, "all_orgs_unhealthy", `no notaries registered for ${from_dna_hash}`);
  }

  // 3. Try candidates in per-request RANDOM order — stateless load-spreading
  //    (migrations arrive ~one at a time; a fixed order hammers the first
  //    daemon). Hard stops propagate immediately, transient → next.
  const transientCodes: string[] = [];
  for (const notaryEntry of shuffled(candidates, rand)) {
    const outcome = await fetchClose(
      notaryEntry.url,
      notaryEntry.api,
      agent_pubkey,
      from_dna_hash,
      env,
      fetchImpl,
    );
    if (outcome.kind === "package") {
      // 4. Forward the closing-summary package verbatim.
      return ok({
        payload: outcome.payload,
        notary_signatures: outcome.notary_signatures,
        close_action: outcome.close_action,
      });
    }
    if (outcome.kind === "hard_stop") {
      return errorJson(outcome.status, outcome.code, outcome.message, outcome.details);
    }
    transientCodes.push(outcome.code);
  }

  // All candidates failed transiently. Surface the most informative cause rather
  // than collapsing a config/auth/rate-limit fault into a generic "unavailable".
  if (transientCodes.includes("unable_to_verify")) {
    return errorJson(503, "unable_to_verify", "all notaries were unable to verify the close state");
  }
  if (transientCodes.includes("auth_failed")) {
    return errorJson(
      502,
      "auth_failed",
      "notaries rejected the router's credentials — service misconfiguration",
    );
  }
  if (transientCodes.includes("rate_limited")) {
    return errorJson(503, "rate_limited", "notaries are rate limiting requests; retry shortly");
  }
  return errorJson(503, "all_orgs_unhealthy", "all notaries for the from-DNA are unavailable");
}
