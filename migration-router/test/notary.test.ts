import { describe, expect, it } from "vitest";
import { attestClose, normalizeHashB64, type FetchLike } from "../src/notary";

// A HoloHash is exactly 39 bytes; the router accepts one either as its b64 string
// or as that raw byte array, which is how serde_json encodes holo_hash.
describe("normalizeHashB64", () => {
  it("passes a b64 string through unchanged", () => {
    expect(normalizeHashB64("uhC0k_whatever")).toBe("uhC0k_whatever");
  });

  it("encodes a 39-byte HoloHash array as unpadded base64url with the 'u' prefix", () => {
    const bytes = [0x84, 0x2d, 0x24, ...Array(32).fill(0), 0xde, 0xad, 0xbe, 0xef]; // 39
    // Node's base64url is an independent oracle for the same transform.
    const expected = "u" + Buffer.from(bytes).toString("base64url");
    expect(normalizeHashB64(bytes)).toBe(expected);
    expect(normalizeHashB64(bytes)).not.toMatch(/[+/=]/); // url-safe, unpadded
  });

  it("rejects arrays that are not exactly 39 bytes", () => {
    expect(normalizeHashB64([])).toBeUndefined();
    expect(normalizeHashB64(Array(38).fill(0))).toBeUndefined();
    expect(normalizeHashB64(Array(40).fill(0))).toBeUndefined();
  });

  it("rejects a 39-length array carrying a non-byte value (out of range / non-integer / non-number)", () => {
    expect(normalizeHashB64([...Array(38).fill(0), 256])).toBeUndefined();
    expect(normalizeHashB64([...Array(38).fill(0), -1])).toBeUndefined();
    expect(normalizeHashB64([...Array(38).fill(0), 1.5])).toBeUndefined();
    expect(
      normalizeHashB64([...Array(38).fill(0), "0" as unknown as number]),
    ).toBeUndefined();
  });

  it("returns undefined for null / undefined / non-array / object shapes", () => {
    expect(normalizeHashB64(null)).toBeUndefined();
    expect(normalizeHashB64(undefined)).toBeUndefined();
    expect(normalizeHashB64(42)).toBeUndefined();
    expect(normalizeHashB64({})).toBeUndefined();
  });
});

describe("attestClose", () => {
  const answer = (status: number, body: string): FetchLike =>
    (async () => new Response(body, { status })) as FetchLike;
  const ask = (fetch: FetchLike) =>
    attestClose("https://n1/", "v2", "uhCAk_agent", { MIGRATION_NOTARY_BEARER_TOKEN: "t" }, fetch);
  const sig = `{"notary":"uhCAk_n1","signature":${JSON.stringify(Array(64).fill(1))}}`;
  const pay = `{"agent_pubkey":"uhCAk_agent","source_dna_hash":"uhC0k_a","target_dna_hash":"uhC0k_b"}`;
  const body = (payload: string, signatures: string, closeAction: string) =>
    `{"payload":${payload},"notary_signatures":[${signatures}],"close_action":${closeAction}}`;

  it("reads one attestation, keeping the served text", async () => {
    expect(await ask(answer(200, body(pay, sig, `"uhCkk_c"`)))).toEqual({
      kind: "attestation",
      attestation: {
        payload: pay,
        closeAction: `"uhCkk_c"`,
        signature: sig,
        signer: "uhCAk_n1",
        agent: "uhCAk_agent",
        source: "uhC0k_a",
        target: "uhC0k_b",
      },
    });
  });

  it("calls a 200 that is not exactly one attestation malformed", async () => {
    for (const text of [
      body(pay, `${sig},${sig}`, `"uhCkk_c"`),
      body(pay, "", `"uhCkk_c"`),
      `{"payload":${pay},"notary_signatures":[${sig}]}`,
      body(pay, sig, "null"),
      body(pay, sig, "[6]"),
      body("null", sig, `"uhCkk_c"`),
      body(`{"source_dna_hash":"uhC0k_a","target_dna_hash":"uhC0k_b"}`, sig, `"uhCkk_c"`),
      body(pay, `{"notary":[1],"signature":${JSON.stringify(Array(64).fill(1))}}`, `"uhCkk_c"`),
      body(pay, `{"notary":"uhCAk_n1","signature":[1]}`, `"uhCkk_c"`),
      "not json{",
    ]) {
      expect((await ask(answer(200, text))).kind, text).toBe("malformed");
    }
  });

  it("passes a known daemon code on, with its message and details", async () => {
    const text = JSON.stringify({
      error: { code: "warranted", message: "w", details: { warrants: [1] } },
    });
    expect(await ask(answer(422, text))).toEqual({
      kind: "error",
      code: "warranted",
      message: "w",
      details: { warrants: [1] },
    });
  });

  it("turns an unknown code, or an answer without an error body, into internal", async () => {
    for (const [status, text] of [
      [502, JSON.stringify({ error: { code: "teapot" } })],
      [404, ""],
      [418, "<html>"],
    ] as const) {
      const got = await ask(answer(status, text));
      expect(got.kind === "error" && got.code, text).toBe("internal");
    }
  });

  it("reads an answer from in front of the daemon by its status", async () => {
    const kindOf = async (status: number) => {
      const got = await ask(answer(status, "<html>edge</html>"));
      return got.kind === "error" ? got.code : got.kind;
    };
    expect(await kindOf(401)).toBe("auth_failed");
    expect(await kindOf(403)).toBe("auth_failed");
    expect(await kindOf(429)).toBe("rate_limited");
    expect(await kindOf(502)).toBe("unreachable");
    expect(await kindOf(530)).toBe("unreachable");
  });

  it("sends the Cloudflare Access credentials when both are set", async () => {
    let headers: Record<string, string> = {};
    const fetch = (async (_: RequestInfo | URL, init?: RequestInit) => {
      headers = init?.headers as Record<string, string>;
      return new Response("", { status: 404 });
    }) as FetchLike;
    await attestClose(
      "https://n1",
      "v2",
      "uhCAk_agent",
      { MIGRATION_NOTARY_BEARER_TOKEN: "t", CF_ACCESS_CLIENT_ID: "id", CF_ACCESS_CLIENT_SECRET: "s" },
      fetch,
    );
    expect(headers["CF-Access-Client-Id"]).toBe("id");
    expect(headers["CF-Access-Client-Secret"]).toBe("s");
  });

  it("calls a failed connection unreachable", async () => {
    const fetch = (async () => {
      throw new TypeError("connection refused");
    }) as FetchLike;
    expect((await ask(fetch)).kind).toBe("unreachable");
  });
});
