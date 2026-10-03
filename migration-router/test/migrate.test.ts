import { afterEach, describe, expect, it, vi } from "vitest";
import { Registry, type RawRegistry } from "../src/registry";
import { migrate, type MigrateBody } from "../src/handlers";
import type { Env, FetchLike } from "../src/notary";

// Hashes in the daemons' wire form: serde_json writes a HoloHash as its 39
// bytes (3-byte prefix, 32-byte core, 4-byte location).
const hash = (prefix: number[], seed: number) => [
  ...prefix,
  ...Array(32).fill(seed),
  0,
  0,
  0,
  0,
];
const b64 = (bytes: number[]) => "u" + Buffer.from(bytes).toString("base64url");
const dna = (seed: number) => hash([0x84, 0x2d, 0x24], seed);
const key = (seed: number) => hash([0x84, 0x20, 0x24], seed);
const action = (seed: number) => hash([0x84, 0x29, 0x24], seed);

const v01 = b64(dna(1));
const v02 = b64(dna(2));
const v03 = b64(dna(3));
const AGENT_SEED = 100;
const AGENT = b64(key(AGENT_SEED));
const DNA_BYTES: Record<string, number[]> = { [v01]: dna(1), [v02]: dna(2), [v03]: dna(3) };
const ENV: Env = { MIGRATION_NOTARY_BEARER_TOKEN: "test-token" };
const KEEP_ORDER = () => 0.99;

/** v01 has three daemons and needs two attestations; v02 one of one. */
function registry(m = 2): Registry {
  const raw: RawRegistry = {
    version: 1,
    dnas: [
      {
        dna_hash: v01,
        version: "v0.1",
        upgrade_targets: [v02, v03],
        closing_threshold: m,
        notaries: [
          { url: "https://n1", api: "v2" },
          { url: "https://n2", api: "v2" },
          { url: "https://n3", api: "v2" },
        ],
      },
      {
        dna_hash: v02,
        version: "v0.2",
        upgrades_from: v01,
        upgrade_targets: [v03],
        closing_threshold: 1,
        notaries: [{ url: "https://m1", api: "v2" }],
      },
      {
        dna_hash: v03,
        version: "v0.3",
        upgrades_from: v02,
        notaries: [{ url: "https://t1", api: "v2" }],
      },
    ],
  };
  return Registry.load(raw);
}

/** The carryover's integer-like keys are in the order the notaries signed. */
const payloadText = (source = v01, target = v02, agent = AGENT_SEED) =>
  `{"agent_pubkey":${JSON.stringify(key(agent))},"source_dna_hash":${JSON.stringify(DNA_BYTES[source])},"target_dna_hash":${JSON.stringify(DNA_BYTES[target])},"closing_state":{"agreement_carry_forward":[{"carryover":{"10":1,"9":2}}]},"chain_top":${JSON.stringify(action(7))}}`;

/** One notary's signature as a daemon serves it, spacing included. */
const sigText = (signer: number) =>
  `{"notary":${JSON.stringify(key(signer))}, "signature":${JSON.stringify(Array(64).fill(signer))}}`;

interface Attest {
  signer: number;
  source?: string;
  target?: string;
  agent?: number;
  payload?: string;
  closeAction?: number;
  signature?: string;
}

const attestText = (a: Attest) =>
  `{"payload":${a.payload ?? payloadText(a.source, a.target, a.agent)},"notary_signatures":[${a.signature ?? sigText(a.signer)}],"close_action":${JSON.stringify(action(a.closeAction ?? 6))}}`;

const attest = (a: Attest) => () =>
  new Response(attestText(a), {
    status: 200,
    headers: { "content-type": "application/json" },
  });

const daemonError = (status: number, code: string, details?: unknown) => () =>
  new Response(
    JSON.stringify({ error: { code, message: `daemon said ${code}`, details } }),
    { status, headers: { "content-type": "application/json" } },
  );

const pending = () => () => new Promise<Response>(() => {});

type Daemon = () => Response | Promise<Response>;

/** A fetch answering by daemon origin, recording every URL asked. An origin
 * with no daemon cannot be reached. */
function daemons(byOrigin: Record<string, Daemon>) {
  const asked: string[] = [];
  const requests: { url: string; init?: RequestInit }[] = [];
  const fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input.toString();
    asked.push(url);
    requests.push({ url, init });
    const origin = new URL(url).origin;
    const daemon = byOrigin[origin];
    if (!daemon) throw new TypeError(`network error: ${url}`);
    return daemon();
  }) as FetchLike;
  return { fetch, asked, requests };
}

