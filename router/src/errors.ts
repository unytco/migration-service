// Uniform error envelope shared shape with the notary daemon.
// `code` is the stable machine-readable contract; clients switch on it.

export type ErrorCode =
  | "unknown_to_dna"
  | "unknown_from_dna"
  | "to_is_chain_root"
  | "not_registered_predecessor"
  | "auth_failed"
  | "rate_limited"
  | "warranted"
  | "no_close_found"
  | "too_new"
  | "all_orgs_unhealthy"
  | "unable_to_verify"
  | "internal";

export interface ErrorBody {
  error: {
    code: ErrorCode;
    message: string;
    details?: unknown;
  };
}

const JSON_HEADERS = { "content-type": "application/json" };

export function errorJson(
  status: number,
  code: ErrorCode,
  message: string,
  details?: unknown,
): Response {
  const body: ErrorBody = { error: { code, message, ...(details !== undefined ? { details } : {}) } };
  return new Response(JSON.stringify(body), { status, headers: JSON_HEADERS });
}

export function ok(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: JSON_HEADERS });
}
