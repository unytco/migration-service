# migration-service

Off-chain pipeline for unyt DNA migration. Lets an app fetch its committed closing summary over HTTP — instead of hand-driving zome calls against someone else's conductor — and open on the successor DNA with it.

Two components:

- **`migration-router/`** — a Cloudflare Worker (TypeScript). Public HTTP entry point. Reads a bundled registry (version chain + per-DNA notary endpoints, each pinned to a daemon API version), validates the `(from_dna_hash, to_dna_hash)` pair against the `upgrades_from` chain, tries the from-DNA's notary daemons in per-request random order (transient failures fail over; hard stops return immediately), calls the chosen daemon's `/{api}/fetch-close`, and returns the closing-summary package `{ payload, notary_signatures, close_action }` verbatim to the app. Holds no keys and never interprets the payload.

- **`notary-daemon/`** — a Rust `axum` + [`ham`](https://github.com/unytco/ham) service. Runs co-located with a Holochain conductor that keeps serving the old (from-DNA) network. Its `/v1/fetch-close` calls the alliance `read_predecessor_close` zome fn — a pure read of the agent's committed close; the M-of-N notary signatures *inside* the package carry the trust, collected on-chain before the close. The daemon has **no signing capability of any kind**. Exposed to the router via a Cloudflare Tunnel; healthy only when both the conductor and its app cell answer.

- **`headless-migrator/`** — a Rust `clap` + [`ham`](https://github.com/unytco/ham) binary, the headless counterpart of the app's migration ceremony for the **stateful server agents** the fleet provisions (bridge orchestrator, hf-swapper). Two supervised systemd services: a **close service** on the old server (collect M-of-N notary signatures → close the chain) and an **open service** on the new server (wait out gossip for the package → fresh membrane proof for the carried key → install with the package as the alliance role's `init_properties` so the DNA's `init` opens the chain → verify), plus `status` and `verify` one-shots. Each is probe-first and idempotent and exits 0 only on success, so `Restart=on-failure` drives the loop. It operates on an already-carried agent key (the lair-version-aware key carry across droplets is `automation/`'s job).

The app (and the headless-migrator's open service) completes the flow by installing the new-DNA app with the package as the alliance role's `init_properties`, so the DNA's `init` opens the agent's own chain at genesis — no off-chain service can do that.

Full design + protocol contracts are maintained in unyt's internal version-migration specs.

## Layout

```text
migration-router/ Cloudflare Worker (TS) — wrangler + vitest
notary-daemon/   Rust crate — axum + ham
headless-migrator/ Rust crate — clap + ham (headless server-agent close/open services)
.github/workflows/  ci.yml (test on push/PR to develop + main) + deploy.yml (router → CF on main)
                    + release.yml (binaries → GitHub release on tag)
```

## Build / test

- **migration-router/** — `npm ci && npm run typecheck && npm test`. Self-contained, no private deps.
- **notary-daemon/** — `cd notary-daemon && cargo test`. The migration wire types come from the **published `rave_engine`** release (crates.io — no unyt-repo access needed); `ham` is a public git dep ([`unytco/ham`](https://github.com/unytco/ham) — fetched anonymously, no token). The HTTP↔zome mapping tests mock the conductor, so they need no Holochain conductor.
- **headless-migrator/** — `cd headless-migrator && cargo test`. Same public deps as the daemon. The M-of-N policy, the probe→next-step state machine (incl. partial close), close/open idempotency, and the verify comparison are all tested against a scripted mock conductor — no Holochain conductor needed.
- **Real-conductor round-trips (gated):** `cd notary-daemon && cargo test --test live_roundtrip -- --ignored` (a live conductor with a closed agent — locks the package ⇄ `MigrationInitRequest` serde round-trip) and `cd headless-migrator && cargo test --test live_roundtrip -- --ignored` (live old+new conductors + a `wrangler dev` router — the full close → carry → open → verify arc + restart drills). Env vars + fixture notes in each test's file header.

## Releases

Cut a release by pushing a semver tag:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

Both binaries ship from one tag, so the workflow checks it against the version in **both** crates' `Cargo.toml` — bump and commit both before tagging. Tags run the workflow as it exists in the tagged commit, so merge first.

Assets have **fixed names**, so a provisioning script can hardcode the URL:

| Asset | Description |
|---|---|
| `headless-migrator` | Stripped release binary — the close/open services |
| `migration-notary` | Stripped release binary — the notary daemon |
| `<name>.sha256` | Digest of each, bare filename inside |

```text
https://github.com/unytco/migration-service/releases/download/v0.1.0/migration-notary
https://github.com/unytco/migration-service/releases/latest/download/migration-notary
```

`releases/latest/download/` resolves to the newest non-prerelease, so a droplet pointed at `latest` will not pick up an `-rc` tag.

### Installing from cloud-init

```bash
VERSION=v0.1.0
BIN=migration-notary          # or headless-migrator
INSTALL_DIR=/opt/migration-service

mkdir -p "$INSTALL_DIR"
cd "$INSTALL_DIR"

for asset in "$BIN" "$BIN.sha256"; do
  curl -fsSL -o "$asset" \
    "https://github.com/unytco/migration-service/releases/download/${VERSION}/${asset}"
done

sha256sum -c "$BIN.sha256"
chmod 755 "$BIN"
```

The binaries are built on the same Ubuntu release the fleet droplets run, so a target needs no toolchain and no extra packages: they link nothing beyond glibc, and their TLS roots are compiled in. Neither is configured by file — the daemon reads its config from the environment, `headless-migrator` from flags plus the environment.

The asset names carry no version, so `--version` on either binary is how a deployed build identifies itself.

## Branching / CI

- Integrate on `develop`; release by merging `develop → main`.
- CI runs `cargo test` (daemon + headless-migrator) + `vitest` (router) on push/PR.
- The **router Worker auto-deploys to Cloudflare on push to `main`**. The daemon and headless-migrator are CI-tested and published as release assets on a tag (see [Releases](#releases)); they reach HEART droplets from there via unyt's deployment-automation hub (not auto-deployed).
