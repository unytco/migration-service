// Client for one notary daemon's /{api}/attest-close over its Cloudflare Tunnel:
// the daemon reads the agent's closed chain on its own conductor and answers
// with its own signature over the close.

import { rawElements, rawMembers } from "./raw-json";

export interface Env {
  /** Bearer token shared with every notary daemon. */
  MIGRATION_NOTARY_BEARER_TOKEN: string;
  /** Cloudflare Access service-token credentials (so only this Worker reaches the daemon). */
  CF_ACCESS_CLIENT_ID?: string;
  CF_ACCESS_CLIENT_SECRET?: string;
  /** Optional read-only GitHub token for /v1/update-check's build lookup — unauthenticated by
   * default; set only to raise the rate ceiling if the live-lineage count ever grows. */
  GITHUB_TOKEN?: string;
  /** Local-testnet only (never set on the deployed Worker): point the build axis at a local
   * artifact server speaking the GitHub releases JSON shape. See builds.ts `releasesApi`. */
  GITHUB_RELEASES_URL?: string;
}

/** Injectable fetch so tests can mock daemon responses. */
export type FetchLike = typeof fetch;

/** One daemon's attestation, each JSON field as the exact text it served. */
export interface Attestation {
  payload: string;
  closeAction: string;
  signature: string;
  /** The signing notary, base64. */
  signer: string;
  /** `payload.source_dna_hash` and `payload.target_dna_hash`, base64. */
  source: string;
  target: string;
}

/** The daemon codes the router acts on. Anything else a daemon answers is a
 * daemon fault, so no code the client does not know reaches it. */
const DAEMON_CODES = [
  "warranted",
  "bad_request",
  "no_close_found",
  "unable_to_verify",
  "auth_failed",
  "rate_limited",
  "internal",
] as const;
export type DaemonCode = (typeof DAEMON_CODES)[number];

export type DaemonAnswer =
  | { kind: "attestation"; attestation: Attestation }
  /** A 200 that is not one well-formed attestation. */
  | { kind: "malformed" }
  | { kind: "error"; code: DaemonCode; message: string; details?: unknown }
  /** No answer within the budget, or no connection. */
  | { kind: "unreachable" };

/** An answer later than this does not count. */
const ATTEST_TIMEOUT_MS = 10_000;

/**
 * The base64 form of a HoloHash the daemon may serialize EITHER as its b64
 * string or as its raw byte array (holo_hash serializes to bytes in JSON).
 * Only a 39-element array of integers in 0..=255 is a HoloHash (3-byte prefix,
 * 32-byte hash, 4-byte location); anything else is `undefined`.
 */
export function normalizeHashB64(value: unknown): string | undefined {
  if (typeof value === "string") return value;
  if (
    Array.isArray(value) &&
    value.length === 39 &&
    value.every(
      (b) => typeof b === "number" && Number.isInteger(b) && b >= 0 && b <= 255,
    )
  ) {
    const bin = String.fromCharCode(...(value as number[]));
    return "u" + btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }
  return undefined;
}

/** Ask one daemon to attest `agentPubkey`'s close. Never throws. */
export async function attestClose(
  daemonUrl: string,
  api: string,
  agentPubkey: string,
  env: Env,
  fetchImpl: FetchLike,
): Promise<DaemonAnswer> {
  const headers: Record<string, string> = {
    "content-type": "application/json",
    authorization: `Bearer ${env.MIGRATION_NOTARY_BEARER_TOKEN}`,
  };
  if (env.CF_ACCESS_CLIENT_ID && env.CF_ACCESS_CLIENT_SECRET) {
    headers["CF-Access-Client-Id"] = env.CF_ACCESS_CLIENT_ID;
    headers["CF-Access-Client-Secret"] = env.CF_ACCESS_CLIENT_SECRET;
  }

  const timeout = new AbortController();
  const timer = setTimeout(() => timeout.abort(), ATTEST_TIMEOUT_MS);
  let status: number;
  let text: string;
  try {
    const resp = await fetchImpl(
      `${daemonUrl.replace(/\/$/, "")}/${api}/attest-close`,
      {
        method: "POST",
        headers,
        body: JSON.stringify({ agent_pubkey: agentPubkey }),
        signal: timeout.signal,
      },
    );
    status = resp.status;
    text = await resp.text();
  } catch {
    return { kind: "unreachable" };
  } finally {
    clearTimeout(timer);
  }

  if (status === 200) {
    const attestation = parseAttestation(text);
    return attestation ? { kind: "attestation", attestation } : { kind: "malformed" };
  }
  return parseError(text);
}

function parseAttestation(text: string): Attestation | undefined {
  const body = rawMembers(text);
  const payload = body?.get("payload");
  const closeAction = body?.get("close_action");
  const signatures = rawElements(body?.get("notary_signatures") ?? "");
  if (!payload || !closeAction || signatures?.length !== 1) return undefined;
  const hashes = JSON.parse(payload) as {
    source_dna_hash?: unknown;
    target_dna_hash?: unknown;
  } | null;
  const signed = JSON.parse(signatures[0]) as { notary?: unknown } | null;
  const source = normalizeHashB64(hashes?.source_dna_hash);
  const target = normalizeHashB64(hashes?.target_dna_hash);
  const signer = normalizeHashB64(signed?.notary);
  if (!source || !target || !signer || JSON.parse(closeAction) === null) {
    return undefined;
  }
  return { payload, closeAction, signature: signatures[0], signer, source, target };
}

function parseError(text: string): DaemonAnswer {
  let body: { error?: { code?: unknown; message?: unknown; details?: unknown } };
  try {
    body = JSON.parse(text);
  } catch {
    body = {};
  }
  const code = body?.error?.code;
  if (!DAEMON_CODES.includes(code as DaemonCode)) {
    return { kind: "error", code: "internal", message: "notary daemon fault" };
  }
  const message = body.error?.message;
  return {
    kind: "error",
    code: code as DaemonCode,
    message: typeof message === "string" ? message : String(code),
    details: body.error?.details,
  };
}