/** `from` null: the router discovers the source. */
const pair = (to = v02, from: string | null = v01): MigrateBody => ({
  ...(from ? { from_dna_hash: from } : {}),
  to_dna_hash: to,
  agent_pubkey: AGENT,
});

async function json(resp: Response): Promise<any> {
  return resp.json();
}

async function code(resp: Response) {
  return { status: resp.status, code: (await json(resp)).error?.code };
}

describe("migrate: pair validation", () => {
  const none = daemons({}).fetch;

  it("rejects unknown to_dna, unknown from_dna, a chain root and an unreachable target", async () => {
    const r = registry();
    expect(await code(await migrate(r, pair("x"), ENV, none))).toEqual({
      status: 400,
      code: "unknown_to_dna",
    });
    expect(await code(await migrate(r, pair(v02, "x"), ENV, none))).toEqual({
      status: 400,
      code: "unknown_from_dna",
    });
    expect(await code(await migrate(r, pair(v01), ENV, none))).toEqual({
      status: 400,
      code: "to_is_chain_root",
    });
    expect(await code(await migrate(r, pair(v02, v02), ENV, none))).toEqual({
      status: 400,
      code: "unreachable_target",
    });
  });

  it("requires to_dna_hash and agent_pubkey", async () => {
    const resp = await migrate(registry(), { from_dna_hash: v01 }, ENV, none);
    expect(await code(resp)).toEqual({ status: 400, code: "bad_request" });
  });
});

