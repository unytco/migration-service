import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { unstable_readConfig } from "wrangler";
import testnetRegistry from "../registry.json";
import mainnetRegistry from "../registry.mainnet.json";

const path = (relative: string) =>
  fileURLToPath(new URL(`../${relative}`, import.meta.url).href);

interface Network {
  config: string;
  name: string;
  entry: string;
  repo: string;
  registry?: { dnas: { release_url?: string }[] };
}

const NETWORKS: Network[] = [
  {
    config: "wrangler.toml",
    name: "migration-router",
    entry: "src/index.ts",
    repo: "unytco/unyt-sandbox",
    registry: testnetRegistry,
  },
  {
    config: "wrangler.mainnet.toml",
    name: "migration-router-mainnet",
    entry: "src/index.mainnet.ts",
    repo: "unytco/unyt",
    registry: mainnetRegistry,
  },
  {
    config: "wrangler.local.toml",
    name: "migration-router-local",
    entry: "src/index.local.ts",
    repo: "unytco/unyt-sandbox",
  },
];

describe("one router per network", () => {
  it.each(NETWORKS)(
    "$config runs $name from $entry, reading $repo releases",
    ({ config, name, entry, repo }) => {
      const c = unstable_readConfig(
        { config: path(config) },
        { hideWarnings: true },
      );
      expect(c.name).toBe(name);
      expect(c.main).toBe(path(entry));
      expect(c.vars.GITHUB_RELEASES_URL).toBe(
        `https://api.github.com/repos/${repo}/releases`,
      );
    },
  );

  it("every router is its own Worker", () => {
    expect(new Set(NETWORKS.map((n) => n.name)).size).toBe(NETWORKS.length);
  });

  it.each(NETWORKS.filter((n) => n.registry))(
    "$config's registry links only $repo releases",
    ({ repo, registry }) => {
      for (const { release_url } of registry?.dnas ?? []) {
        if (release_url !== undefined)
          expect(release_url).toMatch(
            new RegExp(`^https://github\\.com/${repo}/releases/`),
          );
      }
    },
  );
});
