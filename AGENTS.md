# migration-service — Agent Instructions

> **This repo follows the workshop root's patterns — it does not define its own.** Development workflow, process, changelog conventions, and spec/feature-doc discipline live in the workshop: [`CLAUDE.md`](../CLAUDE.md), [`AGENTS.md`](../AGENTS.md), [`documentation/DEVELOPMENT_WORKFLOW.md`](../documentation/DEVELOPMENT_WORKFLOW.md). Below is only what's specific to THIS repo.

## Purpose

`service` — off-chain DNA-migration pipeline. A stateless **router** (Cloudflare Worker) validates a `(from_dna_hash, to_dna_hash)` pair against the registry's `upgrades_from` chain, picks a notary daemon serving the from-DNA (per-request random order, transient failover), calls its `/{api}/fetch-close`, and returns the closing-summary package `{ payload, notary_signatures, close_action }` verbatim to the app — which then submits a single `migration_init` zome call on its own new-DNA conductor. Neither service signs anything: the close already carries its collected M-of-N notary signatures, and the on-chain validators (`GlobalDefinition.migration` closing/opening pairs) are the trust authority. Design contracts: [`migration-router.md`](../documentation/specs/version-migration/migration-router.md) + [`notary-daemon.md`](../documentation/specs/version-migration/notary-daemon.md).

## Stack

- **`router/`** — Cloudflare Worker, TypeScript. `wrangler` + `vitest` + `tsc`. Payload-opaque, no `rave_engine` dependency. Self-contained, no private deps.
- **`notary-daemon/`** — Rust crate, `axum` + [`ham`](../ham/). Depends on the **published** `rave_engine` release for the migration wire types (no git pins — see the spec's § Versioning). Ships a build-only `flake.nix` providing the musl cross-toolchain for the static deploy binary (`automation/setup-migration-notary.sh` builds the daemon inside it); CI's `cargo build` / `cargo test` use the ambient toolchain.

## Build

```bash
cd router && npm ci
cd notary-daemon && cargo build --release
# Static deploy binary (what automation/ ships to the non-Nix notary droplets):
nix develop -c bash -c 'cd notary-daemon && cargo build --release --target x86_64-unknown-linux-musl'
```

## Format

```bash
cd notary-daemon && cargo fmt && cargo fmt --check
# router has no separate formatter step; tsc is the gate (see Test).
```

## Test

```bash
cd router && npm run typecheck && npm test    # vitest — registry, handlers, shuffled failover, dna_hash guard
cd notary-daemon && cargo test                # mocked Conductor — /v1/fetch-close variants, two-check /healthz, bad_request
```

Both suites mock their downstream (daemon `fetch` / the zome `Conductor`), so no Holochain conductor is needed. The daemon-output ⇄ app-input package round-trip is the one thing the mocks can't prove — it is locked by the gated integration test (`cd notary-daemon && cargo test --test live_roundtrip -- --ignored` against a live conductor with a closed agent; exact env vars in the file header).

## Deploy

- **Router** — auto-deploys to the Cloudflare Worker on push to `main` via `.github/workflows/deploy.yml` (`cloudflare/wrangler-action`). The registry is bundled (`router/registry.json`); editing it + redeploying adds a DNA version or notary. Runtime secrets (`MIGRATION_NOTARY_BEARER_TOKEN`, CF Access service token) are set once via `wrangler secret put`.
- **Notary daemon** — CI-tested here, but ships to HEART conductor droplets through the workshop [`automation/`](../automation/) hub (binary + systemd + a Cloudflare Tunnel), not auto-deployed. The host shape lives in [`documentation/specs/version-migration/notary-host.md`](../documentation/specs/version-migration/notary-host.md).

## Repo-specific rules

- **Branching:** integrate on `develop`; release by merging `develop → main` (the merge triggers the router deploy). CI (`ci.yml`) runs on push/PR to both.
- **The router is payload-opaque — keep it that way.** It forwards the package (`payload` + `notary_signatures` + `close_action`) verbatim and never deserializes it, so it carries no `rave_engine` dep and is unaffected by wire-type version changes. The only field it may read out of `payload` is `dna_hash`, for the `from_dna_hash` sanity guard.
- **`registry.json` ships a placeholder** (`uhC0kREPLACE_WITH_v0_1_DNA_HASH`); the Worker logs an "un-provisioned" line at startup until it's populated. Real provisioning is release-time work — don't hard-code live DNA hashes here outside it. Every notary entry pins the daemon HTTP API it speaks (`"api": "v1"`).
- **Error envelope is a shared contract.** Both services emit `{ "error": { "code", "message", "details"? } }`; `code` is a fixed enum (see the service doc). `bad_request` (4xx, client-side) vs `internal` (5xx, our fault) vs `unable_to_verify` / `all_orgs_unhealthy` (transient, drives router failover) are load-bearing distinctions — don't collapse them.
- **Changelog:** record `code`/envelope changes and the daemon's pinned `rave_engine` rev under the workshop changelog conventions; the wire-type pin is reproducibility-critical.
