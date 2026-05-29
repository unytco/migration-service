# migration-service

Off-chain pipeline for unyt DNA migration. Lets an app trigger a DNA migration
over HTTP instead of hand-driving zome calls.

Two components:

- **`router/`** — a Cloudflare Worker (TypeScript). Public HTTP entry point. Reads
  a bundled registry (version chain + per-DNA notary endpoints), validates the
  `(from_dna_hash, to_dna_hash)` pair against the `upgrades_from` chain, picks a
  healthy notary daemon serving the from-DNA, calls its `/v1/notarize`, and
  returns `{ payload, signature }` verbatim to the app. Holds no signing key and
  never interprets the payload.

- **`notary-daemon/`** — a Rust `axum` + [`ham`](https://github.com/unytco/ham)
  service. Runs co-located with a Holochain conductor (reaches it over
  `ws://localhost:8800`). Its `/v1/notarize` calls the alliance
  `notary_read_predecessor_close` zome fn (read + validate + sign the close state
  on the old DNA) and maps the result to HTTP. Exposed to the router via a
  Cloudflare Tunnel.

The app completes the flow itself with a single `migration_init_with_signature`
zome call on its new-DNA conductor — no daemon can do that (it opens the agent's
own chain).

Design + protocol contract: see the unyt repo's
`docs/architecture/features/dna-migration/service-migration-service.md`.

## Layout

```
router/         Cloudflare Worker (TS) — wrangler + vitest
notary-daemon/  Rust crate — axum + ham
.github/workflows/  ci.yml (test on develop) + deploy.yml (router → CF on main)
```

## Build status

- **router/** — builds and tests green: `npm ci && npm run typecheck && npm test`
  (21 tests passing). Self-contained, no private deps.
- **notary-daemon/** — written; **not yet compiled against the real deps** because
  it git-deps `rave_engine` from the **private** `unytco/unyt` repo. First build
  needs the preconditions below. The HTTP↔zome mapping tests mock the conductor,
  so they need no Holochain — only the deps must resolve.

### First-build checklist (notary-daemon)

1. **Push the `rave_engine` branch** the dep points at (`feat/migration-upgrade`
   today; switch the dep to `develop` once it merges). The migration wire types
   (`NotaryReadRequest`/`NotaryReadResponse`/`SummaryStatePayload`) live there.
2. **Provide read access to the private `unytco/unyt` repo:**
   - Local: SSH or an HTTPS credential helper for github.com.
   - CI: set repo secret `UNYT_REPO_TOKEN` (read-only) — `ci.yml` already wires
     it via `git config insteadOf` + `CARGO_NET_GIT_FETCH_WITH_CLI`.
3. `cd notary-daemon && cargo test` — expect the 8 `tests/notarize.rs` cases to
   pass. On first build, confirm the `hdi::prelude::{Signature, Timestamp}` and
   `holo_hash` imports resolve against the pinned `hdi 0.7.1` / `holo_hash 0.6.1`
   (version-aligned with the unyt workspace).
4. **Manual smoke (real conductor):** lock the `payload`/`signature` JSON
   round-trip (daemon output ⇄ app `MigrationInitRequest` decode) — the one thing
   the mocked tests can't prove. See the service doc § "Wire format".

> If the private-repo dep becomes a friction point, the alternative is to mirror
> the v0_1 wire types locally (as `pricing_oracle` does) or publish a small
> public types crate. Git-dep was chosen for payload-encoding fidelity.

## Branching / CI

- Integrate on `develop`; release by merging `develop → main`.
- CI runs `cargo test` (daemon) + `vitest` (router) on push/PR.
- The **router Worker auto-deploys to Cloudflare on push to `main`**. The daemon
  is CI-tested but ships to HEART droplets via the workshop `automation/` hub
  (not auto-deployed).