describe("migrate: combining attestations", () => {
  it("asks /{api}/attest-close with the bearer token and the agent", async () => {
    const d = daemons({
      "https://n1": attest({ signer: 1 }),
      "https://n2": attest({ signer: 2 }),
    });
    await migrate(registry(), pair(), ENV, d.fetch, KEEP_ORDER);
    const req = d.requests.find((r) => r.url.startsWith("https://n1"))!;
    expect(req.url).toBe("https://n1/v2/attest-close");
    expect(req.init?.method).toBe("POST");
    expect((req.init?.headers as Record<string, string>).authorization).toBe(
      "Bearer test-token",
    );
    expect(JSON.parse(req.init?.body as string)).toEqual({ agent_pubkey: AGENT });
  });

  it("asks M daemons at once, before any answers", async () => {
    vi.useFakeTimers();
    const d = daemons({
      "https://n1": pending(),
      "https://n2": pending(),
      "https://n3": pending(),
    });
    void migrate(registry(2), pair(), ENV, d.fetch);
    await vi.advanceTimersByTimeAsync(0);
    expect(d.asked).toHaveLength(2);
  });

  it("returns M signatures over the daemons' own payload and close_action text", async () => {
    const d = daemons({
      "https://n1": attest({ signer: 1 }),
      "https://n2": attest({ signer: 2 }),
    });
    const resp = await migrate(registry(2), pair(), ENV, d.fetch, KEEP_ORDER);
    expect(resp.status).toBe(200);
    const text = await resp.text();
    expect(text).toBe(
      `{"payload":${payloadText()},"notary_signatures":[${sigText(1)},${sigText(2)}],"close_action":${JSON.stringify(action(6))}}`,
    );
    expect(text).toContain('{"10":1,"9":2}');
  });

  it("also reads hashes served as base64 strings", async () => {
    const payload = `{"agent_pubkey":"${AGENT}","source_dna_hash":"${v01}","target_dna_hash":"${v02}"}`;
    const signature = `{"notary":"${b64(key(1))}","signature":${JSON.stringify(Array(64).fill(1))}}`;
    const d = daemons({ "https://n1": attest({ signer: 1, payload, signature }) });
    const resp = await migrate(registry(1), pair(), ENV, d.fetch, KEEP_ORDER);
    expect(resp.status).toBe(200);
    expect((await json(resp)).payload.source_dna_hash).toBe(v01);
  });

  it("orders the daemons at random for each request", async () => {
    const firstAsked = async (rand: () => number) => {
      const d = daemons({
        "https://n1": attest({ signer: 1 }),
        "https://n2": attest({ signer: 2 }),
        "https://n3": attest({ signer: 3 }),
      });
      await migrate(registry(2), pair(), ENV, d.fetch, rand);
      return d.asked.map((u) => new URL(u).origin).sort();
    };
    expect(await firstAsked(KEEP_ORDER)).toEqual(["https://n1", "https://n2"]);
    expect(await firstAsked(() => 0)).toEqual(["https://n2", "https://n3"]);
  });

  const notCounting: [string, Daemon][] = [
    ["internal", daemonError(500, "internal")],
    ["a malformed 200", () => new Response("not json{", { status: 200 })],
    ["an answer for another source", attest({ signer: 1, source: v02 })],
    ["an answer for another agent", attest({ signer: 1, agent: 101 })],
    ["a malformed signature", attest({ signer: 1, signature: `{"notary":${JSON.stringify(key(1))},"signature":"x"}` })],
    ["auth_failed", daemonError(401, "auth_failed")],
    ["rate_limited", daemonError(429, "rate_limited")],
    ["a tunnel error page", () => new Response("<html>1033</html>", { status: 530 })],
  ];
  for (const [name, daemon] of notCounting) {
    it(`asks the next daemon after ${name}`, async () => {
      const d = daemons({
        "https://n1": daemon,
        "https://n2": attest({ signer: 2 }),
        "https://n3": attest({ signer: 3 }),
      });
      const resp = await migrate(registry(2), pair(), ENV, d.fetch, KEEP_ORDER);
      expect(resp.status).toBe(200);
      expect(d.asked).toHaveLength(3);
      const text = await resp.text();
      expect(text).toContain(sigText(2));
      expect(text).toContain(sigText(3));
    });
  }

  it("substitutes the next daemon for one that is unable to verify", async () => {
    const d = daemons({
      "https://n1": daemonError(503, "unable_to_verify"),
      "https://n2": attest({ signer: 2 }),
      "https://n3": attest({ signer: 3 }),
    });
    const resp = await migrate(registry(2), pair(), ENV, d.fetch, KEEP_ORDER);
    expect(resp.status).toBe(200);
    expect(d.asked).toHaveLength(3);
  });

  it("does not abandon a source over one no_close_found", async () => {
    const d = daemons({
      "https://n1": daemonError(404, "no_close_found"),
      "https://n2": attest({ signer: 2 }),
      "https://n3": attest({ signer: 3 }),
    });
    const resp = await migrate(registry(2), pair(), ENV, d.fetch, KEEP_ORDER);
    expect(resp.status).toBe(200);
  });

  it("counts one signature per notary, and never the agent's own", async () => {
    const d = daemons({
      "https://n1": attest({ signer: 1 }),
      "https://n2": attest({ signer: 1 }),
      "https://n3": attest({ signer: AGENT_SEED }),
    });
    const resp = await migrate(registry(2), pair(), ENV, d.fetch, KEEP_ORDER);
    expect(await code(resp)).toEqual({ status: 503, code: "unable_to_verify" });
  });

  it("asks the next daemon for a repeated signer", async () => {
    const d = daemons({
      "https://n1": attest({ signer: 1 }),
      "https://n2": attest({ signer: 1 }),
      "https://n3": attest({ signer: 3 }),
    });
    const resp = await migrate(registry(2), pair(), ENV, d.fetch, KEEP_ORDER);
    expect(resp.status).toBe(200);
    expect(d.asked).toHaveLength(3);
  });

  it("does not count a package that differs from the one that fixed it", async () => {
    const d = daemons({
      "https://n1": attest({ signer: 1 }),
      "https://n2": attest({ signer: 2, closeAction: 5 }),
      "https://n3": attest({ signer: 3 }),
    });
    const resp = await migrate(registry(2), pair(), ENV, d.fetch, KEEP_ORDER);
    expect(resp.status).toBe(200);
    expect(await resp.text()).not.toContain(sigText(2));
  });
});

