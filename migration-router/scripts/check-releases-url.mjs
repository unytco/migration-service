import { fileURLToPath } from "node:url";
import { unstable_readConfig } from "wrangler";

const TIMEOUT_MS = 10_000;

/**
 * Why a router built from `configPath` must not deploy, or null when its releases URL answers
 * 200 to a request without a token, as the deployed router's requests are.
 *
 * @param {string} configPath
 * @param {typeof fetch} fetchImpl
 * @returns {Promise<string | null>}
 */
export async function releasesUrlProblem(configPath, fetchImpl) {
  const { vars } = unstable_readConfig(
    { config: configPath },
    { hideWarnings: true },
  );
  const url = vars.GITHUB_RELEASES_URL;
  if (typeof url !== "string" || !url) {
    return `${configPath} sets no GITHUB_RELEASES_URL`;
  }
  let resp;
  try {
    resp = await fetchImpl(url, {
      headers: {
        accept: "application/vnd.github+json",
        "user-agent": "unyt-migration-router-deploy",
      },
      signal: AbortSignal.timeout(TIMEOUT_MS),
    });
  } catch (err) {
    return `${url} did not answer (${err})`;
  }
  return resp.status === 200 ? null : `${url} answered HTTP ${resp.status}`;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const configPath = process.argv[2] ?? "wrangler.toml";
  const problem = await releasesUrlProblem(configPath, fetch);
  if (problem) {
    console.error(`Refusing to deploy ${configPath}: ${problem}`);
    process.exit(1);
  }
}
