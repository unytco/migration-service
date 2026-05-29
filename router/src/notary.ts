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

/** Transient: this notary failed, but another may succeed. `code` distinguishes
 * a reachable-but-couldn't-verify daemon (`unable_to_verify`) from an
 * unreachable/unhealthy one (`all_orgs_unhealthy`), so the router can aggregate. */
export interface Transient {
  kind: "transient";
  code: "unable_to_verify" | "all_orgs_unhealthy";
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
  // A reachable daemon that couldn't verify → unable_to_verify; any other error
  // (internal, auth_failed, unexpected) → treat the daemon as unhealthy and try
  // the next candidate.
  return {
    kind: "transient",
    code: code === "unable_to_verify" ? "unable_to_verify" : "all_orgs_unhealthy",
  };
}
