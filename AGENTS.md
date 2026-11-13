# AGENTS.md

Guidance for coding agents working on mypaas-rs. Read this first, then the
design document.

## What this is

mypaas-rs is a single-server PaaS on Docker and Traefik. A single Rust binary
(`mypaas`) does three jobs:

- `mypaas …` is the client CLI, run on a laptop or in CI.
- `mypaas server …` is the server CLI, run as root. It changes local state only.
- `mypaas server daemon` (the HTTP API on `127.0.0.1:88` behind Traefik) and
  `mypaas server socket-proxy` (a read-only Docker socket for Traefik) are
  started by systemd.

[docs/design.md](docs/design.md) is the
specification, including the threat model. [README.md](README.md) is the user
documentation, and [docs/ci.md](docs/ci.md) covers deploying from CI. When
behaviour changes, update the code, the design document, the README and
`docs/ci.md` in the same change. Each one must keep matching the code.

## Commands

```sh
cargo build
cargo test                                  # unit tests, no Docker needed (~1s)
cargo test --test docker -- --ignored       # end-to-end, needs Docker, busybox:1.36, traefik:v<TRAEFIK_VERSION>
cargo clippy --all-targets                  # must stay warning-free
cargo build --no-default-features           # without LAN mode (no mdns-sd)
cargo test --no-default-features            # same tests, LAN feature off
scripts/build-static.sh                     # musl static binary in target-musl/
scripts/build-deb.sh                        # .deb of that binary in target-deb/
scripts/vm-test.sh up|test|ssh|down         # full run on a local Debian VM (QEMU/KVM)
```

