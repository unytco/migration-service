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

/** One daemon's attestation: `payload`, `closeAction` and `signature` as the
 * exact text it served, the hashes read out of them as base64. */
export interface Attestation {
  payload: string;
  closeAction: string;
  signature: string;
  signer: string;
  agent: string;
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
  | { kind: "malformed"; reason: string }
  | { kind: "error"; code: DaemonCode; message: string; details?: unknown }
  /** No answer within the budget, no connection, or a gateway error in front
   * of the daemon. */
  | { kind: "unreachable"; reason: string };

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
  if (isBytes(value, 39)) {
    const bin = String.fromCharCode(...value);
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
  } catch (e) {
    return { kind: "unreachable", reason: String(e) };
  } finally {
    clearTimeout(timer);
  }

  if (status === 200) {
    const attestation = parseAttestation(text);
    return typeof attestation === "string"
      ? { kind: "malformed", reason: attestation }
      : { kind: "attestation", attestation };
  }
  return parseError(status, text);
}

/** The attestation, or why the 200 is not one. */
function parseAttestation(text: string): Attestation | string {
  const body = rawMembers(text);
  if (!body) return "not a JSON object";
  const payload = body.get("payload");
  const closeAction = body.get("close_action");
  const signatures = rawElements(body.get("notary_signatures") ?? "");
  if (!payload || !closeAction) return "no payload or close_action";
  if (signatures?.length !== 1) return "not exactly one signature";
  const hashes = JSON.parse(payload) as {
    agent_pubkey?: unknown;
    source_dna_hash?: unknown;
    target_dna_hash?: unknown;
  } | null;
  const signed = JSON.parse(signatures[0]) as {
    notary?: unknown;
    signature?: unknown;
  } | null;
  const agent = normalizeHashB64(hashes?.agent_pubkey);
  const source = normalizeHashB64(hashes?.source_dna_hash);
  const target = normalizeHashB64(hashes?.target_dna_hash);
  const signer = normalizeHashB64(signed?.notary);
  if (!agent || !source || !target) return "a payload hash is missing or malformed";
  if (!signer || !isBytes(signed?.signature, 64)) return "a malformed signature";
  if (!normalizeHashB64(JSON.parse(closeAction))) return "a malformed close_action";
  return {
    payload,
    closeAction,
    signature: signatures[0],
    signer,
    agent,
    source,
    target,
  };
}

function isBytes(value: unknown, length: number): value is number[] {
  return (
    Array.isArray(value) &&
    value.length === length &&
    value.every((b) => Number.isInteger(b) && b >= 0 && b <= 255)
  );
}

function parseError(status: number, text: string): DaemonAnswer {
  let body: { error?: { code?: unknown; message?: unknown; details?: unknown } };
  try {
    body = JSON.parse(text);
  } catch {
    body = {};
  }
  const code = body?.error?.code;
  if (DAEMON_CODES.includes(code as DaemonCode)) {
    const message = body.error?.message;
    return {
      kind: "error",
      code: code as DaemonCode,
      message: typeof message === "string" ? message : String(code),
      details: body.error?.details,
    };
  }
  if (code !== undefined) {
    return {
      kind: "error",
      code: "internal",
      message: `notary daemon answered an unknown code ${String(code)}`,
    };
  }
  // No error envelope: the answer came from in front of the daemon, the tunnel
  // or Cloudflare Access, not from the daemon itself.
  const reason = `HTTP ${status} without an error body`;
  if (status === 401 || status === 403) {
    return { kind: "error", code: "auth_failed", message: reason };
  }
  if (status === 429) return { kind: "error", code: "rate_limited", message: reason };
  if (status >= 500) return { kind: "unreachable", reason };
  return { kind: "error", code: "internal", message: reason };
}
