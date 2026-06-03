# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- router: `GET /v1/update-check?current_dna_hash=` — forward successor lookup so an app can detect a newer network and get its download link
- router: optional `release_url` field on registry DNA entries (surfaced by `/v1/update-check`)

### Changed

- router: registry now rejects a fork (two DNAs upgrading from the same predecessor) so forward lookup is unambiguous

### Fixed

- notary-daemon: `/v1/notarize` now parses `agent_pubkey` via `AgentPubKeyB64::from_str` instead of serde `Deserialize` — holo_hash's B64 serde does not round-trip its own string form (reads the chars as raw bytes → `BadSize`), so every real router call was rejected as `400`. First compile against the real `rave_engine`/`ham` deps; 8 daemon tests pass (also fixed an invalid hand-typed test fixture).
