// Route handlers. Kept free of the Worker runtime so they're unit-testable with
// an injected registry + fetch.

import { errorJson, ok } from "./errors";
import { Registry, type DnaEntry, type SourceEntry } from "./registry";
import { attestClose, type DaemonAnswer, type Env, type FetchLike } from "./notary";
import {
  publishedBuilds,
  lineageOf,
  lineageOfReleaseUrl,
  newestOnLineage,
  type CacheLike,
} from "./builds";

export const API_VERSIONS = ["v1"] as const;
export const PROTOCOL_VERSIONS = ["v0_2"] as const;

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
  // Missing param only, and deliberately still `unknown_to_dna`. Since an unregistered
  // target is a 200 (below), this is the sole path to that code HERE — whereas
  // `/v1/migrate` keeps emitting it for a hash that isn't in the registry. Same code,
  // two disjoint conditions; don't read one endpoint's meaning from the other. The
  // spec's § Wire format & errors table documents both, so this is the as-built
  // contract, not drift: the router's convention for a missing GET query param is a
  // dedicated per-endpoint code (`/v1/update-check` does the same with
  // `unknown_current_dna`), not the body-scoped `bad_request`.
  if (!toDnaHash) {
    return errorJson(
      400,
      "unknown_to_dna",
      "to_dna_hash query parameter is required",
    );
  }
  // An unregistered target is NOT an error — the spec (version-migration
  // `migration-router.md` § Endpoints) contracts "chain root / unknown DNA →
  // `{ options: [] }`", and the app relies on it: it distinguishes "no migration
  // path, join fresh" from "router errored, show a retry card". A 4xx here would
  // strand a fresh installer on the retry card instead of letting them join.
  if (!registry.get(toDnaHash)) {
    return ok({ to_dna_hash: toDnaHash, options: [] });
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

  let sources: SourceEntry[];
  if (from_dna_hash) {
    if (!registry.get(from_dna_hash))
      return errorJson(
        400,
        "unknown_from_dna",
        `unknown from_dna_hash ${from_dna_hash}`,
      );
    const source = registry.source(from_dna_hash, to_dna_hash);
    if (!source) {
      return errorJson(
        400,
        "unreachable_target",
        `${to_dna_hash} is not a proven upgrade target of ${from_dna_hash}`,
      );
    }
    sources = [source];
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

  const tally: Tally = {
    fault: false,
    unableToVerify: false,
    authFailed: false,
    rateLimited: false,
    unreachable: false,
  };
  for (const source of sources) {
    const outcome = await attestSource(
      source,
      to_dna_hash,
      agent_pubkey,
      env,
      fetchImpl,
      rand,
      tally,
    );
    if (outcome.kind === "package") {
      return new Response(outcome.body, {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    }
    if (outcome.kind === "stop") return outcome.response;
  }
  return shortResponse(tally);
}

/** What the daemons said across every source, for the response when no
 * source reaches M. */
interface Tally {
  fault: boolean;
  unableToVerify: boolean;
  authFailed: boolean;
  rateLimited: boolean;
  unreachable: boolean;
}

function shortResponse(tally: Tally): Response {
  if (tally.fault) {
    return errorJson(
      500,
      "internal",
      "a notary daemon answered with a fault or a package that disagrees",
    );
  }
  if (tally.unableToVerify) {
    return errorJson(
      503,
      "unable_to_verify",
      "too few notaries could attest the close yet",
    );
  }
  if (tally.authFailed) {
    return errorJson(
      502,
      "auth_failed",
      "notaries rejected the router's credentials: service misconfiguration",
    );
  }
  if (tally.rateLimited) {
    return errorJson(
      503,
      "rate_limited",
      "notaries are rate limiting requests; retry shortly",
    );
  }
  if (tally.unreachable) {
    return errorJson(
      503,
      "all_orgs_unhealthy",
      "too few notaries answered in time",
    );
  }
  return errorJson(
    404,
    "no_close_found",
    "no committed close bound to the requested target was found",
  );
}

type SourceOutcome =
  | { kind: "package"; body: string }
  | { kind: "stop"; response: Response }
  /** The source did not reach M, or its close binds another target. */
  | { kind: "next" };

/** Ask `source`'s daemons, M at once in a random order, each answer that does
 * not count starting the next, until M attestations of one close count. */
function attestSource(
  source: SourceEntry,
  to: string,
  agent: string,
  env: Env,
  fetchImpl: FetchLike,
  rand: () => number,
  tally: Tally,
): Promise<SourceOutcome> {
  const order = shuffled(source.notaries, rand);
  const m = source.closing_threshold;
  const signatures = new Map<string, string>();
  let fixed: { payload: string; closeAction: string } | undefined;

  /** An outcome that ends the source, or whether the answer counts. */
  const judge = (answer: DaemonAnswer): SourceOutcome | boolean => {
    switch (answer.kind) {
      case "unreachable":
        tally.unreachable = true;
        return false;
      case "malformed":
        tally.fault = true;
        return false;
      case "error":
        switch (answer.code) {
          case "warranted":
            return {
              kind: "stop",
              response: errorJson(422, "warranted", answer.message, answer.details),
            };
          case "bad_request":
            return {
              kind: "stop",
              response: errorJson(400, "bad_request", answer.message),
            };
          case "internal":
            tally.fault = true;
            break;
          case "unable_to_verify":
            tally.unableToVerify = true;
            break;
          case "auth_failed":
            tally.authFailed = true;
            break;
          case "rate_limited":
            tally.rateLimited = true;
            break;
          case "no_close_found":
            break;
        }
        return false;
      case "attestation": {
        const a = answer.attestation;
        if (a.source !== source.dna_hash) {
          tally.fault = true;
          return false;
        }
        if (!fixed && a.target !== to) return { kind: "next" };
        fixed ??= { payload: a.payload, closeAction: a.closeAction };
        if (a.payload !== fixed.payload || a.closeAction !== fixed.closeAction) {
          tally.fault = true;
          return false;
        }
        if (a.signer === agent || signatures.has(a.signer)) return false;
        signatures.set(a.signer, a.signature);
        if (signatures.size < m) return true;
        return { kind: "package", body: packageBody(fixed, [...signatures.values()]) };
      }
    }
  };

  return new Promise((resolve) => {
    let next = 0;
    let inFlight = 0;
    let settled = false;
    const settle = (outcome: SourceOutcome) => {
      settled = true;
      resolve(outcome);
    };
    const settleIfIdle = () => {
      if (inFlight > 0 || next < order.length) return;
      if (fixed) tally.unableToVerify = true;
      settle({ kind: "next" });
    };
    const ask = () => {
      if (next === order.length) return settleIfIdle();
      const daemon = order[next++];
      inFlight++;
      void attestClose(daemon.url, daemon.api, agent, env, fetchImpl).then(
        (answer) => {
          inFlight--;
          if (settled) return;
          const verdict = judge(answer);
          if (verdict === false) return ask();
          if (verdict === true) return settleIfIdle();
          settle(verdict);
        },
      );
    };
    for (let i = 0; i < m; i++) ask();
  });
}

/** The package as JSON text, built from the daemons' own text. */
function packageBody(
  fixed: { payload: string; closeAction: string },
  signatures: string[],
): string {
  return `{"payload":${fixed.payload},"notary_signatures":[${signatures.join(",")}],"close_action":${fixed.closeAction}}`;
}
