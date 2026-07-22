# migration-service — Agent Instructions

> **Conventions.** This repo's development-workflow, changelog, and spec/feature-doc conventions follow unyt's internal toolkit; below is only what's specific to THIS repo.

## Purpose

`service` — off-chain DNA-migration pipeline. A stateless **router** (Cloudflare Worker) validates a `(from_dna_hash, to_dna_hash)` pair against the registry's `upgrades_from` chain, picks a notary daemon serving the from-DNA (per-request random order, transient failover), calls its `/{api}/fetch-close`, and returns the closing-summary package `{ payload, notary_signatures, close_action }` verbatim to the app — which then installs its own new-DNA app with that package as the alliance role's `init_properties`, so the DNA's `init` opens the chain. Neither service signs anything: the close already carries its collected M-of-N notary signatures, and the on-chain validators (`GlobalDefinition.migration` closing/opening pairs) are the trust authority. A third component, the **headless-migrator**, is the headless equivalent of the UI ceremony for the stateful server agents the fleet provisions: supervised close/open services that carry a server agent's economic identity across a DNA upgrade. Full design + protocol contracts are maintained in unyt's internal version-migration specs (`migration-router.md`, `notary-daemon.md`, `server-agent-migration.md`).

## Stack

- **`migration-router/`** — Cloudflare Worker, TypeScript. `wrangler` + `vitest` + `tsc`. Payload-opaque, no `rave_engine` dependency. Self-contained, no private deps.
- **`notary-daemon/`** — Rust crate, `axum` + [`ham`](https://github.com/unytco/ham). Depends on the **published** `rave_engine` release for the migration wire types (no git pins — see the spec's § Versioning). Ships a build-only `flake.nix` providing the musl cross-toolchain for the static deploy binary (`automation/setup-migration-notary.sh` builds the daemon inside it); CI's `cargo build` / `cargo test` use the ambient toolchain.
- **`headless-migrator/`** — Rust crate (standalone sibling of `notary-daemon`, same `ham` + published-`rave_engine` pins), `clap` CLI. Four modes (`status` · `close-service` · `open-service` · `verify`) run as supervised systemd services (`Restart=on-failure`, probe-first + idempotent, exit 0 only on success — no overall deadline). The close service collects M-of-N notary signatures and closes the old chain; the open service is the new server's install step (waits out gossip for the package, requests a fresh membrane proof for the carried key, installs with the package as `init_properties` so the DNA's `init` opens the chain, verifies). Operates on an already-carried agent key — the lair-version-aware key carry across droplets is `automation/`'s `migrate-carry-key.sh`. Shares the repo's musl `flake.nix`/toolchain; the gated `tests/live_roundtrip.rs` is release-time (live conductors + a `wrangler dev` router).

## Build

```bash
cd migration-router && npm ci
cd notary-daemon && cargo build --release
cd headless-migrator && cargo build --release
# Static deploy binaries (what automation/ ships to the non-Nix droplets):
nix develop -c bash -c 'cd notary-daemon  && cargo build --release --target x86_64-unknown-linux-musl'
nix develop -c bash -c 'cd headless-migrator && cargo build --release --target x86_64-unknown-linux-musl'
```

## Format

```bash
cd notary-daemon  && cargo fmt && cargo fmt --check
cd headless-migrator && cargo fmt && cargo fmt --check
# router has no separate formatter step; tsc is the gate (see Test).
```

## Test

```bash
cd migration-router && npm run typecheck && npm test    # vitest — registry, handlers, shuffled failover, dna_hash guard
cd notary-daemon && cargo test                # mocked Conductor — /v1/fetch-close variants, two-check /healthz, bad_request
cd headless-migrator && cargo test              # mocked Conductor — M-of-N policy, probe→next-step (incl. partial close), close/open idempotency, verify
```

All three suites mock their downstream (daemon `fetch` / the zome `Conductor` / a scripted `Conductor` + a one-shot local HTTP responder), so no Holochain conductor is needed. The cross-boundary proofs the mocks can't make are locked by gated integration tests: the daemon's daemon-output ⇄ app-input package round-trip (`cd notary-daemon && cargo test --test live_roundtrip -- --ignored` against a live conductor with a closed agent), and the agent's full close → carry → open → verify arc + restart drills (`cd headless-migrator && cargo test --test live_roundtrip -- --ignored` against live old+new conductors + a `wrangler dev` router — release-time only). Exact env vars are in each test's file header.

> **Clippy in CI only.** `cargo clippy` currently fails to compile the `num_enum` 0.6 proc-macro in some local/offline toolchains (a `clippy-driver` ⇄ `rustc` proc-macro mismatch affecting **every** crate in this repo, `notary-daemon` included — not crate-specific). `cargo check --all-targets` / `cargo build` / `cargo test` are unaffected and are the local gate; rely on CI's clippy run.

## Deploy

- **Router** — auto-deploys to the Cloudflare Worker on push to `main` via `.github/workflows/deploy.yml` (`cloudflare/wrangler-action`). The registry is bundled (`migration-router/registry.json`); editing it + redeploying adds a DNA version or notary. Runtime secrets (`MIGRATION_NOTARY_BEARER_TOKEN`, CF Access service token) are set once via `wrangler secret put`.
- **Notary daemon** — CI-tested here, but ships to HEART conductor droplets through unyt's deployment-automation hub (binary + systemd + a Cloudflare Tunnel), not auto-deployed. The host shape lives in unyt's internal `notary-host.md` spec.

## Repo-specific rules

- **Branching:** integrate on `develop`; release by merging `develop → main` (the merge triggers the router deploy). CI (`ci.yml`) runs on push/PR to both.
- **The router is payload-opaque — keep it that way.** It forwards the package (`payload` + `notary_signatures` + `close_action`) verbatim and never deserializes it, so it carries no `rave_engine` dep and is unaffected by wire-type version changes. The only fields it may read out of `payload` are `source_dna_hash` (the wrong-cell sanity guard — the served close must match the source it was fetched from) and `target_dna_hash` (the single-landing target filter for skip routing) — both treated as opaque b64 strings, never deserialized.
- **`registry.json` ships a placeholder** (`uhC0kREPLACE_WITH_v0_1_DNA_HASH`); the Worker logs an "un-provisioned" line at startup until it's populated. Real provisioning is release-time work — don't hard-code live DNA hashes here outside it. Every notary entry pins the daemon HTTP API it speaks (`"api": "v1"`).
- **Error envelope is a shared contract.** Both services emit `{ "error": { "code", "message", "details"? } }`; `code` is a fixed enum (see the service doc). `bad_request` (4xx, client-side) vs `internal` (5xx, our fault) vs `unable_to_verify` / `all_orgs_unhealthy` (transient, drives router failover) are load-bearing distinctions — don't collapse them.
- **Changelog:** record `code`/envelope changes and the daemon's pinned `rave_engine` rev under unyt's changelog conventions; the wire-type pin is reproducibility-critical.
