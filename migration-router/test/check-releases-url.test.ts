import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { releasesUrlProblem } from "../scripts/check-releases-url.mjs";

const MAINNET_CONFIG = fileURLToPath(
  new URL("../wrangler.mainnet.toml", import.meta.url).href,
);
const MAINNET_RELEASES =
  "https://api.github.com/repos/unytco/unyt-release/releases";

function fetchAnswering(answer: () => Promise<Response>) {
  const asked: { url: string; headers: Headers }[] = [];
  const fetchImpl = (async (input: RequestInfo | URL, init?: RequestInit) => {
    asked.push({ url: String(input), headers: new Headers(init?.headers) });
    return answer();
  }) as typeof fetch;
  return { fetchImpl, asked };
}

describe("the pre-deploy releases check", () => {
  it("passes when the config's releases URL answers 200 to a request without a token", async () => {
    const { fetchImpl, asked } = fetchAnswering(async () => Response.json([]));
    expect(await releasesUrlProblem(MAINNET_CONFIG, fetchImpl)).toBeNull();
    expect(asked.map((a) => a.url)).toEqual([MAINNET_RELEASES]);
    expect(asked[0].headers.has("authorization")).toBe(false);
  });

  it("refuses a 404, naming the URL and status", async () => {
    const { fetchImpl } = fetchAnswering(
      async () => new Response("Not Found", { status: 404 }),
    );
    expect(await releasesUrlProblem(MAINNET_CONFIG, fetchImpl)).toBe(
      `${MAINNET_RELEASES} answered HTTP 404`,
    );
  });

  it("refuses when the URL does not answer, naming the URL", async () => {
    const { fetchImpl } = fetchAnswering(async () => {
      throw new TypeError("fetch failed");
    });
    expect(await releasesUrlProblem(MAINNET_CONFIG, fetchImpl)).toBe(
      `${MAINNET_RELEASES} did not answer (TypeError: fetch failed)`,
    );
  });
});
