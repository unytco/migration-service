// Route handlers. Kept free of the Worker runtime so they're unit-testable with
// an injected registry + fetch.

import { errorJson, ok } from "./errors";
import { Registry, type DnaEntry } from "./registry";
import { fetchClose, type Env, type FetchLike, type HardStop } from "./notary";

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
  // All candidate sources that have a proven path to `to` (direct skip: not just
  // the immediate predecessor) — the app may have closed on any of them.
  const options = registry
    .sourcesReaching(toDnaHash)
    .map((s) => ({ from_dna_hash: s.dna_hash, from_version: s.version }));
  return ok({ to_dna_hash: toDnaHash, options });
}

/** GET /v1/update-check?current_dna_hash=
 * Forward lookup: given the app's currently-installed DNA, is there a newer one to
 * migrate to, and where to download it. Detection-only — no auth, no notary calls. */
export function updateCheck(registry: Registry, currentDnaHash: string | null): Response {
  if (!currentDnaHash) {
    return errorJson(400, "unknown_current_dna", "current_dna_hash query parameter is required");
  }
  // The FURTHEST proven target (deepest descendant in `upgrade_targets`) — the app
  // jumps straight there in one hop rather than version-by-version.
  const target = registry.furthestTargetOf(currentDnaHash);
  if (!target) {
    // Unknown DNA, chain tip, or no proven target — nothing to upgrade to.
    return ok({ current_dna_hash: currentDnaHash, has_upgrade: false });
  }
  return ok({
    current_dna_hash: currentDnaHash,
    has_upgrade: true,
    target: {
      to_dna_hash: target.dna_hash,
      to_version: target.version,
      ...(target.release_url !== undefined ? { release_url: target.release_url } : {}),
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
  // `to` + `agent` are required; `from` is OPTIONAL — a freshly-installed app may
  // no longer know its predecessor, so the router discovers the source.
  if (!to_dna_hash || !agent_pubkey) {
    // Missing required fields is a client error — a 4xx-classed `bad_request`,
    // not the 5xx-classed `internal` (which the envelope reserves for our faults).
    return errorJson(400, "bad_request", "to_dna_hash and agent_pubkey are required");
  }
  const toEntry = registry.get(to_dna_hash);
  if (!toEntry) return errorJson(400, "unknown_to_dna", `unknown to_dna_hash ${to_dna_hash}`);
  if (!toEntry.upgrades_from) {
    return errorJson(400, "to_is_chain_root", `${to_dna_hash} is a chain root (no predecessor)`);
  }

  // 1. Resolve the candidate sources. A supplied `from` is validated as a proven
  //    upgrade target and used directly; otherwise discover every source that
  //    lists `to` among its upgrade_targets (direct skip — any of them).
  let sources: DnaEntry[];
  if (from_dna_hash) {
    const fromEntry = registry.get(from_dna_hash);
    if (!fromEntry) return errorJson(400, "unknown_from_dna", `unknown from_dna_hash ${from_dna_hash}`);
    if (!registry.reaches(from_dna_hash, to_dna_hash)) {
      return errorJson(
        400,
        "unreachable_target",
        `${to_dna_hash} is not a proven upgrade target of ${from_dna_hash}`,
      );
    }
    sources = [fromEntry];
  } else {
    sources = registry.sourcesReaching(to_dna_hash);
    if (sources.length === 0) {
      return errorJson(400, "unreachable_target", `no registered source reaches ${to_dna_hash}`);
    }
  }

  // 2. Try each source's daemons in per-request RANDOM order — stateless load-
  //    spreading (migrations arrive ~one at a time; a fixed order hammers the
  //    first daemon) — advancing across sources during discovery. Accept the
  //    first package actually bound to `to`. Only an AGENT-level / malformed fault
  //    (`warranted`, `bad_request`) is terminal across all sources; a SOURCE-specific
  //    fault (`no_close_found`, or a daemon `internal` — a wrong-cell/registry
  //    mismatch) must NOT abort the OTHER candidate sources, so it skips to the next
  //    source and is surfaced only if none succeeds; a transient daemon → its sibling.
  const transientCodes: string[] = [];
  // The first source-specific daemon fault (a wrong-cell `internal`, with its details) —
  // surfaced verbatim if NO source yields the package, so its diagnostic survives.
  let internalFault: HardStop | undefined;
  let sawMalformedPackage = false; // a package carried no target_dna_hash (daemon/shape fault)
  let sawZeroNotary = false; // a candidate source has no registered notaries (config fault)
  for (const source of sources) {
    if (source.notaries.length === 0) {
      sawZeroNotary = true;
      continue; // can't query this source — try the others
    }
    for (const notaryEntry of shuffled(source.notaries, rand)) {
      const outcome = await fetchClose(
        notaryEntry.url,
        notaryEntry.api,
        agent_pubkey,
        source.dna_hash,
        env,
        fetchImpl,
      );
      if (outcome.kind === "package") {
        if (outcome.target_dna_hash === to_dna_hash) {
          // 3. Forward the closing-summary package verbatim.
          return ok({
            payload: outcome.payload,
            notary_signatures: outcome.notary_signatures,
            close_action: outcome.close_action,
          });
        }
        // A complete package carrying NO target_dna_hash is a malformed/old-shape package
        // (every 0.5.0 daemon binds one) — a daemon fault, not a clean stale close. A
        // SIBLING daemon may serve a well-formed one, so try the rest of this source's
        // notaries; surface it only if none succeeds (never silently "no close").
        if (typeof outcome.target_dna_hash !== "string" || outcome.target_dna_hash.length === 0) {
          sawMalformedPackage = true;
          continue;
        }
        break; // a genuine stale close (bound elsewhere) — every daemon of this source
        // serves the same chain, so only the next source can help.
      }
      if (outcome.kind === "hard_stop") {
        // Terminal regardless of which source we ask — no source fixes a warranted
        // chain or a bad request; propagate immediately.
        if (outcome.code === "warranted" || outcome.code === "bad_request") {
          return errorJson(outcome.status, outcome.code, outcome.message, outcome.details);
        }
        // `no_close_found` is a content verdict every daemon of this source returns
        // identically — move on to the next source.
        if (outcome.code === "no_close_found") break;
        // A source-specific `internal` (this daemon's URL points at the wrong conductor) —
        // a SIBLING daemon may serve the right cell, so try the rest of this source's
        // notaries; capture the first internal (with its details) for the final surface.
        if (!internalFault) internalFault = outcome;
        continue;
      }
      transientCodes.push(outcome.code);
    }
  }

  // No source yielded a package bound to `to`. Config / daemon FAULTS (5xx) rank FIRST:
  // they will not fix themselves by retrying, and the internal-vs-transient distinction is
  // load-bearing — a definite fault must never read as a momentary outage (operator
  // triage), even when a transient sibling co-occurs and would otherwise mask it.
  if (internalFault) {
    // The captured wrong-cell fault carries its diagnostic details verbatim.
    return errorJson(internalFault.status, internalFault.code, internalFault.message, internalFault.details);
  }
  if (transientCodes.includes("internal") || sawMalformedPackage) {
    return errorJson(500, "internal", "a notary daemon returned an internal error for a candidate source");
  }
  // A candidate source has NO registered notaries — a registry/provisioning fault on our
  // side: surface as 5xx so it is fixed (not retried as a transient), and so a close that
  // may live on that source is never reported "absent".
  if (sawZeroNotary) {
    return errorJson(500, "internal", "a candidate source has no registered notaries — registry misconfiguration");
  }
  // Then the retryable transients — the close may be on a momentarily-unreachable /
  // not-yet-verifiable source, so the caller should retry rather than conclude "no
  // record". unable_to_verify (the close likely exists but isn't verifiable yet) wins
  // the group, ahead of `no_close_found`.
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
  if (transientCodes.length > 0) {
    return errorJson(503, "all_orgs_unhealthy", "all candidate notaries are unavailable");
  }
  // Every candidate was reachable and definitively had no close bound to `to`.
  return errorJson(404, "no_close_found", "no committed close bound to the requested target was found");
}