describe("migrate: answers that end the request or the source", () => {
  it("returns 422 warranted at once, with the daemon's details", async () => {
    const d = daemons({
      "https://n1": daemonError(422, "warranted", { warrants: ["w"] }),
      "https://n2": pending(),
      "https://n3": attest({ signer: 3 }),
    });
    const resp = await migrate(registry(2), pair(), ENV, d.fetch, KEEP_ORDER);
    expect(resp.status).toBe(422);
    const b = await json(resp);
    expect(b.error.code).toBe("warranted");
    expect(b.error.details).toEqual({ warrants: ["w"] });
  });

  it("returns 400 bad_request at once", async () => {
    const d = daemons({
      "https://n1": daemonError(400, "bad_request"),
      "https://n2": pending(),
      "https://n3": attest({ signer: 3 }),
    });
    const resp = await migrate(registry(2), pair(), ENV, d.fetch, KEEP_ORDER);
    expect(await code(resp)).toEqual({ status: 400, code: "bad_request" });
  });

  it("tries the next source when a close binds another target", async () => {
    const d = daemons({
      "https://n1": attest({ signer: 1, target: v02 }),
      "https://n2": attest({ signer: 2, target: v02 }),
      "https://n3": attest({ signer: 3, target: v02 }),
      "https://m1": attest({ signer: 4, source: v02, target: v03 }),
    });
    const resp = await migrate(registry(2), pair(v03, null), ENV, d.fetch);
    expect(resp.status).toBe(200);
    expect(await resp.text()).toContain(sigText(4));
  });

  it("finds the close on a later source when an earlier one falls short", async () => {
    const d = daemons({
      "https://n1": daemonError(503, "unable_to_verify"),
      "https://n2": daemonError(503, "unable_to_verify"),
      "https://n3": daemonError(503, "unable_to_verify"),
      "https://m1": attest({ signer: 4, source: v02, target: v03 }),
    });
    const resp = await migrate(registry(2), pair(v03, null), ENV, d.fetch);
    expect(resp.status).toBe(200);
  });
});

