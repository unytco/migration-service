# migration-service — Agent Instructions

> **This repo follows the workshop root's patterns — it does not define its own.** Development workflow, process, changelog conventions, and spec/feature-doc discipline live in the workshop: [`CLAUDE.md`](../CLAUDE.md), [`AGENTS.md`](../AGENTS.md), [`documentation/DEVELOPMENT_WORKFLOW.md`](../documentation/DEVELOPMENT_WORKFLOW.md). Below is only what's specific to THIS repo.

## Purpose

`service` — off-chain DNA-migration pipeline. A stateless **router** (Cloudflare Worker) validates a `(from_dna_hash, to_dna_hash)` pair against the registry's `upgrades_from` chain, picks a healthy **notary daemon** serving the from-DNA, calls its `/v1/notarize`, and returns the `{ payload, signature }` verbatim to the app — which then submits a single `migration_init_with_signature` zome call on its own new-DNA conductor. The router holds no signing key; the on-chain validator (`GlobalDefinition.migration.notary_agents`) is the trust authority. Design contract: [`documentation/specs/version-migration/service-migration-service.md`](../documentation/specs/version-migration/service-migration-service.md).

## Stack

- **`router/`** — Cloudflare Worker, TypeScript. `wrangler` + `vitest` + `tsc`. Payload-opaque, no `rave_engine` dependency. Self-contained, no private deps.
- **`notary-daemon/`** — Rust crate, `axum` + [`ham`](../ham/). Git-deps `rave_engine` from the **private** `unytco/unyt` repo (the migration wire types), so a first build needs read access to it (SSH/HTTPS locally; `UNYT_REPO_TOKEN` in CI). No `flake.nix` — uses the ambient `cargo` toolchain.

## Build

```bash
cd router && npm ci
cd notary-daemon && cargo build --release
```

## Format

```bash
cd notary-daemon && cargo fmt && cargo fmt --check
# router has no separate formatter step; tsc is the gate (see Test).
```

## Test

```bash
cd router && npm run typecheck && npm test    # vitest — registry, handlers, failover, dna_hash guard
cd notary-daemon && cargo test                # mocked Conductor — /v1/notarize variants, /healthz, bad_request
```

Both suites mock their downstream (daemon `fetch` / the zome `Conductor`), so no Holochain conductor is needed. The daemon-output ⇄ app-input `{payload, signature}` JSON round-trip is the one thing the mocks can't prove — it is locked by a manual real-conductor smoke post-deploy (workshop `BACKLOG` B26).

## Deploy

- **Router** — auto-deploys to the Cloudflare Worker on push to `main` via `.github/workflows/deploy.yml` (`cloudflare/wrangler-action`). The registry is bundled (`router/registry.json`); editing it + redeploying adds a DNA version or notary. Runtime secrets (`MIGRATION_NOTARY_BEARER_TOKEN`, CF Access service token) are set once via `wrangler secret put`.
- **Notary daemon** — CI-tested here, but ships to HEART conductor droplets through the workshop [`automation/`](../automation/) hub (binary + systemd + a Cloudflare Tunnel), not auto-deployed. Build-out is tracked in [`documentation/specs/version-migration/plans/07-release-registry-notary-fleet.md`](../documentation/specs/version-migration/plans/07-release-registry-notary-fleet.md).

## Repo-specific rules

- **Branching:** integrate on `develop`; release by merging `develop → main` (the merge triggers the router deploy). CI (`ci.yml`) runs on push/PR to both.
- **The router is payload-opaque — keep it that way.** It forwards `{ payload, signature }` verbatim and never deserializes them, so it carries no `rave_engine` dep and is unaffected by wire-type version changes. The only field it may read out of `payload` is `dna_hash`, for the `from_dna_hash` sanity guard.
- **`registry.json` ships a placeholder** (`uhC0kREPLACE_WITH_v0_1_DNA_HASH`); the Worker logs an "un-provisioned" line at startup until it's populated. Real provisioning is plan 07 — don't hard-code live DNA hashes here outside that.
- **Error envelope is a shared contract.** Both services emit `{ "error": { "code", "message", "details"? } }`; `code` is a fixed enum (see the service doc). `bad_request` (4xx, client-side) vs `internal` (5xx, our fault) vs `unable_to_verify` / `all_orgs_unhealthy` (transient, drives router failover) are load-bearing distinctions — don't collapse them.
- **Changelog:** record `code`/envelope changes and the daemon's pinned `rave_engine` rev under the workshop changelog conventions; the wire-type pin is reproducibility-critical.
