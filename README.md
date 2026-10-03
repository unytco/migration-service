# migration-service

Off-chain pipeline for unyt DNA migration. Once an agent's chain has closed, it gathers M notaries' attestations of that close over HTTP, so the agent can open on the successor DNA with them without hand-driving zome calls against someone else's conductor.

Two components:

- **`migration-router/`**: a Cloudflare Worker (TypeScript) and the public HTTP entry point. It reads a bundled registry (the version chain, and for each DNA its notary daemons, each pinned to a daemon API version, and its `closing_threshold` M), validates the `(from_dna_hash, to_dna_hash)` pair, asks M of the source's daemons at once for `/{api}/attest-close`, asks the next daemon in a per-request random order for each answer that does not count, and returns `{ payload, notary_signatures, close_action }` with M signatures over one close, in the daemons' own JSON text. It holds no keys.

- **`notary-daemon/`**: a Rust `axum` + [`ham`](https://github.com/unytco/ham) service, run beside a Holochain conductor whose cell is a closing notary on the old (from-DNA) network. Its `/v2/attest-close` calls the alliance `notary_attest_close` zome fn, which reads the agent's closed chain and signs the close, and serves the package carrying that one signature; the router combines M of them. Attesting commits nothing, and the daemon signs its own zome calls through the node's lair, so connecting commits nothing either. Exposed to the router via a Cloudflare Tunnel; healthy only when both the conductor and its app cell answer.

- **`headless-migrator/`**: a Rust `clap` + [`ham`](https://github.com/unytco/ham) binary, the headless counterpart of the app's migration ceremony for the **stateful server agents** the fleet provisions (bridge orchestrator, hf-swapper). Two supervised systemd services: a **close service** on the old server (M notaries check the payload → close the chain) and an **open service** on the new server (wait out gossip for the package → fresh membrane proof for the carried key → install with the package as the alliance role's `init_properties` so the DNA's `init` opens the chain → verify), plus `status` and `verify` one-shots. Each is probe-first and idempotent and exits 0 only on success, so `Restart=on-failure` drives the loop. It operates on an already-carried agent key (the lair-version-aware key carry across droplets is `automation/`'s job).

The app (and the headless-migrator's open service) completes the flow by installing the new-DNA app with the package as the alliance role's `init_properties`, so the DNA's `init` opens the agent's own chain at genesis — no off-chain service can do that.

The contract is [`documentation/specs/version-migration/`](https://github.com/unytco/workshop/tree/main/documentation/specs/version-migration) in `unytco/workshop`.

## Layout

```text
migration-router/ Cloudflare Worker (TS) — wrangler + vitest
notary-daemon/   Rust crate — axum + ham
headless-migrator/ Rust crate — clap + ham (headless server-agent close/open services)
.github/workflows/  ci.yml (test on push/PR to develop + main) + deploy.yml (router → CF on main)
                    + release.yml (binaries → GitHub release on tag)
```

## Build / test

- **migration-router/**: `npm ci && npm run typecheck && npm test`. Self-contained, no private deps.
- **notary-daemon/**: `cd notary-daemon && cargo test`. The migration wire types come from the published `rave_engine` release on crates.io, and `ham` is a public git dep ([`unytco/ham`](https://github.com/unytco/ham)), so fetching needs no credentials. The HTTP↔zome mapping tests mock the conductor, so they need no Holochain conductor.
- **headless-migrator/**: `cd headless-migrator && cargo test`. Same deps as the daemon. The M-of-N policy, the close-state probe (incl. a summary with no close), close/open idempotency, and the verify comparison are all tested against a scripted mock conductor, so they need no Holochain conductor.
- **Real-conductor round-trips (gated):** `cd notary-daemon && cargo test --test live_roundtrip -- --ignored` (a live notary conductor and an agent whose chain has closed: locks the package ⇄ `MigrationInitRequest` serde round-trip) and `cd headless-migrator && cargo test --test live_roundtrip -- --ignored` (live old+new conductors + a `wrangler dev` router: the full close → carry → open → verify arc + restart drills). Env vars + fixture notes in each test's file header.

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
