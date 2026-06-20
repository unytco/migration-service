# migration-service

Off-chain pipeline for unyt DNA migration. Lets an app fetch its committed closing summary over HTTP — instead of hand-driving zome calls against someone else's conductor — and open on the successor DNA with it.

Two components:

- **`router/`** — a Cloudflare Worker (TypeScript). Public HTTP entry point. Reads a bundled registry (version chain + per-DNA notary endpoints, each pinned to a daemon API version), validates the `(from_dna_hash, to_dna_hash)` pair against the `upgrades_from` chain, tries the from-DNA's notary daemons in per-request random order (transient failures fail over; hard stops return immediately), calls the chosen daemon's `/{api}/fetch-close`, and returns the closing-summary package `{ payload, notary_signatures, close_action }` verbatim to the app. Holds no keys and never interprets the payload.

- **`notary-daemon/`** — a Rust `axum` + [`ham`](https://github.com/unytco/ham) service. Runs co-located with a Holochain conductor that keeps serving the old (from-DNA) network. Its `/v1/fetch-close` calls the alliance `read_predecessor_close` zome fn — a pure read of the agent's committed close; the M-of-N notary signatures *inside* the package carry the trust, collected on-chain before the close. The daemon has **no signing capability of any kind**. Exposed to the router via a Cloudflare Tunnel; healthy only when both the conductor and its app cell answer.

The app completes the flow itself with a single `migration_init` zome call on its new-DNA conductor — no service can do that (it opens the agent's own chain).

Full design + protocol contracts are maintained in unyt's internal version-migration specs.

## Layout

```
router/         Cloudflare Worker (TS) — wrangler + vitest
notary-daemon/  Rust crate — axum + ham
.github/workflows/  ci.yml (test on develop) + deploy.yml (router → CF on main)
```

## Build / test

- **router/** — `npm ci && npm run typecheck && npm test`. Self-contained, no private deps.
- **notary-daemon/** — `cd notary-daemon && cargo test`. The migration wire types come from the **published `rave_engine`** release (crates.io — no unyt-repo access needed); `ham` is a public git dep ([`unytco/ham`](https://github.com/unytco/ham) — fetched anonymously, no token). The HTTP↔zome mapping tests mock the conductor, so they need no Holochain conductor.
- **Real-conductor round-trip (gated):** `cd notary-daemon && cargo test --test live_roundtrip -- --ignored` against a live conductor with a closed agent — locks the package ⇄ `MigrationInitRequest` serde round-trip the mocks can't prove. Env vars + fixture notes in the test's file header.

## Branching / CI

- Integrate on `develop`; release by merging `develop → main`.
- CI runs `cargo test` (daemon) + `vitest` (router) on push/PR.
- The **router Worker auto-deploys to Cloudflare on push to `main`**. The daemon is CI-tested but ships to HEART droplets via unyt's deployment-automation hub (not auto-deployed).
