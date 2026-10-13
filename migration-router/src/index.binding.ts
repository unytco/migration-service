// Cloudflare Worker entry point — a DEPLOYMENT THAT BRINGS ITS OWN REGISTRY.
// The registry arrives in the REGISTRY_JSON binding at deploy time, so one
// published bundle serves any environment: fleet-command deploys one per
// environment, serving every release in it from that environment's version
// chain and notaries. Released as migration-router-worker.js (see
// .github/workflows/release.yml).
//
// Bundles no registry.json, deliberately: a deployment missing the binding
// fails every request, /healthz included, instead of quietly serving the
// shared chain that index.ts carries.

import { createWorker, registryFromBinding } from "./worker";

export default createWorker(registryFromBinding());
