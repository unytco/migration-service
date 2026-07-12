// Client for calling a notary daemon's /{api}/fetch-close over its Cloudflare
// Tunnel: the daemon serves the agent's committed closing summary from its own
// conductor, and the router hands it back to the app verbatim.

import type { ErrorCode } from "./errors";

export interface Env {
  /** Bearer token shared with every notary daemon. */
  MIGRATION_NOTARY_BEARER_TOKEN: string;
  /** Cloudflare Access service-token credentials (so only this Worker reaches the daemon). */
  CF_ACCESS_CLIENT_ID?: string;
  CF_ACCESS_CLIENT_SECRET?: string;
  /** Optional read-only GitHub token for /v1/update-check's build lookup — unauthenticated by
   * default; set only to raise the rate ceiling if the live-lineage count ever grows. */
  GITHUB_TOKEN?: string;
}

/** Injectable fetch so tests can mock daemon responses. */
export type FetchLike = typeof fetch;

export interface PackageResult {
  kind: "package"; // opaque closing-summary package — forwarded verbatim
  payload: unknown;
  notary_signatures: unknown;
  close_action: unknown;
  /** The successor this close is bound to, read from `payload.target_dna_hash`, so
   * the handler can target-filter a discovered close (skipping a stale one bound
   * to a different version). */
  target_dna_hash: unknown;
}

export interface HardStop {
  kind: "hard_stop";
  status: number;
  code: ErrorCode; // warranted | no_close_found | bad_request | internal (dna_hash mismatch)
  message: string;
  details?: unknown;
}

/** Transient: this notary failed, but another may succeed. `code` carries the daemon's
 * cause (or `all_orgs_unhealthy` when unreachable) so the router can aggregate and
 * surface the most informative final error. */
export interface Transient {
  kind: "transient";
  code: string;
}

export type FetchCloseOutcome = PackageResult | HardStop | Transient;

// Codes the router must not retry across notaries: a content verdict every notary
// returns identically (`warranted` / `no_close_found`), or a client input error
// (`bad_request`) the next notary would reject the same way.
const HARD_STOP_CODES: ReadonlySet<string> = new Set([
  "warranted",
  "no_close_found",
  "bad_request",
]);

/** Per-call budget: a daemon that accepts the socket but never responds maps to
 * `transient` after this, so the candidate loop advances instead of stalling /v1/migrate. */
const FETCH_CLOSE_TIMEOUT_MS = 10_000;

/** Call one daemon's /{api}/fetch-close. Never throws — transport failures, timeouts,
 * and malformed success bodies all map to `transient`. A success payload whose
 * `source_dna_hash` differs from `expectedDnaHash` is a misconfigured notary →
 * `internal` hard stop (B2). The package's `target_dna_hash` is surfaced for the
 * handler's target-filter. */
export async function fetchClose(
  daemonUrl: string,
  api: string,
  agentPubkey: string,
  expectedDnaHash: string,
  env: Env,
  fetchImpl: FetchLike,
): Promise<FetchCloseOutcome> {
  const headers: Record<string, string> = {
    "content-type": "application/json",
    authorization: `Bearer ${env.MIGRATION_NOTARY_BEARER_TOKEN}`,
  };
  if (env.CF_ACCESS_CLIENT_ID && env.CF_ACCESS_CLIENT_SECRET) {
    headers["CF-Access-Client-Id"] = env.CF_ACCESS_CLIENT_ID;
    headers["CF-Access-Client-Secret"] = env.CF_ACCESS_CLIENT_SECRET;
  }

  let resp: Response;
  try {
    resp = await fetchImpl(`${daemonUrl.replace(/\/$/, "")}/${api}/fetch-close`, {
      method: "POST",
      headers,
      body: JSON.stringify({ agent_pubkey: agentPubkey }),
      signal: AbortSignal.timeout(FETCH_CLOSE_TIMEOUT_MS),
    });
  } catch {
    return { kind: "transient", code: "all_orgs_unhealthy" };
  }

  if (resp.status === 200) {
    // B3: a 200 can still carry a truncated/non-JSON body — guard the parse (the
    // "Never throws" contract) and fail over rather than escape the candidate loop.
    let body: {
      payload?: unknown;
      notary_signatures?: unknown;
      close_action?: unknown;
    };
    try {
      body = (await resp.json()) as typeof body;
    } catch {
      return { kind: "transient", code: "unable_to_verify" };
    }
    // A 200 missing any package field (or null) is as malformed as a non-JSON body — fail over.
    if (body.payload == null || body.notary_signatures == null || body.close_action == null) {
      return { kind: "transient", code: "unable_to_verify" };
    }
    // B2: the daemon serves exactly one DNA, so a wrong `source_dna_hash` is a
    // misconfigured notary (registry URL ↔ daemon mismatch) — reject as `internal`.
    const payloadSourceDna = (body.payload as { source_dna_hash?: unknown } | undefined)
      ?.source_dna_hash;
    if (typeof payloadSourceDna === "string" && payloadSourceDna !== expectedDnaHash) {
      return {
        kind: "hard_stop",
        status: 500,
        code: "internal",
        message: "notary returned a payload for a different source DNA",
        details: { expected_dna_hash: expectedDnaHash, got_dna_hash: payloadSourceDna },
      };
    }
    return {
      kind: "package",
      payload: body.payload,
      notary_signatures: body.notary_signatures,
      close_action: body.close_action,
      target_dna_hash: (body.payload as { target_dna_hash?: unknown } | undefined)?.target_dna_hash,
    };
  }

  let code = "internal";
  let message = `notary returned ${resp.status}`;
  let details: unknown;
  try {
    const body = (await resp.json()) as { error?: { code?: string; message?: string; details?: unknown } };
    if (body.error?.code) code = body.error.code;
    if (body.error?.message) message = body.error.message;
    details = body.error?.details;
  } catch {
    /* non-JSON error body — keep defaults */
  }

  if (HARD_STOP_CODES.has(code)) {
    return { kind: "hard_stop", status: resp.status, code: code as ErrorCode, message, details };
  }
  // Not a hard stop: preserve the daemon's code so the router can surface the real
  // cause if every candidate fails the same way.
  return { kind: "transient", code };
}
