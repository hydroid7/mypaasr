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
scripts/build-static.sh                     # musl static binary in target-musl/
scripts/vm-test.sh up|test|ssh|down         # full run on a local Debian VM (QEMU/KVM)
```

To run a daemon locally:
`MYPAAS_HOME=/tmp/x mypaas server init --domain localhost.test`, then
`MYPAAS_HOME=/tmp/x mypaas server daemon`. The client accepts a plain-HTTP
remote on loopback only: `mypaas -r http://127.0.0.1:88 ls`.

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
| `src/server/sockproxy.rs` | Read-only Docker socket proxy for Traefik |
| `src/server/stats.rs` | History: access log ingest and usage samples in hourly buckets |
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
- The `target*/` directories are build outputs and the VM disk. Never commit
  them.
