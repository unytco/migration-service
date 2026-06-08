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

Design + protocol contract: see
`workshop/documentation/specs/dna-migration/service-migration-service.md`.

## Layout

```
router/         Cloudflare Worker (TS) — wrangler + vitest
notary-daemon/  Rust crate — axum + ham
.github/workflows/  ci.yml (test on develop) + deploy.yml (router → CF on main)
```

## Build status

- **router/** — builds and tests green: `npm ci && npm run typecheck && npm test`
  (36 tests passing). Self-contained, no private deps.
- **notary-daemon/** — **builds and tests green** against the real
  `rave_engine`/`ham` deps: `cd notary-daemon && cargo test` (8 `tests/notarize.rs`
  cases). Needs read access to the private `unytco/unyt` repo (see local setup
  below). The HTTP↔zome mapping tests mock the conductor, so they need no
  Holochain conductor — only the deps must resolve.

### Local build setup (notary-daemon)

The daemon git-deps `rave_engine` from the **private** `unytco/unyt` repo on the
`feat/migration-upgrade` branch (switch the dep to `develop` once it merges).
To build locally with an SSH key that has read access:

```bash
# Make cargo fetch git deps via the system git, and route unytco GitHub deps over SSH.
printf '\n[net]\ngit-fetch-with-cli = true\n' >> ~/.cargo/config.toml
git config --global url."git@github.com:unytco/".insteadOf "https://github.com/unytco/"
cd notary-daemon && cargo test   # 8 tests pass
```

CI uses the read-only `UNYT_REPO_TOKEN` secret instead — `ci.yml` wires it via
`git config insteadOf` + `CARGO_NET_GIT_FETCH_WITH_CLI`.

**Still to verify — manual smoke (real conductor):** lock the `payload`/`signature`
JSON round-trip (daemon output ⇄ app `MigrationInitRequest` decode) — the one
thing the mocked tests can't prove. See the service doc § "Wire format".

> If the private-repo dep becomes a friction point, the alternative is to mirror
> the v0_1 wire types locally (as `pricing_oracle` does) or publish a small
> public types crate. Git-dep was chosen for payload-encoding fidelity.

## Branching / CI

- Integrate on `develop`; release by merging `develop → main`.
- CI runs `cargo test` (daemon) + `vitest` (router) on push/PR.
- The **router Worker auto-deploys to Cloudflare on push to `main`**. The daemon
  is CI-tested but ships to HEART droplets via the workshop `automation/` hub
  (not auto-deployed).