describe("migrate: the answer when no source reaches M", () => {
  const cases: [string, Record<string, Daemon>, number, string][] = [
    [
      "a daemon fault outranks everything",
      {
        "https://n1": daemonError(500, "internal"),
        "https://n2": daemonError(503, "unable_to_verify"),
      },
      500,
      "internal",
    ],
    [
      "a 200 that is not well formed is a fault",
      {
        "https://n1": () => new Response("not json{", { status: 200 }),
        "https://n2": daemonError(404, "no_close_found"),
        "https://n3": daemonError(404, "no_close_found"),
      },
      500,
      "internal",
    ],
    [
      "an answer for another source is a fault",
      {
        "https://n1": attest({ signer: 1, source: v02 }),
        "https://n2": daemonError(404, "no_close_found"),
        "https://n3": daemonError(404, "no_close_found"),
      },
      500,
      "internal",
    ],
    [
      "a code the router does not know is a fault",
      {
        "https://n1": daemonError(418, "teapot"),
        "https://n2": daemonError(404, "no_close_found"),
        "https://n3": daemonError(404, "no_close_found"),
      },
      500,
      "internal",
    ],
    [
      "a package that differs from the one that fixed it is a fault",
      {
        "https://n1": attest({ signer: 1 }),
        "https://n2": attest({ signer: 2, closeAction: 5 }),
        "https://n3": daemonError(404, "no_close_found"),
      },
      500,
      "internal",
    ],
    [
      "a payload that differs is a fault",
      {
        "https://n1": attest({ signer: 1 }),
        "https://n2": attest({ signer: 2, payload: payloadText().replace('"10":1', '"10":2') }),
        "https://n3": daemonError(404, "no_close_found"),
      },
      500,
      "internal",
    ],
    [
      "unable_to_verify outranks auth, rate limits and outages",
      {
        "https://n1": daemonError(503, "unable_to_verify"),
        "https://n2": daemonError(401, "auth_failed"),
        "https://n3": daemonError(429, "rate_limited"),
      },
      503,
      "unable_to_verify",
    ],
    [
      "some counted but fewer than M is unable_to_verify",
      {
        "https://n1": attest({ signer: 1 }),
        "https://n2": daemonError(404, "no_close_found"),
      },
      503,
      "unable_to_verify",
    ],
    [
      "an Access refusal without an error body is auth_failed",
      {
        "https://n1": () => new Response("<html>denied</html>", { status: 403 }),
        "https://n2": daemonError(404, "no_close_found"),
        "https://n3": daemonError(404, "no_close_found"),
      },
      502,
      "auth_failed",
    ],
    [
      "a gateway error page is an unreachable daemon",
      {
        "https://n1": () => new Response("<html>530</html>", { status: 530 }),
        "https://n2": daemonError(404, "no_close_found"),
        "https://n3": daemonError(404, "no_close_found"),
      },
      503,
      "all_orgs_unhealthy",
    ],
    [
      "only the agent's own signature leaves no_close_found",
      {
        "https://n1": attest({ signer: AGENT_SEED }),
        "https://n2": daemonError(404, "no_close_found"),
        "https://n3": daemonError(404, "no_close_found"),
      },
      404,
      "no_close_found",
    ],
    [
      "auth_failed outranks rate limits",
      {
        "https://n1": daemonError(401, "auth_failed"),
        "https://n2": daemonError(429, "rate_limited"),
      },
      502,
      "auth_failed",
    ],
    [
      "rate_limited outranks an outage",
      { "https://n1": daemonError(429, "rate_limited") },
      503,
      "rate_limited",
    ],
    [
      "an unreachable daemon outranks no_close_found",
      { "https://n1": daemonError(404, "no_close_found") },
      503,
      "all_orgs_unhealthy",
    ],
    [
      "every daemon saying no_close_found is 404",
      {
        "https://n1": daemonError(404, "no_close_found"),
        "https://n2": daemonError(404, "no_close_found"),
        "https://n3": daemonError(404, "no_close_found"),
      },
      404,
      "no_close_found",
    ],
  ];

  for (const [name, byOrigin, status, errorCode] of cases) {
    it(name, async () => {
      const d = daemons(byOrigin);
      const resp = await migrate(registry(2), pair(), ENV, d.fetch, KEEP_ORDER);
      expect(await code(resp)).toEqual({ status, code: errorCode });
    });
  }

  it("ranks what every source's daemons said", async () => {
    for (const [first, status, errorCode] of [
      ["unable_to_verify", 503, "unable_to_verify"],
      ["internal", 500, "internal"],
    ] as const) {
      const d = daemons({
        "https://n1": daemonError(503, first),
        "https://n2": daemonError(503, first),
        "https://n3": daemonError(503, first),
        "https://m1": daemonError(404, "no_close_found"),
      });
      const resp = await migrate(registry(2), pair(v03, null), ENV, d.fetch);
      expect(await code(resp)).toEqual({ status, code: errorCode });
    }
  });

  it("every close binding elsewhere is 404", async () => {
    const d = daemons({
      "https://n1": attest({ signer: 1, target: v02 }),
      "https://n2": attest({ signer: 2, target: v02 }),
      "https://m1": daemonError(404, "no_close_found"),
    });
    const resp = await migrate(registry(2), pair(v03, null), ENV, d.fetch, KEEP_ORDER);
    expect(await code(resp)).toEqual({ status: 404, code: "no_close_found" });
  });

  it("a daemon that has not answered in 10 s does not count", async () => {
    vi.useFakeTimers();
    const answering = daemons({
      "https://n1": attest({ signer: 1 }),
      "https://n3": daemonError(404, "no_close_found"),
    });
    const fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
      if (!input.toString().startsWith("https://n2")) {
        return answering.fetch(input, init);
      }
      return new Promise<Response>((_, reject) =>
        init?.signal?.addEventListener("abort", () => reject(new Error("aborted"))),
      );
    }) as FetchLike;
    let settled = false;
    const result = migrate(registry(2), pair(), ENV, fetch, KEEP_ORDER);
    void result.then(() => (settled = true));
    await vi.advanceTimersByTimeAsync(9_999);
    expect(settled).toBe(false);
    expect(answering.asked).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(await code(await result)).toEqual({ status: 503, code: "unable_to_verify" });
    expect(answering.asked).toHaveLength(2);
  });
});

afterEach(() => {
  vi.useRealTimers();
});

describe("migrate: customers-last", () => {
  it("serves an unpublished successor's package", async () => {
    const r = Registry.load({
      version: 1,
      dnas: [
        {
          dna_hash: v01,
          version: "v0.1",
          upgrade_targets: [v02],
          closing_threshold: 1,
          notaries: [{ url: "https://n1", api: "v2" }],
        },
        {
          dna_hash: v02,
          version: "v0.2",
          upgrades_from: v01,
          published: false,
          notaries: [{ url: "https://m1", api: "v2" }],
        },
      ],
    });
    const d = daemons({ "https://n1": attest({ signer: 1 }) });
    expect((await migrate(r, pair(), ENV, d.fetch)).status).toBe(200);
  });
});
