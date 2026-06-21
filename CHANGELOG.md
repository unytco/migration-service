# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **migration-agent: new Rust crate — the headless server-agent migration driver.** A standalone `clap` + `ham` binary (sibling of `notary-daemon`, same published-`rave_engine` + `ham` pins) with four modes: `status` (probe report: old chain closed · package fetchable · new chain opened), `close-service` (supervised: drop fees if owed → `prepare_closing_summary` → collect M-of-N via the parameterized policy → `close_agent_chain`), `open-service` (supervised: wait out gossip for the package → fresh membrane proof for the carried key from the target joining service → `install_app` → `migration_init` first → verify), and `verify` (new-chain ledger vs the close summary, per-field). Each supervised service is probe-first + idempotent and exits 0 only on success (systemd `Restart=on-failure` drives the loop; no overall deadline). Operates on an already-carried agent key. Unit-tested against a mocked conductor seam (M-of-N policy with seeded RNG, probe→next-step incl. partial close, close/open no-op idempotency, `no_close_found`-keeps-waiting, non-fresh-chain → uninstall/reinstall, verify mismatch reporting); the full close → carry → open → verify arc + restart drills are a gated `tests/live_roundtrip.rs` (release-time). Writes a machine-readable state file for the `automation/` report collector; logs to journald.
- migration-agent: wired into the repo `flake.nix`/musl toolchain and CI (`ci.yml` `agent` job: fmt + clippy + test, mirroring `notary-daemon`).
- notary-daemon: build-only `flake.nix` providing the musl cross-toolchain for the static deploy binary.
- router: `GET /v1/update-check?current_dna_hash=` — forward successor lookup so an app can detect a newer network and get its download link
- router: optional `release_url` field on registry DNA entries (surfaced by `/v1/update-check`)
- notary-daemon: gated live round-trip test (`tests/live_roundtrip.rs`, `cargo test --test live_roundtrip -- --ignored`) — the real daemon + real `ham` against a live conductor with a closed agent; locks the served package decoding with the same `rave_engine` types the app consumes (run command in the file header)

### Changed

- **Breaking — the daemon is now the fetch-only service of the close-time M-of-N flow; it has no signing capability of any kind.** `POST /v1/notarize` (read + validate + sign) is replaced by `POST /v1/fetch-close`, which serves the agent's committed closing summary — the three-field package `{ payload, notary_signatures, close_action }` — via the read-only `read_predecessor_close` extern, so the migration-service can hand it to that agent for `migration_init` on the successor DNA. The `too_new` error code and the freshness window are gone from the API; the close's own collected signatures carry the trust
- notary-daemon: `/healthz` is healthy only when BOTH the conductor answers (`ping`) and the app cell answers (a `whoami` zome call) — either failing returns 503 with a distinct message (`conductor unreachable` vs `app cell unresponsive`)
- router: `/v1/migrate` forwards the daemon's three-field package verbatim (was `{ payload, signature }`); candidate notaries are tried in per-request RANDOM order (stateless load-spreading; injectable `rand` keeps tests deterministic); transient failures still fail over, hard stops still return immediately
- router: registry notary entries now pin the daemon HTTP API version (`"api": "v1"`); startup validation requires an `https` url and a supported `api` on every notary entry — a deficient entry fails at startup, never at request time
- notary-daemon: `rave_engine` pinned to the published release carrying the close-time M-of-N wire types (`ReadCloseResponse`, `NotarySignature`, `chain_top` payload)

- notary-daemon: re-pin `rave_engine` to the current migration shape (`a57bfc81` — agent-bound `SummaryStatePayload` + `SummaryState.agreement_carry_forward`); the prior lock (`221af08`) predated both, so the daemon signed/decoded a stale payload shape the deployed DNA validator would reject
- router: registry now rejects a fork (two DNAs upgrading from the same predecessor) so forward lookup is unambiguous

### Fixed

- notary-daemon: add a wire round-trip smoke (`verified_envelope_round_trips_into_migration_init_request`) locking the daemon `Verified` envelope ⇄ app `MigrationInitRequest` encoding, and update the test fixtures to the agent-bound payload shape; 9 daemon tests pass against the re-pinned deps
- notary-daemon: `/v1/notarize` now parses `agent_pubkey` via `AgentPubKeyB64::from_str` instead of serde `Deserialize` — holo_hash's B64 serde does not round-trip its own string form (reads the chars as raw bytes → `BadSize`), so every real router call was rejected as `400`. First compile against the real `rave_engine`/`ham` deps; 8 daemon tests pass (also fixed an invalid hand-typed test fixture).
