# mypaas-rs

A single-server PaaS on Docker and Traefik. You describe a service with
`# mypaas.*` comments in its Dockerfile and deploy it with `mypaas push`. The
server exposes one small HTTPS API behind Traefik, and nothing else: no
dashboard, no SSH needed for deploys, no database.

One binary does everything:

- `mypaas …` is the client, on your laptop or in CI.
- `mypaas server …` runs on the server as root and changes local state only.
- `mypaas server daemon` and `mypaas server socket-proxy` are started by systemd.

The full design, including the threat model, is in
[docs/design.md](docs/design.md); deploying from CI is covered in
[docs/ci.md](docs/ci.md).

## Contents

- [Quick start](#quick-start)
- [Configuring a service](#configuring-a-service)
- [Client commands](#client-commands)
- [Keys and scopes](#keys-and-scopes)
- [Server](#server)
- [Security model](#security-model)
- [Development](#development)
- [Status](#status)

## Quick start

### 1. Prepare the server

You need a Linux server with Docker and systemd, and a DNS name for the admin
API (here `paas.example.com`) pointing at it. Ports 80 and 443 must be free.

Build the statically linked binary (needs only Docker on your machine; it runs
on any x86_64 Linux, whatever its glibc) and copy it to the server:

```sh
scripts/build-static.sh
scp target-musl/x86_64-unknown-linux-musl/release/mypaas root@server:/usr/local/bin/mypaas
```

On your laptop, create a key (or use an existing `~/.ssh/id_ed25519`) and
print its public line:

```sh
mypaas key init                 # asks for a passphrase; empty for none
mypaas key show > mykey.pub
```

On the server, as root:

```sh
mypaas server init --domain paas.example.com --email you@example.com --key mykey.pub
mypaas server restart all
```

`init` creates `~/_mypaas`, writes the Traefik configuration and the systemd
units, and adds your key with the `admin` scope. It is safe to run again: keys,
certificates and secrets are kept.

### 2. Deploy

On your laptop:

```sh
mypaas remote add prod paas.example.com
mypaas doctor                   # key, TLS, clock, signed request
mypaas push path/to/project     # or a path to a Dockerfile
```

A minimal Dockerfile:

```dockerfile
# mypaas.service=hello
# mypaas.url=https://hello.example.com
# mypaas.port=8080

FROM python:3.13-slim
WORKDIR /app
COPY . .
USER 1000
CMD ["python", "-m", "http.server", "8080"]
```

`push` packs the directory (honouring `.gitignore`), uploads it, builds the
image on the server and streams the build and deploy log back. The last line is
`result: ok` or `result: failed`.

## Configuring a service

[examples/](examples/README.md) has complete services to start from: a static
site, a web app with zero-downtime deploys, SQLite on a volume, staging and
production from one project, a multi-stage Go build, a Redis worker and a
shared Postgres.

A service is one Dockerfile. A line counts as configuration when it starts with
`#` followed by `mypaas.`, with spaces or tabs allowed around the `#`. Writing
`#--- mypaas.url=` switches a line off. Unknown keys fail the deploy.

| Key | Meaning | Default |
| --- | --- | --- |
| `service` | Name of the service; a deploy replaces the service with this name. Required. | |
| `url` | `http(s)://host[/path]`, repeatable. Without one the service is internal. | |
| `port` | Port the process listens on inside the container | `80` |
| `volume` | `name:/path[:ro]` or `/host/path:/path[:ro]`, repeatable | |
| `env` | `KEY=value`, or `KEY` alone to take the value from the service's secrets | |
| `scale` | `N` plus `roll` or `safe`; `0` means a single container | `0`, `roll` |
| `healthcheck` | `/path interval timeout`, e.g. `/health 10s 2s`; used with `scale` above 0 | |
| `maxcpu`, `maxmem` | Limits per container, within the server maximums | `1`, `512m` |
| `network` | Shared network to join, created if missing, repeatable; see [Shared networks](#shared-networks) | |
| `readonly` | `true` mounts the root filesystem read-only, with a tmpfs on `/tmp` | `false` |
| `cap_add` | Linux capabilities to add back; needs `admin` | |

### Several instances of one project

The identity of a deployment is `mypaas.service`, not the directory. Two
Dockerfiles with different service names are independent deployments (see
[examples/staging-prod](examples/staging-prod)):

```sh
mypaas push Dockerfile.staging
mypaas push Dockerfile.prod
```

### What gets uploaded

The client walks the project and applies the `.gitignore` files inside it.
Global ignore files, `.git/info/exclude` and `.gitignore` files above the
project are not read, so every machine packs the same bundle. `.git` is always
left out, and symlinks are skipped with a warning. The chosen Dockerfile is
stored as `Dockerfile`; a `.dockerignore` still applies to the build.

```sh
mypaas push --dry-run           # file count, sizes and the ten largest files
```

Bundles are limited to 100 MiB compressed and 200 MiB unpacked by default
(the server's `[limits]` decide). The client warns above 50 MiB and lists the
largest files, since a stray binary is usually a missing `.gitignore` entry.

### Deploy strategies

The image is built first, so a failed build changes nothing. Then:

| Mode | Set by | What happens |
| --- | --- | --- |
| Single | no `scale`, or `scale=0` | The old container stops, then the new one starts. |
| Safe | `scale=N safe` | All old containers stop, then N new ones start; versions never mix. |
| Roll | `scale=N roll` | One new container starts and must be healthy before one old one stops, N times. No downtime. |

A new container counts as ready when its healthcheck answers 2xx or 3xx within
60 seconds. Without a healthcheck it must still be running, and not restarting,
after 5 seconds. If anything fails, the new containers are removed and the old
ones come back.

`mypaas rollback <service>` redeploys the previous image with the
configuration it was deployed with. Only one previous version is kept, so a
second rollback fails.

### Volumes

A volume source without a leading `/` or `~` is a name. It maps to
`~/_mypaas/volumes/<service>/<name>` on the server and is created if missing.
If the image runs as a non-root `USER`, an empty named volume is handed to that
user. A volume that already holds data owned by someone else is not changed;
the deploy prints the `chown` command to run instead.

An absolute host path needs the `admin` scope and an entry in the server's
`[volumes] allow` list. This is also the only way for two services to share a
directory.

### Secrets

```sh
mypaas secret set shop DB_PASSWORD      # hidden prompt, or pipe the value on stdin
mypaas secret ls shop                   # names only
mypaas secret rm shop DB_PASSWORD
```

`# mypaas.env=DB_PASSWORD` in the Dockerfile reads the value from the
service's own secrets; a missing secret fails the deploy. Values can't be read
back through the API and never appear in logs. A changed secret takes effect at
the next deploy or rollback. Values are passed as environment variables, so
root on the server can see them with `docker inspect`.

### Shared networks

Every service runs on its own Docker network and can't reach any other
service. To let several services share, for example, one Postgres, they all
name the same network:

```dockerfile
# mypaas.network=db
```

The network is created on the first deploy that names it, and each service on
it is reachable there by its service name. Nothing is configured on the
server, and any service may join, so the network is a convenience, not a
security boundary: a service on it must check who connects, for example with
one Postgres role and database per application.

There are no host port mappings. Everything public goes through Traefik as
HTTP or HTTPS; everything else is reached over a shared network.

[examples/postgres](examples/postgres) is a complete setup: one Postgres
shared by two apps, each with its own role and database, running with all
capabilities dropped.

### Single writer (SQLite and friends)

The default, `scale=0`, stops the old container before it starts the new one,
so two processes never open the same database file. With `scale` above 0 the
replicas share their volumes, and the deploy warns if one of them is writable.

## Client commands

| Command | Scope | What it does |
| --- | --- | --- |
| `push [PATH] [--dry-run]` | `deploy` | Pack, upload, build and deploy |
| `ls` | `read` | Replicas, health, age, requests in the last hour, CPU, memory, URLs |
| `status <service>` | `read` | Containers, URLs, networks, volumes, env names, last and previous deploy |
| `logs <service> [-f] [--tail N] [--since 10m] [--container N]` | `logs` | Container logs, merged across replicas |
| `top` | `read` | Live view, refreshed every few seconds; `q` quits |
| `stats <service> [--since 7d] [--by hour\|day]` | `read` | Requests, status classes, p50/p95 latency and a sparkline |
| `dashboard [service] [--since 24h] [--watch]` | `read` | Uptime, memory and request history as terminal charts |
| `rollback <service>` | `deploy` | Redeploy the previous version |
| `rm <service>` | `rm` | Remove containers, networks, stored spec and stats; volumes and secrets stay |
| `secret set\|ls\|rm <service> [KEY]` | `secrets` | Manage secrets |
| `doctor` | `read` | Check the key, TLS, the clock and a signed request |
| `remote add\|ls\|rm` | | Manage servers in `~/.config/mypaas/remotes.toml` |
| `key init\|gen\|show` | | Create or select a key, print a CI key, print the public line |

Global options:

- `-r <name>` picks a remote; a domain (`paas.example.com` or
  `https://paas.example.com`) works too. With a single remote it is the
  default, and `MYPAAS_REMOTE` overrides it.
- `--json` prints JSON, one object per line for streams.
- `-q` prints only errors and results; `--no-color` turns off colour.

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Success |
| `1` | Error |
| `2` | Usage error |
| `3` | Denied or locked out |
| `4` | The deploy failed on the server |

So CI can tell a bad key from a bad build.

### CI

See [docs/ci.md](docs/ci.md) for keys, scopes and ready-to-use GitHub Actions
and GitLab CI jobs. In short:

```sh
MYPAAS_PRIVATE_KEY="$CI_KEY" mypaas -r paas.example.com push .
```

## Keys and scopes

Keys are Ed25519 in OpenSSH format, so an existing `~/.ssh/id_ed25519` works,
with or without a passphrase. The client uses, in this order:

1. `MYPAAS_PRIVATE_KEY`
2. the key chosen with `mypaas key init`
3. `~/.config/mypaas/id_ed25519`
4. `~/.ssh/id_ed25519`

The server's `~/_mypaas/authorized_keys` has one key per line:

```
ssh-ed25519 AAAA… ci scope=deploy:shop-*,read expires=2027-01-01
```

| Scope | Allows |
| --- | --- |
| `read` | `ls`, `status`, `top`, `stats`, `doctor` |
| `logs` | `logs` |
| `deploy:<glob>` | `push` and `rollback` for matching services |
| `secrets:<glob>` | `secret` for matching services |
| `rm:<glob>` | `rm` for matching services |
| `admin` | Everything, including `cap_add` and host-path volumes |

Keys are managed on the server only. The API has no endpoint for them, not
even for `admin`:

```sh
mypaas server key add mykey.pub --name laptop --scope admin
mypaas server key add - --name ci --scope 'deploy:shop-*' --scope read --expires 2027-01-01 < ci.pub
mypaas server key ls
mypaas server key rm ci         # takes effect within 5 seconds
```

## Server

### Files

Everything lives in `~/_mypaas`, which is also the only tree to back up:

```
~/_mypaas/
  config.toml           settings, limits, volume allowlist
  authorized_keys       keys and scopes
  traefik.toml  staticroutes.toml  acme.json
  secrets/<service>.toml
  services/<service>/current.json  previous.json
  volumes/<service>/<name>/
  stats/                traffic buckets
  logs/access.log       Traefik access log, rotated by the daemon
  run/docker.sock       read-only Docker socket for Traefik
  tmp/                  uploads, removed after every deploy
```

### config.toml

`config.toml` is read again at every deploy, so changes to `[defaults]`,
`[limits]`, `[maximums]` and `[volumes]` need no restart. Changes to `[router]` and
`[stats]` take effect after `mypaas server restart all`. The defaults:

```toml
domain = "paas.example.com"
email = ""                      # Let's Encrypt account email, optional
listen = "127.0.0.1:88"         # the daemon; must be a loopback address

[limits]
max_upload = "100m"
max_unpacked = "200m"
max_files = 40000
max_depth = 32
build_timeout = "15m"

[defaults]                      # per container, unless the Dockerfile says otherwise
cpu = 1.0
mem = "512m"
pids = 512
log_max_size = "10m"
log_max_file = 3

[maximums]                      # the most a Dockerfile may ask for
cpu = 4.0
mem = "4g"

[volumes]
allow = ["/srv/media"]          # host paths admin deploys may mount
chown = true                    # hand empty named volumes to the image's user

[router]
socket_proxy = true             # Traefik gets a read-only Docker socket
read_timeout = "300s"           # longest request on :443, uploads included
strict_sni = true               # refuse TLS for names without a certificate
api_allow = []                  # e.g. ["203.0.113.0/24"]: only these reach the API

[stats]
enabled = true                  # Traefik writes an access log for `mypaas stats`
retention_days = 90
```

The server refuses to start if a `volumes.allow` entry overlaps `/`,
`~/.ssh`, `~/_mypaas` or `/var/run`.

### Server commands

| Command | What it does |
| --- | --- |
| `server init [--domain D] [--email E] [--key FILE]` | Create `~/_mypaas`, the Traefik files and the systemd units |
| `server restart all\|daemon\|router` | Restart the daemon (`mypaasd`), or the socket proxy and Traefik, or everything |
| `server deploy DIR` | Deploy a local directory as admin, through the same code as the API; for debugging |
| `server key add\|ls\|rm` | Manage `authorized_keys` |
| `server schedule-reboot [WHEN]` | Reboot on a systemd `OnCalendar` schedule, default `Sun 06:00:00` |

Logs go to the journal: `journalctl -u mypaasd` and
`journalctl -u mypaas-socket-proxy`. Set `MYPAAS_LOG=debug` in the unit for
more detail.

### Traffic stats

Traefik writes a JSON access log that keeps only the service name, status,
duration and start time; client addresses, paths and headers are dropped
before they reach the disk. The daemon reads the log every 10 seconds into
hourly buckets per service and keeps them for `retention_days`. The log is
rotated at 32 MiB without losing lines.

### Dashboard

Once a minute the daemon also records, per service, whether its replicas are
running and how much memory and CPU they use. `mypaas dashboard` shows it all
in the terminal:

```
$ mypaas dashboard shop
shop  healthy 2/2  last 24h by hour
uptime 99.86%  requests 48210  5xx 0.02%  p95 41ms  mem peak 212.4 MiB

requests  per hour
  3.1k ┤                ▂▅▇█▆▃
       ┤             ▁▅████████▅▂
     0 ┤▁▁▁▂▂▃▄▅▆▇███████████████▃
       └09:00    15:00    21:00    11-08

memory  peak of all replicas
  212M ┤        ▃▃▃▃▅▅▅▇▇▇███████
    0B ┤▅▅▅▅▅▅▅▅████████████████████
       └09:00    15:00    21:00    11-08

uptime  worst part of each column
99.86% │████████████████▃█████████
█ all replicas up  ▆ some replicas down  ▃ partly down  ▁ down  · no data
```

Without a service name it shows one row per service with sparklines.
`--since 7d` widens the window (hourly up to 14 days, daily beyond), `--watch`
redraws full-screen every 10 seconds, and `--json` prints the data. Hours
without samples, for example while the server was off, show as `·`, not as
downtime.

## Security model

The full review, the remaining risk and a checklist for the server's operator
(host firewall, SSH, keys) are in [docs/security.md](docs/security.md).

- **One public entry.** Traefik is the only process on a public port, and it
  forwards only `/v1/` on the admin domain to the daemon, which listens on
  loopback.
- **Authentication before data.** Small requests are signed with Ed25519 and
  must be within 30 seconds of the server's clock; nonces prevent replays.
  Uploads need a single-use ticket that is bound to the exact bundle size and
  hash, so a bad upload is refused from its headers alone, before a byte of
  the body is read.
- **No information for attackers.** Every failed authentication gets the same
  `403 denied`, and failures from one address cost exponentially growing
  lockouts, up to an hour.
- **Isolated containers.** Each service runs on its own network with all
  capabilities dropped, `no-new-privileges`, a process limit and memory and
  CPU limits. Privileged mode, host networking, devices and the Docker socket
  are never allowed.
- **A read-only Docker socket for Traefik.** The socket proxy allows only the
  five calls Traefik needs and removes environment variables, and with them
  the secrets, from what it returns.
- **Hardened systemd units.** The daemon and the socket proxy run sandboxed;
  the proxy has no network and no capabilities at all.

A deploy key runs arbitrary `RUN` steps inside Docker, so treat it like any
other CI credential. The design document has the full threat model.

## Development

```sh
cargo test                                  # unit tests, no Docker needed
cargo test --test docker -- --ignored       # end-to-end tests against Docker
cargo clippy --all-targets
```

The end-to-end tests start real daemons, socket proxies and Traefik
containers. They need Docker and the `busybox:1.36` and `traefik:v3.7.17`
images, use unique names and clean up after themselves.

### Testing on a real systemd server, locally

`scripts/vm-test.sh` boots a Debian 13 VM under QEMU/KVM and runs the whole
thing as on a server: `server init`, the systemd units, Traefik on ports 80 and
443, deploys from this machine, the shared Postgres example, traffic stats and
a reboot. It needs `qemu-system-x86_64`, `qemu-img` and `genisoimage`, but no
root and no libvirt; only loopback ports are forwarded to the VM.

```sh
scripts/build-static.sh
scripts/vm-test.sh up       # first time: downloads and verifies the cloud image
scripts/vm-test.sh test     # can be run again on the same VM
scripts/vm-test.sh ssh      # look around
scripts/vm-test.sh down
```

Let's Encrypt is not part of it: Traefik serves its default certificate, and
the client reaches the daemon through an SSH tunnel.

For local experiments, the client also accepts a plain-HTTP remote on
loopback, e.g. `mypaas -r http://127.0.0.1:18088 ls` against a daemon started
with `MYPAAS_HOME=/tmp/x mypaas server daemon`.

The source is laid out like this:

| Path | Contents |
| --- | --- |
| `src/spec.rs` | Dockerfile comment parsing and validation |
| `src/auth.rs` | Keys, scopes, request signing |
| `src/bundle.rs` | Packing and the strict unpacker |
| `src/config.rs` | `~/_mypaas` paths, `config.toml`, volume path checks |
| `src/client/` | The client CLI |
| `src/server/daemon.rs` | HTTP API, authentication, uploads |
| `src/server/guard.rs` | Backoff, nonces, tickets, rate limits |
| `src/server/deploy.rs` | Builds, strategies, rollback, recovery |
| `src/server/api.rs` | Services, status, logs, secrets |
| `src/server/stats.rs` | Access log ingest, usage samples and the hourly history buckets |
| `src/server/sockproxy.rs` | Read-only Docker socket for Traefik |
| `src/server/owner.rs` | Image user and volume ownership |
| `src/server/setup.rs` | `server init`, keys, Traefik files, systemd units |

## Status

All four phases of the design are implemented and covered by tests, including
a full run on a local systemd VM. Still to do:

- A trial on a public server: real Let's Encrypt certificates and deploys
  from CI.

Signing through `ssh-agent` is deliberately not supported: the client reads
its key from a file or `MYPAAS_PRIVATE_KEY` and depends on nothing from SSH.
