// Cloudflare Worker entry point — the DEPLOYED configuration. Loads the bundled
// registry with the strict validator (https-only notary URLs); the local-testnet
// relaxation lives solely in index.local.ts and is structurally absent here.

import registryJson from "../registry.json";
import { Registry, type RawRegistry } from "./registry";
import { createWorker, warnIfUnprovisioned } from "./worker";

// Parse + validate the registry once at module load. A bad registry throws here,
// so the Worker fails fast (and /healthz never returns ok).
const registry = Registry.load(registryJson as RawRegistry);

warnIfUnprovisioned(registryJson as RawRegistry);

export default createWorker(registry);
