# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **Tag-driven release workflow.** Pushing a semver tag builds both Rust binaries and publishes them — each with a `.sha256` — as fixed-name GitHub release assets. The tag must match the version in both crates' `Cargo.toml`; they release together, so one tag is one tested pairing of the close-side and open-side halves of a migration.
- `--version` on `headless-migrator` and `migration-notary`, derived from the crate version. The release assets have fixed filenames, so this is how a deployed binary identifies itself.
- **router: per-entry `published` visibility gate — customers-last migration surfacing.** Registry `DnaEntry` gains an optional `published` boolean (absent = unpublished): honored by `/v1/update-check`, ignored by `/v1/migrate` + `/v1/migration-options`. Additive + backward-compatible.
- **router: local-testnet mode (local-testnet task 02).** A local entry point (`src/index.local.ts`, `npm run dev:local`) loads a gitignored `registry.local.json` admitting `http://` notaries and an optional `GITHUB_RELEASES_URL`; the deployed entry point stays strict https-only.
- **router: `latest_build.assets` — the release's downloadable installers, for the in-app updater (release-patterns task 07).** `/v1/update-check`'s `latest_build` carries each asset's `name` + `url` + optional `digest`; the router stays platform-agnostic. Additive + backward-compatible.
- headless-migrator: the verify step cross-checks the committed agreement state (via the DNA's `get_opened_agreement_state`) against the fetched close package.
- **headless-migrator: new Rust crate — the headless server-agent migration driver.** A `clap` + `ham` binary with four modes (`status`, `close-service`, `open-service`, `verify`), each probe-first and idempotent under systemd `Restart=on-failure`. Operates on an already-carried agent key.
- headless-migrator: wired into the repo `flake.nix`/musl toolchain and CI (`ci.yml` `agent` job: fmt + clippy + test, mirroring `notary-daemon`).
- notary-daemon: build-only `flake.nix` providing the musl cross-toolchain for the static deploy binary.
- router: `GET /v1/update-check?current_dna_hash=` — forward successor lookup so an app can detect a newer network and get its download link
- router: optional `release_url` field on registry DNA entries (surfaced by `/v1/update-check`)
- **router: two-channel `/v1/update-check` — a `latest_build` answer beside the migration answer.** An optional `app_version` resolves the newest published GitHub release on the caller's lineage, so a UI-only release is detectable. Additive + backward-compatible.
- notary-daemon: gated live round-trip test (`tests/live_roundtrip.rs`, `cargo test --test live_roundtrip -- --ignored`) — the real daemon + `ham` against a live conductor with a closed agent.
- **Skip-version migration routing + single-landing close target (M14).** Registry entries carry `upgrade_targets`; reachability is computed over that forward graph across `/v1/update-check`, `/v1/migration-options` and `/v1/migrate`. The headless close binds to one configured successor (`MIGRATION_AGENT_TO_DNA`, required).

### Changed

- **Operators:** a droplet can be provisioned straight from a release — `https://github.com/unytco/migration-service/releases/latest/download/migration-notary` — with no repo checkout and no build on the operator's host. `latest` resolves to the newest non-prerelease, so an `-rc` tag is not picked up by a droplet pointed at it.
- `[profile.release]` sets `strip = "symbols"` in both crates, so a release build produces the same binary whether it comes from CI or an operator's host.
- **Operators:** stripped binaries no longer carry function names in panic backtraces. Ordinary failures are unaffected — errors still print their full `anyhow` context chain.
- **headless-migrator: a debt on any unit blocks a close, not just the base one.** What an agent owes is now stated per unit (`rave_engine` 0.10.0), so the check before closing a chain reads every unit. `notary-daemon` stays on 0.9.0; the migration wire types are unchanged between the two.
- headless-migrator: a successor definition that is not yet inside its effective window waits under the bounded deadline and is named as its own cause, rather than being confused with one that has not gossiped in.
- **Upgrade to Holochain 0.7 + `rave_engine` 0.9.0** (`headless-migrator` + `notary-daemon`): exact pins `holochain_client =0.9.0`, `holo_hash` / `holochain_types` `=0.7.0`, `hdi =0.8.0`, `zfuel 0.9.0`, `ham` on branch `main`; holonix moves `main-0.6` → `main-0.7`. CI now also fires on `develop-0.7`.
- **Operational consequence — close and open need binaries from different branches for the 0.6→0.7 hop:** the close is built from `develop` (0.6 conductor), the open from `develop-0.7`. Config-level in `automation` (`.migrate.migration_service_repo`); no deploy change here.
- **router: client-fault responses now use `bad_request`, not `internal`** — malformed-JSON `POST /v1/migrate` and unmatched routes return `400` / `404 bad_request`; statuses + messages unchanged.
- **CI/deploy: every GitHub Actions `uses:` is pinned to an immutable commit SHA with `persist-credentials: false`, and CI now runs on pull requests to `main` as well as `develop`.** Router `compatibility_date` bumped `2024-09-23 → 2024-12-30`.
- **CI/deploy: the deploy job's npm cache is dropped and CI runs are now per-branch-concurrent.** `deploy.yml` no longer shares a lockfile-keyed `~/.npm` with `ci.yml` (cache poisoning); `ci.yml` gains a `concurrency` group with `cancel-in-progress`.
- **headless-migrator: GD-wait exhaustion now reports a config fault, not a raw genesis error, and the open loop is mock-drivable (local-testnet B91).** A `Connector` factory-trait injection lets a `MockConductor` drive the open loop (`tests/open_service.rs`).
- Adopt published `rave_engine` 0.7.0: headless-migrator drops its local `OpenedAgreementState` wire mirror, and notary-daemon preserves the `AgreementCarryForward` `locked` field when serving a close package.
- headless-migrator: DNA migration errors classify by typed `[MIGERR:<CODE>]` code first, English substring only as a fallback — a validator reword no longer silently reclassifies an error.
- **Upgrade to Holochain 0.6.2-rc.0 + `rave_engine` 0.6.0** (`headless-migrator` + `notary-daemon`) — the workshop-wide 0.6.2-rc.0 bump; this repo was the gap in the task-17 bump.
- **headless-migrator: install the successor with the package as `init_properties`, retiring the `migration_init` zome call (HC-795).** The DNA's `init` opens the chain at genesis; a too-early install retries under a bounded `MIGRATION_AGENT_GD_WAIT_TIMEOUT_SECS` (default 30 min), and `status` makes no zome call.
- **Breaking — `rave_engine` pinned to the published `0.6.0`** — the skip-migration shape: `MigrationConfig.upgrade_targets` + `opening_predecessors` (was `opening_notaries` / `opening_threshold`), and `SummaryStatePayload`'s `source_dna_hash` / `target_dna_hash` split.
- **Breaking — router: the `/v1/migrate` unregistered-pair reject is now `unreachable_target`** (was `not_registered_predecessor`) — it fires when the source DNA has no forward path to the requested `to`.
- router: `/v1/migrate` error aggregation hardened — a config/daemon fault surfaces as a 5xx `internal` instead of being masked as a 503 outage, and a bad response from one of a source's daemons falls through to its siblings.
- **Breaking — the daemon is now the fetch-only service of the close-time M-of-N flow; it has no signing capability of any kind.** `POST /v1/notarize` is replaced by `POST /v1/fetch-close`, serving the three-field package `{ payload, notary_signatures, close_action }`; the `too_new` code and the freshness window are gone from the API.
- notary-daemon: `/healthz` is healthy only when both the conductor (`ping`) and the app cell (`whoami`) answer — either failing returns 503 with a distinct message.
- router: `/v1/migrate` forwards the daemon's three-field package verbatim (was `{ payload, signature }`); candidate notaries are tried in per-request random order.
- router: registry notary entries now pin the daemon HTTP API version (`"api": "v1"`); startup validation requires an `https` url and a supported `api` on every notary entry.
- notary-daemon: `rave_engine` pinned to the published release carrying the close-time M-of-N wire types (`ReadCloseResponse`, `NotarySignature`, `chain_top` payload)
- notary-daemon: re-pin `rave_engine` to the current migration shape (`a57bfc81` — agent-bound `SummaryStatePayload` + `SummaryState.agreement_carry_forward`).
- router: registry now rejects a fork (two DNAs upgrading from the same predecessor) so forward lookup is unambiguous

### Fixed

- **headless-migrator: a reply it cannot read stops the run instead of retrying forever, and no longer blames the notaries.** Two versions disagreeing about a message shape is never resolved by waiting. Asking a notary to sign treated every failure as that notary being unreachable, so an unreadable reply worked through the whole list and reported `notary list exhausted`, sending an operator to check servers that were fine. A reply to a write still retries: it says nothing about whether the write landed.

- **router: `/v1/migrate` is now fully fail-closed on the served close's `source_dna_hash`** — the guard rejects (`500 internal`) whenever the normalized source ≠ the queried DNA, including `undefined`, and `normalizeDnaHashB64` accepts only a 39-byte HoloHash array.
- **headless-migrator: the migrating install now applies the network's DNA properties — the cell lands on the network's DNA instead of an isolated one.** Carried from the joining service's `dna_modifiers.properties` as order-preserving `YamlProperties`; the open now hard-stops on a cell that isn't on the `to_dna` or the carried key.
- **headless-migrator: a rejected membrane proof is now terminal, not an infinite retry** — `genesis_self_check`'s four verdicts join the init hard-failure table instead of falling through to the unbounded transient arm.
- **router: `/v1/migrate` now normalizes a byte-array `target_dna_hash` (local-testnet D7)** — a DNA hash arriving as a raw HoloHash byte array is accepted alongside base64, for both `target_dna_hash` and `source_dna_hash`.
- notary-daemon: add a wire round-trip smoke locking the daemon `Verified` envelope ⇄ app `MigrationInitRequest` encoding; test fixtures updated to the agent-bound payload shape.
- notary-daemon: `/v1/notarize` parses `agent_pubkey` via `AgentPubKeyB64::from_str` instead of serde `Deserialize` — holo_hash's B64 serde does not round-trip its own string form, so every real router call was rejected as `400`.
