// Route handlers. Kept free of the Worker runtime so they're unit-testable with
// an injected registry + fetch.

import { errorJson, ok } from "./errors";
import { Registry, type DnaEntry } from "./registry";
import { fetchClose, type Env, type FetchLike, type HardStop } from "./notary";
import {
  publishedBuilds,
  lineageOf,
  lineageOfReleaseUrl,
  newestOnLineage,
  type CacheLike,
} from "./builds";

export const API_VERSIONS = ["v1"] as const;
export const PROTOCOL_VERSIONS = ["v0_1"] as const;

export function healthz(): Response {
  return ok({
    status: "ok",
    api_versions: API_VERSIONS,
    protocol_versions: PROTOCOL_VERSIONS,
  });
}

/** GET /v1/migration-options?to_dna_hash= */
export function migrationOptions(
  registry: Registry,
  toDnaHash: string | null,
): Response {
  if (!toDnaHash) {
    return errorJson(
      400,
      "unknown_to_dna",
      "to_dna_hash query parameter is required",
    );
  }
  // An unregistered target is a client error, not an empty result: mirror
  // /v1/migrate's `unknown_to_dna` so an unknown hash never reads as "known, but
  // no sources reach it" (which `options: []` would imply).
  if (!registry.get(toDnaHash)) {
    return errorJson(400, "unknown_to_dna", `unknown to_dna_hash ${toDnaHash}`);
  }
  // Any source with a proven path to `to` (direct skip, not just the immediate
  // predecessor) — the app may have closed on any of them.
  const options = registry
    .sourcesReaching(toDnaHash)
    .map((s) => ({ from_dna_hash: s.dna_hash, from_version: s.version }));
  return ok({ to_dna_hash: toDnaHash, options });
}

/** GET /v1/update-check?current_dna_hash=&app_version=
 * Two orthogonal answers on one response:
 *  - migration axis (`has_upgrade` + `target`): is this DNA superseded — registry-only, as before.
 *  - build axis (`latest_build`): the newest published build on the caller's OWN lineage, resolved
 *    from GitHub and gated on a parseable `app_version`.
 * Detection-only — no auth, no notary calls. A caller sending no (or an unparseable) app_version
 * gets byte-identical behaviour to the pre-two-channel router: the migration answer with its
 * recorded target link and no build axis. Any GitHub failure degrades to exactly that too, so the
 * migration answer never breaks (still 2xx). */
export async function updateCheck(
  registry: Registry,
  currentDnaHash: string | null,
  appVersion: string | null,
  fetchImpl: FetchLike,
  env: Env,
  cache?: CacheLike,
): Promise<Response> {
  if (!currentDnaHash) {
    return errorJson(
      400,
      "unknown_current_dna",
      "current_dna_hash query parameter is required",
    );
  }

  // The FURTHEST proven target (deepest descendant in `upgrade_targets`) — one hop straight there.
  const target = registry.furthestTargetOf(currentDnaHash);
  const callerLineage = lineageOf(appVersion);

  // No parseable app version → build axis off; byte-identical to the pre-two-channel router.
  if (callerLineage === null) {
    return ok(migrationAnswer(currentDnaHash, target, target?.release_url));
  }

  // app_version present → resolve published builds ONCE (never throws; [] on any failure), reused
  // for both the target's freshest download link and the caller's own latest_build.
  const builds = await publishedBuilds(fetchImpl, env, cache);

  // Target link: newest published build of the TARGET's lineage (parsed from the tag in its
  // registry release_url), falling back to the recorded link so a migration always carries one.
  let targetLink = target?.release_url;
  if (target) {
    const fresh = newestOnLineage(
      builds,
      lineageOfReleaseUrl(target.release_url),
    );
    if (fresh) targetLink = fresh.release_url;
  }

  // Build axis: newest published build on the caller's OWN lineage. Absent is NOT "up to date".
  const latest = newestOnLineage(builds, callerLineage);

  return ok({
    ...migrationAnswer(currentDnaHash, target, targetLink),
    ...(latest
      ? {
          latest_build: {
            version: latest.version,
            release_url: latest.release_url,
            assets: latest.assets ?? [],
          },
        }
      : {}),
  });
}

/** The migration axis of the response — shared by the fast (no-version) and full paths. */
function migrationAnswer(
  currentDnaHash: string,
  target: DnaEntry | null | undefined,
  releaseUrl: string | undefined,
) {
  if (!target) {
    return { current_dna_hash: currentDnaHash, has_upgrade: false as const };
  }
  return {
    current_dna_hash: currentDnaHash,
    has_upgrade: true as const,
    target: {
      to_dna_hash: target.dna_hash,
      to_version: target.version,
      ...(releaseUrl !== undefined ? { release_url: releaseUrl } : {}),
    },
  };
}

export interface MigrateBody {
  from_dna_hash?: string;
  to_dna_hash?: string;
  agent_pubkey?: string;
}

/** Fisher–Yates on a copy. `rand` is injectable so tests can seed the order;
 * production uses `Math.random`. */