To run a daemon locally:
`MYPAAS_HOME=/tmp/x mypaas server init --domain localhost.test`, then
`MYPAAS_HOME=/tmp/x mypaas server daemon`. The client accepts a plain-HTTP
remote on loopback only: `mypaas -r http://127.0.0.1:88 ls`. In LAN mode with
`plain_admin`, it also accepts `http://` for a `.local` host (see
[design.md](docs/design.md#lan-mode)).

## Source layout

| Path | Contents |
| --- | --- |
| `src/main.rs` | clap CLI, dispatch, `server deploy` |
| `src/spec.rs` | `# mypaas.*` Dockerfile comment parsing and validation (`ServiceSpec`) |
| `src/auth.rs` | Keys, scopes, the canonical string and Ed25519 signatures, dates |
| `src/bundle.rs` | Reproducible `tar.zst` packing, the strict unpacker |
| `src/config.rs` | `~/_mypaas` paths, `config.toml`, defaults and limits, volume path checks, secrets files |
| `src/client/` | Client: remotes, signed requests, `push` (`mod.rs`), day-to-day commands (`ops.rs`), `dashboard` (`dashboard.rs`) and text charts (`chart.rs`) |
| `src/server/daemon.rs` | HTTP server, routing, auth, tickets, uploads |
| `src/server/guard.rs` | Backoff, nonces, tickets, rate limiter (in memory, bounded) |
| `src/server/deploy.rs` | Build, the three strategies, Traefik labels, rollback, crash recovery |
| `src/server/api.rs` | ls, status, logs, secrets handlers |
| `src/server/docker.rs` | Thin `docker` CLI wrapper (argument vectors, never a shell) |
| `src/server/owner.rs` | Image `USER` resolution, chowning named volumes |
| `src/server/setup.rs` | `server init`, keys, Traefik files, systemd units, restart |
| `src/server/lan/` | LAN mode, behind the `lan` feature: `mod.rs` facade and types, `hosts.rs` derivation and validation of `.local` names, `announce.rs` reconcile loop and the `Announcer` trait, `mdns.rs` mdns-sd backend |
| `src/server/sockproxy.rs` | Read-only Docker socket proxy for Traefik |
| `src/server/stats.rs` | History: access log ingest and usage samples in hourly buckets |
| `.github/workflows/` | `ci.yml` (clippy, tests, Docker tests, builds, .deb install check) and `release.yml` (tag `v<version>` → GitHub release) |
| `scripts/` | `build-static.sh`, `build-deb.sh` (package metadata and maintainer scripts inline), `vm-test.sh` |
| `tests/docker.rs` | `#[ignore]`d end-to-end tests against a real Docker |
| `examples/` | Deployable example services, one per pattern; index in [examples/README.md](examples/README.md) |
| `docs/` | [design.md](docs/design.md) (specification and decisions log), [security.md](docs/security.md) (security review, remaining risk, operator checklist), [ci.md](docs/ci.md) (deploying from CI) |

## Invariants to keep

These are security properties from the design. A change must not weaken them
without an explicit decision recorded in the design document.

- **Authentication before data.** Small requests are signed and limited to
  64 KiB. `PUT /v1/deploy` is decided from the headers alone, using a
  single-use ticket bound to the bundle's size and hash. Never read a body
  before authentication.
- **One answer for every auth failure.** Every auth failure returns the same
  `403 denied` body. Unknown paths return the same `404`. Do not add
  distinguishing detail.
- **Bounded state.** Every in-memory table (backoff, nonces, tickets) has a
  hard cap.
- **Logs stay clean.** Never log tickets, signatures or secret values. Secret
  values must not appear in progress lines or errors either.
- **Docker is called with argument vectors only**, never through a shell.
  Every value that reaches a docker argument, a Traefik label or a file path is
  validated in `spec.rs` or `config.rs` first.
- **Services stay isolated.** Each service gets its own network,
  `--cap-drop ALL` and `no-new-privileges`. A service reads only its own
  secrets and only its own volume tree, unless an admin and the server config
  allow more.
- **Routing comes from mypaas alone.** Images whose labels start with
  `traefik.` or `mypaas.` are refused (Docker copies image labels onto
  containers), and no service may use the admin domain. The API routers keep
  priorities above anything a service rule can reach.
- **Signatures cover the host.** The canonical string includes the Host
  header; keep client and daemon in step when it changes.
- **Traefik gets nothing extra.** It runs with `ROUTER_HARDENING` (no
  capabilities but `NET_BIND_SERVICE`, read-only, limits) and a digest-pinned
  image. `tests/docker.rs` repeats those flags.
- **Trust is edited on the server only.** Keys, the volume allowlist and the
  API allowlist change on the server, never through the API.
- **Prune only what is ours.** Only objects labelled `mypaas.service` are ever
  pruned or removed.
- **LAN mode stays removable, and off means off.** All of it lives in
  `src/server/lan/`; other files call it only through the facade in `mod.rs`
  (`start`, `poke`, `check_urls`, `plain_remote_allowed`, `read_state`). With
  `lan.enabled = false` there is no thread, no socket, no state file, and the
  Traefik files and systemd unit are byte-identical to a build without LAN mode
  (golden files in `src/server/testdata/`). Both feature sets must build, test
  and pass clippy.
- **Plain HTTP only where decided.** The client speaks plain HTTP to loopback,
  and to `.local` hosts only when built with the `lan` feature. The admin API
  is served on port 80 only with `lan.plain_admin`, which config validation
  allows only with `lan.enabled` and a `.local` admin domain. `secret set`
  needs `--plain-secrets` on a plain LAN remote.

## LAN mode notes

- The announcer is a reconcile loop on its own thread (not in tokio): it reads
  `services/*/current.json` every 5 s, and at once when a daemon deploy,
  rollback or removal pokes it, and writes `lan/state.json`. Test it with the
  fake `Announcer` in `announce.rs`; only `mdns.rs` touches the network.
- Names get A and AAAA records. Publishing IPv4 only made dual-stack lookups
  stall for about 5 s, or fail when IPv6 stayed open (spike, see
  docs/design.md). Global IPv6 addresses are left out of the AAAA records on
  purpose (their prefix changes, so `api_allow` cannot cover clients that use
  them); only link-local and unique local ones are published. Do not disable
  IPv6 in `mdns.rs` without re-testing with
  `getent hosts` and `avahi-resolve`.
- To try it locally: `MYPAAS_HOME=/tmp/x mypaas server init --lan --domain
  paas-test.local`, set `listen` to a free loopback port, run
  `mypaas server daemon`, then `avahi-resolve -4 -n paas-test.local`.

## Conventions

- Rust edition 2024, so let-chains (`if let … && …`) are used throughout.
- Errors use `anyhow` with context. Client errors that need a specific exit
  code use `client::exit(code, msg)`: 1 error, 2 usage, 3 denied, 4 deploy
  failed.
- Write files with `config::write_atomic` (temp file and rename, with an
  explicit mode).
- Comments say why, not what. Tests are named after the promise they check
  (for example `every_auth_failure_looks_the_same`).
- The examples are user documentation and are tested: `spec::tests::examples_are_valid`
  parses every `examples/**/Dockerfile*` and requires unique service names.
  When a comment key is added, changed or removed, update the examples and
  [examples/README.md](examples/README.md). When writing an example, keep it
  buildable with the default hardening (non-root `USER`, no capabilities).
- Add unit tests next to the code (`#[cfg(test)] mod tests`). Behaviour that
  needs Docker goes in `tests/docker.rs` behind `#[ignore]`.
- `TRAEFIK_VERSION` and `TRAEFIK_DIGEST` in `config.rs` pin Traefik (get the
  digest with `docker buildx imagetools inspect traefik:v<version>`). When you
  bump them, also update
  `tests/docker.rs`, the call list in `sockproxy.rs`, the README and the design
  document, which repeat the version.
- A release is a tag `v<version>` matching `Cargo.toml`; `release.yml` refuses
  any other. Actions in the workflows are pinned by commit SHA with the tag in
  a comment; keep it that way when bumping them. The build job must not get
  `contents: write`.
- The `target*/` directories are build outputs and the VM disk. Never commit
  them.
