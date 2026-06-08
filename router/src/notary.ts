// Client for calling a notary daemon's /v1/notarize over its Cloudflare Tunnel.

import type { ErrorCode } from "./errors";

export interface Env {
  /** Bearer token shared with every notary daemon. */
  MIGRATION_NOTARY_BEARER_TOKEN: string;
  /** Cloudflare Access service-token credentials (so only this Worker reaches the daemon). */
  CF_ACCESS_CLIENT_ID?: string;
  CF_ACCESS_CLIENT_SECRET?: string;
}

/** Injectable fetch so tests can mock daemon responses. */
export type FetchLike = typeof fetch;

export interface VerifiedResult {
  kind: "verified";
  payload: unknown; // opaque SummaryStatePayload — forwarded verbatim
  signature: unknown; // opaque Signature — forwarded verbatim
}

export interface HardStop {
  kind: "hard_stop";
  status: number;
  code: ErrorCode; // warranted | no_close_found | too_new
  message: string;
  details?: unknown;
}

/** Transient: this notary failed, but another may succeed. `code` carries the
 * daemon's machine-readable cause (`unable_to_verify`, `auth_failed`,
 * `rate_limited`, `internal`, …) or `all_orgs_unhealthy` for an unreachable
 * daemon, so the router can aggregate and surface the most informative final error
 * — e.g. a fleet-wide `auth_failed` (shared-token misconfig) must not read as a
 * generic outage. */
export interface Transient {
  kind: "transient";
  code: string;
}

export type NotarizeOutcome = VerifiedResult | HardStop | Transient;

const HARD_STOP_CODES: ReadonlySet<string> = new Set(["warranted", "no_close_found", "too_new"]);

/** Call one daemon's /v1/notarize. Never throws — transport failures map to `transient`. */
export async function notarize(
  daemonUrl: string,
  agentPubkey: string,
  env: Env,
  fetchImpl: FetchLike,
): Promise<NotarizeOutcome> {
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
    resp = await fetchImpl(`${daemonUrl.replace(/\/$/, "")}/v1/notarize`, {
      method: "POST",
      headers,
      body: JSON.stringify({ agent_pubkey: agentPubkey }),
    });
  } catch {
    // transport failure: daemon unreachable / unhealthy
    return { kind: "transient", code: "all_orgs_unhealthy" };
  }

  if (resp.status === 200) {
    const body = (await resp.json()) as { payload: unknown; signature: unknown };
    return { kind: "verified", payload: body.payload, signature: body.signature };
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
  // Not a hard stop: this daemon failed but another candidate may succeed. Preserve
  // the daemon's code (unable_to_verify, auth_failed, rate_limited, internal, …) so
  // the router can surface the real cause if every candidate fails the same way.
  return { kind: "transient", code };
}