export function shuffled<T>(
  xs: readonly T[],
  rand: () => number = Math.random,
): T[] {
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
  // `to` + `agent` are required; `from` is OPTIONAL — a freshly-installed app may no
  // longer know its predecessor, so the router discovers the source.
  if (!to_dna_hash || !agent_pubkey) {
    // Client error: 4xx `bad_request`, not the 5xx `internal` the envelope reserves for our faults.
    return errorJson(
      400,
      "bad_request",
      "to_dna_hash and agent_pubkey are required",
    );
  }
  const toEntry = registry.get(to_dna_hash);
  if (!toEntry)
    return errorJson(
      400,
      "unknown_to_dna",
      `unknown to_dna_hash ${to_dna_hash}`,
    );
  if (!toEntry.upgrades_from) {
    return errorJson(
      400,
      "to_is_chain_root",
      `${to_dna_hash} is a chain root (no predecessor)`,
    );
  }

  // Resolve candidate sources: a supplied `from` is validated and used directly;
  // otherwise discover every source listing `to` among its upgrade_targets.
  let sources: DnaEntry[];
  if (from_dna_hash) {
    const fromEntry = registry.get(from_dna_hash);
    if (!fromEntry)
      return errorJson(
        400,
        "unknown_from_dna",
        `unknown from_dna_hash ${from_dna_hash}`,
      );
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
      return errorJson(
        400,
        "unreachable_target",
        `no registered source reaches ${to_dna_hash}`,
      );
    }
  }

  // Try each source's daemons in per-request random order (stateless load-spreading),
  // accepting the first package bound to `to`. Only an agent-level/malformed fault
  // (`warranted`, `bad_request`) is terminal across all sources; a source-specific fault
  // skips to the next source and is surfaced only if none succeeds.
  const transientCodes: string[] = [];
  let internalFault: HardStop | undefined;
  let sawMalformedPackage = false;
  let sawZeroNotary = false;
  for (const source of sources) {
    if (source.notaries.length === 0) {
      sawZeroNotary = true;
      continue;
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
          return ok({
            payload: outcome.payload,
            notary_signatures: outcome.notary_signatures,
            close_action: outcome.close_action,
          });
        }
        // A package with NO target_dna_hash is malformed (every daemon binds one) — a
        // daemon fault, not a clean stale close. A sibling may serve a well-formed one.
        if (
          typeof outcome.target_dna_hash !== "string" ||
          outcome.target_dna_hash.length === 0
        ) {
          sawMalformedPackage = true;
          continue;
        }
        // A genuine stale close bound elsewhere — every daemon of this source serves the
        // same chain, so only the next source can help.
        break;
      }
      if (outcome.kind === "hard_stop") {
        // Terminal regardless of source — no source fixes a warranted chain or bad request.
        if (outcome.code === "warranted" || outcome.code === "bad_request") {
          return errorJson(
            outcome.status,
            outcome.code,
            outcome.message,
            outcome.details,
          );
        }
        // A content verdict every daemon of this source returns identically — next source.
        if (outcome.code === "no_close_found") break;
        // A source-specific `internal` (wrong-cell): a sibling may serve the right cell, so
        // try the rest; capture the first (with details) for the final surface.
        if (!internalFault) internalFault = outcome;
        continue;
      }
      transientCodes.push(outcome.code);
    }
  }

  // No source yielded a package bound to `to`. Config/daemon FAULTS (5xx) rank FIRST: they
  // won't fix themselves by retrying, and a definite fault must never read as a momentary
  // outage even when a transient sibling co-occurs and would otherwise mask it.
  if (internalFault) {
    return errorJson(
      internalFault.status,
      internalFault.code,
      internalFault.message,
      internalFault.details,
    );
  }
  if (transientCodes.includes("internal") || sawMalformedPackage) {
    return errorJson(
      500,
      "internal",
      "a notary daemon returned an internal error for a candidate source",
    );
  }
  // Zero registered notaries is a registry fault on our side — 5xx so it's fixed, and so a
  // close that may live on that source is never reported "absent".
  if (sawZeroNotary) {
    return errorJson(
      500,
      "internal",
      "a candidate source has no registered notaries — registry misconfiguration",
    );
  }
  // Then the retryable transients — the close may be on a momentarily-unreachable source.
  // unable_to_verify (likely exists but not verifiable yet) wins the group.
  if (transientCodes.includes("unable_to_verify")) {
    return errorJson(
      503,
      "unable_to_verify",
      "all notaries were unable to verify the close state",
    );
  }
  if (transientCodes.includes("auth_failed")) {
    return errorJson(
      502,
      "auth_failed",
      "notaries rejected the router's credentials — service misconfiguration",
    );
  }
  if (transientCodes.includes("rate_limited")) {
    return errorJson(
      503,
      "rate_limited",
      "notaries are rate limiting requests; retry shortly",
    );
  }
  if (transientCodes.length > 0) {
    return errorJson(
      503,
      "all_orgs_unhealthy",
      "all candidate notaries are unavailable",
    );
  }
  // Every candidate was reachable and definitively had no close bound to `to`.
  return errorJson(
    404,
    "no_close_found",
    "no committed close bound to the requested target was found",
  );
}
