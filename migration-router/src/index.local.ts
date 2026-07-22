// Cloudflare Worker entry point — LOCAL-TESTNET mode (documentation/specs/
// local-testnet/). Loads migration-router/registry.local.json (gitignored; start from
// registry.local.example.json) with http:// notary URLs admitted, so a local
// registry can name plain-http daemons on container IPs. Run via
// `npm run dev:local` (wrangler dev -c wrangler.local.toml); the deployed
// Worker builds from index.ts and never contains this entry point. Edits to
// registry.local.json hot-reload — the dev watcher re-bundles JSON imports.

import registryJson from "../registry.local.json";
import { Registry, type RawRegistry } from "./registry";
import { createWorker, warnIfUnprovisioned } from "./worker";

const registry = Registry.load(registryJson as RawRegistry, {
  allowHttpNotaries: true,
});

warnIfUnprovisioned(registryJson as RawRegistry);

export default createWorker(registry);
