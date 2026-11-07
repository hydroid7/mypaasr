# mypaas-rs design

mypaas-rs is a single-server PaaS on Docker and Traefik. You describe a service
with `# mypaas.*` comments in its Dockerfile and deploy it with `mypaas push`.
The server exposes one small HTTPS API and nothing else: no web dashboard, no
SSH needed for deploys, no database.

This document describes how the system works and why. The decisions log at
the end records each design decision with its reason; when the code and this
document disagree, one of them is a bug. The security review, with its
findings, the remaining risk and an operator checklist, is in
[security.md](security.md).

## Contents

1. [Goals](#goals)
2. [Threat model](#threat-model)
3. [Architecture](#architecture)
4. [Authentication](#authentication)
5. [The API](#the-api)
6. [Abuse protection and limits](#abuse-protection-and-limits)
7. [Describing a service](#describing-a-service)
8. [Isolation](#isolation)
9. [Volumes and secrets](#volumes-and-secrets)
10. [Routing with Traefik](#routing-with-traefik)
11. [Deploys and rollback](#deploys-and-rollback)
12. [History and the dashboard](#history-and-the-dashboard)
13. [Client CLI](#client-cli)
14. [Server CLI](#server-cli)
15. [Decisions log](#decisions-log)
16. [Open questions](#open-questions)

## Goals

- **One Dockerfile per service.** Its comments are the whole configuration, and
  `mypaas push` deploys it.
- **A small server surface.** One daemon API behind Traefik, reached over
  HTTPS. No dashboard, no extra listeners.
- **Deploys from anywhere.** A laptop or a CI runner needs only HTTPS and a
  key, not SSH access.
- **No work for strangers.** An unauthenticated caller can never make the
  server read, store or unpack a large amount of data.
- **Isolated services by default.** Services share something (a network, a
  directory) only when that is asked for.
- **The terminal is the dashboard.** Status, logs, traffic, uptime and memory
  history come from the client CLI.

**Non-goals:** several servers or a cluster, a web UI, building images anywhere
but on the server, Kubernetes, raw TCP or UDP services.

## Threat model

| Actor | Can | Cannot |
| --- | --- | --- |
| Internet client without a key | Reach ports 80 and 443 and send small requests, which are rejected cheaply and slowed down exponentially | Upload data, read anything, learn whether a key id exists |
| Holder of a scoped key | Deploy, roll back, read logs of, manage secrets of or remove the services its scope covers. Its containers run with all capabilities dropped on their own network, and can join any shared network they name. | Act on services outside its scope, read a secret value back, route the admin domain or another service's URL, set Traefik labels, mount host paths, add capabilities, run privileged containers |
| Holder of an admin key | Additionally add capabilities and mount allowlisted host paths | Add, change or remove keys, or change the allowlist (both are edited on the server only) |

A build runs arbitrary Dockerfile `RUN` steps, so a deploy key is as powerful
as any production CI credential. Shared networks are open on purpose (see
[Isolation](#isolation)): a service on one must authenticate its clients
itself. Out of scope: container escapes through bugs in Docker or the kernel,
and a compromised host.

## Architecture

One Rust binary runs next to Docker and Traefik on one VM. Traefik is the only
process with a public port.

```
                     internet
                        │ :80 :443
               ┌────────▼─────────┐
               │     Traefik      │  TLS, Let's Encrypt, routing by labels
               └──┬─────────────┬─┘
     /v1 on the   │             │  service URLs
     admin domain │             │
     ┌────────────▼───┐     ┌───▼─────────────────────────────────┐
     │ mypaas daemon  │     │ mypaas-shop     mypaas-blog         │ one network
     │ 127.0.0.1:88   │     │  shop.1 shop.2   blog               │ per service
     └───────┬────────┘     │        └───┬───────┘                │
             │              │       mypaas-net-db: postgres       │ shared, by name
             │ docker CLI   └─────────────────────────────────────┘
     ┌───────▼────────┐  read-only  ┌──────────────────────┐
     │ Docker engine  ◄─────────────┤ socket proxy         ◄── Traefik's Docker provider
     └────────────────┘             └──────────────────────┘
```

**One binary, four roles**

- `mypaas …` is the client, on a laptop or in CI.
- `mypaas server …` is the server CLI. It runs as root and changes local state
  only.
- `mypaas server daemon` is the API, started by systemd as `mypaasd`.
- `mypaas server socket-proxy` gives Traefik a read-only Docker socket, started
  by systemd as `mypaas-socket-proxy`.

The binary is statically linked (musl), so the server needs no language
runtime. Docker, systemd and the Traefik image are the only dependencies.
Releases on GitHub carry it as a plain file and as a `.deb` that installs
`/usr/bin/mypaas` only; `server init` still writes the configuration and the
units, so both install paths end in the same state.

**State on the server**

```
~/_mypaas/
  config.toml           domain, email, limits, defaults, maximums, volume allowlist
  authorized_keys       public keys with scopes, edited on the server only
  traefik.toml  staticroutes.toml  acme.json
  secrets/<service>.toml
  services/<service>/current.json  previous.json   the deployed specs
  volumes/<service>/<name>/
  stats/<service>.json  traffic, uptime and memory history
  logs/access.log       Traefik's access log, read and rotated by the daemon
  run/docker.sock       the socket proxy
  tmp/                  uploads and unpacked bundles
```

There is no database. Running state comes from Docker (containers carry a
`mypaas.service` label) and these files. `~/_mypaas` is the only tree to back
up.

## Authentication

Every request is authenticated before any bulk data is read. Small requests
carry an Ed25519 signature. An upload carries a single-use ticket that the
client obtains first with a signed request.

### Keys and scopes

- Ed25519 keys in OpenSSH format, so an existing `~/.ssh/id_ed25519` works.
  `mypaas key init` creates one, with or without a passphrase. `mypaas key gen`
  prints a passwordless one for CI, which passes it in `MYPAAS_PRIVATE_KEY`.
- The server reads `~/_mypaas/authorized_keys`, one key per line:
  `ssh-ed25519 AAAA… name scope=deploy:shop-*,read expires=2027-01-01`. It
  re-reads the file at most every 5 seconds. A file that does not parse
  rejects every key until it is fixed, so a typo cannot silently widen access.
- The key id is the SHA-256 fingerprint of the public key.
- Scopes: `read`, `logs`, `deploy:<glob>`, `secrets:<glob>`, `rm:<glob>` and
  `admin`. Globs match service names.

### Signed requests

The client signs, with Ed25519, the lines `mypaas/v1`, the host (the admin
domain, lowercase), the method, the path and query, a Unix timestamp, a random
nonce and the SHA-256 of the body. The host binds a signature to one server:
Traefik routes only `Host(<admin domain>)` to the daemon, so a request signed
for another server that trusts the same key is refused. It sends
the signature in `X-Mypaas-Key`, `X-Mypaas-Time`, `X-Mypaas-Nonce` and
`X-Mypaas-Sig`.

- Bodies are at most 64 KiB (16 KiB for a secret value).
- Timestamps are accepted within 30 seconds of the server's clock.
- Nonces are remembered for 60 seconds, so a captured request cannot be
  replayed.
- Every error response carries the server's time, so the client can point out
  clock skew.

### Uploads with a ticket

1. The client packs the project as `tar.zst` and computes its size and
   SHA-256.
2. A signed `POST /v1/deploy-tickets` sends `{service, size, sha256}`. The
   server checks the signature, the nonce, the scope `deploy:<service>`, the
   size against `max_upload`, the free disk space and the cap on open tickets.
3. The server returns a ticket: 256 random bits, valid for 60 seconds, usable
   once, bound to the key, the service, the size and the hash. Tickets live in
   memory only, and the server stores only the SHA-256 of each token.
4. The client sends `PUT /v1/deploy` with `Authorization: Bearer <ticket>` and
   a `Content-Length` equal to the declared size.
5. The server decides from the headers alone. An unknown, expired or used
   ticket, or a different `Content-Length`, is rejected before a byte of the
   body is read. Otherwise the ticket is consumed, and the body is streamed to
   a temp file while it is counted and hashed. Reading stops as soon as the
   count passes the declared size.
6. After the last byte, the hash must match the signed one; otherwise the file
   is deleted unopened.
7. The bundle is unpacked, and the Dockerfile's `mypaas.service` must equal
   the ticket's service, so lying about the service in step 2 does not get
   around the scope. The deploy log streams back in the response.

A stolen ticket is of little use: it is bound to one exact bundle, works once,
expires after 60 seconds and only travels over TLS.

The client talks HTTP/2 to Traefik. HTTP/2 is full duplex, so when the daemon
refuses an upload from its headers (`409 busy`, an expired ticket), the answer
arrives at once and the client stops sending. Over HTTP/1.1, Traefik passes
such an answer on only after the client has sent the whole body. The daemon
never reads that body either way.

## The API

The daemon listens on `127.0.0.1:88`. Traefik forwards `/v1` on the admin
domain to it and nothing else. There is no unauthenticated endpoint, no HTML
and no banner.

| Endpoint | Auth | Returns |
| --- | --- | --- |
| `POST /v1/deploy-tickets` | signed, `deploy:<service>` | a ticket |
| `PUT /v1/deploy` | ticket | the deploy log, streamed |
| `GET /v1/services` | signed, `read` | every service: replicas, health, CPU, memory, requests in the last hour, URLs, last deploy |
| `GET /v1/services/{name}` | signed, `read` | containers, URLs, networks, volumes, env names, last and previous deploy |
| `GET /v1/services/{name}/logs` | signed, `logs` | log lines as JSON, streamed (`follow`, `tail`, `since`, `container`) |
| `GET /v1/services/{name}/stats` | signed, `read` | history per hour or day: requests, status classes, latency, uptime, memory, CPU (`since`, `by`) |
| `POST /v1/services/{name}/rollback` | signed, `deploy:<name>` | the rollback log, streamed |
| `DELETE /v1/services/{name}` | signed, `rm:<name>` | the removal log, streamed |
| `GET /v1/services/{name}/secrets` | signed, `secrets:<name>` | secret names only |
| `PUT`, `DELETE /v1/services/{name}/secrets/{key}` | signed, `secrets:<name>` | result |

Streamed responses end with a line `result: ok` or `result: failed`, because
the HTTP status is sent before the work runs.

**What keeps the surface small**

- Two Traefik routers: `Path(/v1/deploy)` for uploads and `PathPrefix(/v1/)`
  for the rest. A prefix match on `/v1/deploy` would also catch
  `/v1/deploy-tickets`.
- No buffering middleware in front of the daemon. It would read the whole body
  before the daemon could reject it from the headers.
- Every body needs a `Content-Length`; chunked bodies are refused.
- HTTP/1.1 only, headers at most 8 KiB, at most 64 connections, at most 8
  requests in flight per client address. The header read times out after 10
  seconds, an upload after 15 seconds without progress or after
  `router.read_timeout` (300 s) in total.
- One answer for every authentication failure: unknown key, expired key,
  missing scope and bad signature all get the same `403 denied`. Unknown paths
  get the same `404` for everyone.
- The log records key name, endpoint and outcome, never tickets, signatures
  or secret values.

## Abuse protection and limits

Failed authentication costs the caller exponentially growing time, and nothing
before authentication costs the server more than one signature check.

**Backoff for failures**

- Failures are counted per source IP, and per /64 for IPv6. Every `403`
  counts.
- The first 3 failures are free. After that, each failure locks the address
  out for 2^(n-3) seconds, capped at one hour. A locked-out address gets `429`
  with `Retry-After`, before any header is parsed or any cryptography runs.
- A success clears the counter, and an address is forgotten after 24 hours
  without failures. The table holds at most 10,000 addresses and drops the one
  that failed longest ago.
- Key ids are not locked out: they are public, so locking one would let anyone
  shut out its owner. Ed25519 signatures cannot be guessed, so per-IP backoff
  plus a global cap of 50 signature checks per second is enough.
- Behind Traefik the client address is the last `X-Forwarded-For` entry,
  trusted only from loopback. Another proxy in front, such as a CDN, has to be
  listed in Traefik's `forwardedHeaders.trustedIPs`.

**Limits at the edge.** Traefik allows each client address 10 API requests
per second (bursts of 50) and 16 in flight, before the daemon sees them.
`router.api_allow` can restrict the API to a list of networks, so everyone
else gets `403` from Traefik.

**Other guards**

- At most 4 open tickets, at most 2 per key; more get `429`.
- One deploy, rollback or removal at a time, across the daemon and
  `mypaas server deploy`. Another upload gets `409 busy` at once, its ticket
  stays valid, and no connection waits in a queue.
- `tmp/` is emptied after every deploy, after a failure and at daemon start.

**Upload limits** (all in `[limits]` of `config.toml`)

| Limit | Default | Checked |
| --- | --- | --- |
| Compressed bundle | 100 MiB (`max_upload`) | at the ticket request and while streaming; the client warns above 50 MiB |
| Unpacked size | 200 MiB | while decompressing, so a zip bomb stops at the limit |
| Files | 40,000 | while unpacking |
| Path depth | 32 | while unpacking |
| Free disk | 1 GiB plus 4 times the bundle | at the ticket request |
| Build time | 15 minutes | around `docker build` |

The unpacker accepts regular files and directories only. It refuses absolute
paths, `..`, symlinks, hard links and device files, checks the names that GNU
long-name and PAX headers produce, and clears setuid, setgid and sticky bits.

## Describing a service

A service is one Dockerfile. `.gitignore` decides what is uploaded.

**Packing**

- The client applies the `.gitignore` files inside the project only. Global
  ignore files, `.git/info/exclude` and `.gitignore` files above the project
  are not read, so every machine packs the same bundle. `.git` is always left
  out, and symlinks are skipped with a warning.
- The bundle is a `tar.zst` with sorted entries, owner 0, mtime 0 and modes
  normalized to 0644 or 0755, so the same tree always has the same hash. The
  chosen Dockerfile is stored as `Dockerfile`, and a `.dockerignore` still
  applies to the build.
- `mypaas push --dry-run` lists the file count, the sizes and the ten largest
  files without uploading.

**Comment keys**

A line counts when it starts with `#` followed by `mypaas.`, with spaces or
tabs allowed around the `#`; `#--- mypaas.url=` switches a line off. Unknown
keys fail the deploy, and every value is validated before it reaches a docker
argument, a label or a path.

| Key | Meaning | Default |
| --- | --- | --- |
| `service` | Name and identity of the service; a deploy replaces the service of that name. Required. | |
| `url` | `http(s)://host[/path]`, repeatable. Without one the service has no public route. | |
| `port` | Port the process listens on in the container | `80` |
| `volume` | `name:/path[:ro]` or `/host/path:/path[:ro]`, repeatable | |
| `env` | `KEY=value`, or `KEY` alone to take the value from the service's secrets | |
| `scale` | `N` and `roll` or `safe`; `0` means one container | `0`, `roll` |
| `healthcheck` | `/path interval timeout`, for example `/health 10s 2s`; used with `scale` above 0 | |
| `maxcpu`, `maxmem` | Limits per container, within the server's maximums | `1`, `512m` |
| `network` | Shared network to join, created if missing, repeatable | |
| `readonly` | `true` mounts the root filesystem read-only with a tmpfs on `/tmp` | `false` |
| `cap_add` | Linux capabilities to add back; needs `admin` | |

The identity of a deployment is `mypaas.service`, not the directory. Two
Dockerfiles with different names in one repository, for example
`Dockerfile.staging` and `Dockerfile.prod`, are two independent services.

**Examples.** [examples/](../examples/README.md) holds one complete service per
pattern: static files (`safe`), a stateless app (`roll` with a healthcheck),
SQLite (stop first), staging and production from one project, a multi-stage
build, an internal service with a worker, and a shared Postgres. A unit test
parses every Dockerfile there, so they always match the parser.

## Isolation

### Networks

- Each service gets a private bridge network, `mypaas-<service>`. No other
  service can join it. The label `traefik.docker.network` tells Traefik to use
  the container's address on that network.
- `# mypaas.network=db` also puts the containers on the shared network
  `mypaas-net-db`, created on the first deploy that names it. On it, each
  service is reachable by its service name (a network alias), also when it is
  scaled.
- Joining is open to every service. A shared network is a convenience, not a
  security boundary: Postgres, for example, gets one role and one database per
  application, with the passwords in each service's secrets, and that is what
  keeps one application out of another's data.
- Containers are created, connected to their shared networks and then
  started, because `docker run` attaches only one network.
- Containers and builds cannot reach link-local addresses, which on cloud VMs
  include the metadata service that hands out the VM's credentials.
  `mypaas-firewall.service` rejects that traffic in Docker's `DOCKER-USER`
  chain and re-applies the rule whenever Docker restarts.
- The daemon listens on loopback, so containers cannot reach it through their
  bridge gateway.
- There are no host port mappings. Everything public goes through Traefik as
  HTTP or HTTPS; everything else is reached over a shared network.

### Container hardening

| Setting | Default | Override |
| --- | --- | --- |
| Capabilities | all dropped | `cap_add`, with `admin` |
| Privilege escalation | `no-new-privileges` | none |
| Privileged mode, host PID or network, devices, the Docker socket | never | none |
| Root filesystem | writable | `readonly=true` |
| Processes | 512 | `[defaults] pids` |
| CPU and memory | 1 CPU, 512 MiB, no swap | `maxcpu`, `maxmem` within `[maximums]` |
| Logs | `json-file`, 3 files of 10 MiB | `[defaults]` |
| User | the image's own; the deploy warns about root | `USER` in the Dockerfile |

### The Docker socket

- Only Traefik and the daemon use the socket. No service can mount it.
- Traefik gets it through `mypaas server socket-proxy`, which allows only `GET`
  and `HEAD` on `/_ping`, `/version`, `/events`, `/containers/json` and
  `/containers/<id>/json` (the calls Traefik 3.7.17 makes), and removes
  environment variables, and with them the secrets, from what it returns. The
  internet-facing process is therefore not root-equivalent. The proxy runs
  without network access and without capabilities.
- The daemon is root-equivalent by nature. Its systemd unit is sandboxed
  (`ProtectSystem=strict`, `PrivateTmp`, `NoNewPrivileges`, a system call
  filter, a small capability set) and may write only to `~/_mypaas`.
- Rootless Docker or Podman would close this gap and is left for later.

## Volumes and secrets

A service writes only to its own directory tree and reads only its own
secrets, unless the server config says otherwise.

**Volumes**

- A source without a leading `/` or `~` is a name: `data:/var/lib/app` maps to
  `~/_mypaas/volumes/<service>/data`, created if missing. A symlink planted
  there cannot point it elsewhere.
- An absolute host path needs the `admin` scope and an entry in
  `[volumes] allow` in `config.toml`. The server refuses allowlist entries
  that overlap `/`, `/run`, `/var/run`, `~/.ssh` or `~/_mypaas`. This is also
  the only way two services share a directory.
- Host paths are checked twice: normalized without touching the disk and
  compared component by component with the allowed root, then created,
  resolved with symlinks followed and checked again.
- Directories are created owned by root. If the image runs as a non-root
  `USER`, an empty named volume is handed to that user (`[volumes] chown`,
  on by default). A volume that already holds data owned by someone else is
  left alone, and the deploy prints the `chown -R` command to run.
- Anything a container writes outside its volumes is lost on redeploy.

**Secrets**

- Each service has its own file, `secrets/<service>.toml`, mode 0600, replaced
  atomically.
- `# mypaas.env=KEY` reads from the deploying service's own file only. A
  missing secret fails the deploy and names the key, never a value.
- `mypaas secret set|ls|rm` work through the API with the `secrets` scope. The
  API returns names only: a value cannot be read back.
- Values reach the container as environment variables at deploy time, so root
  on the server can see them with `docker inspect`. A changed secret takes
  effect at the next deploy or rollback.
- Values never appear in logs, progress lines or error messages.

## Routing with Traefik

Traefik is configured by two files that `server init` writes and by labels on
each container. The daemon never talks to Traefik directly. The image is
pinned to `traefik:v3.7.17` by digest and moves with mypaas releases; 3.7
needs Docker Engine API 1.40 or newer.

Traefik is the only process on a public port, so its container gets as little
as possible: all capabilities dropped except `NET_BIND_SERVICE`,
`no-new-privileges`, a read-only root filesystem with a small `/tmp`, 512 MiB
of memory and 512 processes.

**Static configuration**

- `traefik.toml`: entrypoints `web` (80, 60 s read timeout) and `web-secure`
  (443, `router.read_timeout` for uploads, 300 s by default), both with a 60 s
  idle timeout and 32 KiB headers, HTTP/2 with 100 streams per connection.
  Traefik has no separate header timeout, so the read timeout also limits slow
  headers. Then the Docker provider through the socket proxy with
  `exposedByDefault = false`, the file provider for `staticroutes.toml`, Let's
  Encrypt over the HTTP challenge with EC256 keys, and an access log with only
  service name, status, duration and start time.
- `staticroutes.toml`: the two daemon routers (priorities 100000 and up, with
  the rate, in-flight and optional allowlist middlewares) and an HTTP-to-HTTPS
  redirect for the admin domain, the middlewares `https-redirect` and
  `hsts-header`, and the TLS options `default` and `intermediate`: TLS 1.2 or
  newer, six AEAD ECDHE suites, and `sniStrict`, so a handshake for a name
  without a certificate fails instead of revealing Traefik's default one.
- `server init` asks for the admin domain and an optional Let's Encrypt
  account email, which must look like `local@domain.tld`.

**Labels for one `https://` URL**

```
traefik.enable=true
traefik.docker.network=mypaas-shop
traefik.http.services.shop-service.loadbalancer.server.port=8080
traefik.http.routers.shop-example-com-router.rule=Host(`shop.example.com`)
traefik.http.routers.shop-example-com-router.entrypoints=web-secure
traefik.http.routers.shop-example-com-router.tls.certresolver=default
traefik.http.routers.shop-example-com-router.tls.options=intermediate@file
traefik.http.routers.shop-example-com-router.middlewares=hsts-header@file
traefik.http.routers.shop-example-com-https-redirect.rule=Host(`shop.example.com`)
traefik.http.routers.shop-example-com-https-redirect.entrypoints=web
traefik.http.routers.shop-example-com-https-redirect.middlewares=https-redirect@file
```

- A path in the URL adds `&& PathPrefix(...)` to the rule. An `http://` URL gets
  only a `web` router, with no TLS and no redirect.
- With `scale` above 0 and a `healthcheck`, Traefik's own health check labels
  keep it from sending traffic to a replica that does not answer.
- Router names come from host and path with every other character turned into
  a dash. A URL another service already routes fails the deploy and names that
  service. URL paths are at most 128 characters.
- URLs on the admin domain are refused, even for `admin`: a route there could
  intercept API requests, tickets and secret values.
- Only mypaas sets these labels. Docker copies an image's `LABEL`s onto its
  containers, so the deploy refuses an image with any label starting with
  `traefik.` or `mypaas.`; otherwise a Dockerfile could route any domain to
  itself.
- A service without `url` gets no Traefik labels at all.

## Deploys and rollback

The image is built first, so a failed build changes nothing. Then the
containers are replaced by one of three strategies, and any failure after that
brings the old containers back.

| Mode | Set by | Downtime | What happens |
| --- | --- | --- | --- |
| Single | no `scale`, or `scale=0` | a few seconds | The old container stops, then the new one starts. |
| Safe | `scale=N safe` | a few seconds | All old containers stop, then N new ones start. Clients never see old and new versions mixed. |
| Roll | `scale=N roll` | none | One new container starts and must be ready before one old one stops, N times. |

**Single writers.** Single mode stops before it starts, so a SQLite file or any
other single-writer store is never opened by two processes. With `scale` above
0 the replicas share their volumes, and the deploy warns if a writable one is
mounted.

**Steps of a deploy**

1. Build `mypaas-<service>:<deploy id>`, with `--pull` when a `FROM` line has
   no tag or `:latest`. Refuse the image if it carries `traefik.` or `mypaas.`
   labels, and read its id.
2. Check the spec against the caller's scope and the server config, resolve
   volumes and secrets, and write the env file (mode 0600, removed afterwards).
3. Rename the running containers to `<name>.old.<unix time>.<n>`. If a rename
   fails, the ones already renamed get their names back, and nothing was
   stopped.
4. Start and stop containers by the mode. A new container is ready when its
   healthcheck answers 2xx or 3xx within 60 seconds. The daemon probes the
   container's address itself, so the image needs no `curl`. Without a
   healthcheck, the container must still be running, and not restarting,
   after 5 seconds.
5. On success: remove the old containers, move the tag `previous` to the old
   image and `current` to the new one, save the spec, and prune only dangling
   images labelled with this service.
6. On failure: remove the new containers, rename the old ones back, start
   them, and report the error with the last log lines of the failed container.

**Rollback**

- `mypaas rollback <service>` redeploys the `previous` image with the spec it
  was deployed with; `services/<service>/` keeps both, since the Dockerfile
  comments are not part of the image.
- One previous version is kept, so a second rollback in a row fails.
- Specs store secret names, not values: a rollback uses the secrets as they
  are now.

**Recovery.** If the daemon dies during a deploy, it finds services with
`.old.` containers and no running new ones at startup, and brings the old
containers back. It touches only services that have a stored spec.

## History and the dashboard

The daemon keeps per-service history in hourly buckets in
`stats/<service>.json`, for `[stats] retention_days` (90 by default).
`mypaas stats` and `mypaas dashboard` read it through
`GET /v1/services/{name}/stats`, which merges buckets per hour (up to 14 days)
or per day.

**Traffic** comes from Traefik's JSON access log. The log keeps only the
service name, the status, the duration and the start time; client addresses,
paths and headers are dropped before they reach the disk. Every 10 seconds the
daemon reads new lines into buckets with the request count, status classes
and a log-scale latency histogram (bins about 19% wide) for p50 and p95. The
read position is saved together with the counts, and the log is rotated at
32 MiB, so no line is counted twice or lost. `[stats] enabled = false` turns
the access log off.

**Uptime and resources** come from the containers. Once a minute the daemon
samples every deployed service: how many replicas are running out of how
many are wanted, and their memory and CPU from `docker stats`. Each bucket
keeps the number of samples, the samples with at least one replica running
(uptime), the samples with all replicas running (full uptime), and the sum and
maximum of memory and the sum of CPU. Sampling needs no access log, so it
always runs. An hour without samples, for example while the server was down,
shows as no data rather than as downtime, because nothing can be said about
it.

**The dashboard** renders in the terminal, from the same JSON any script can
read:

- `mypaas dashboard` shows one row per service: health, replicas, uptime,
  requests and memory peak over the window, with a request sparkline and an
  uptime strip.
- `mypaas dashboard <service>` draws bar charts of requests and memory peak and
  an uptime strip, with time axes. Columns are grouped when the terminal is
  narrower than the number of buckets: requests are averaged, memory takes the
  maximum, and uptime takes the worst value, so a short outage stays visible.
- `--since` picks the window (hourly up to 14 days, daily beyond), `--watch`
  redraws full-screen every 10 seconds, and `--json` prints the data instead.

## Client CLI

**Remotes.** `mypaas remote add|ls|rm` keep servers in
`~/.config/mypaas/remotes.toml`. `-r` takes a remote name or a domain
(`paas.example.com` or `https://paas.example.com`); remote names cannot contain
dots, so the two never clash. With a single remote it is the default, and
`MYPAAS_REMOTE` overrides it. Plain HTTP is accepted only for a daemon on
loopback, for development.

| Command | Scope | What it does |
| --- | --- | --- |
| `push [PATH]` | `deploy` | Pack, upload with a progress bar, build and deploy; streams the log. Retries on `409 busy` while the ticket is valid. `--dry-run` lists files and sizes. |
| `ls` | `read` | One row per service: replicas, health, age, requests in the last hour, CPU, memory, URLs |
| `status <service>` | `read` | Containers, URLs, networks, volumes, env names, last and previous deploy |
| `logs <service>` | `logs` | `-f`, `--tail N`, `--since 10m`, `--container N`, merged across replicas |
| `top` | `read` | Full-screen live view, refreshed every 2 seconds; `q` quits |
| `stats <service>` | `read` | Requests, status classes and p50/p95 latency per hour or day, `--since 7d` |
| `dashboard [service]` | `read` | Uptime, memory and request charts; `--since`, `--watch` |
| `rollback <service>` | `deploy` | Redeploy the previous version |
| `rm <service>` | `rm` | Remove containers, network, stored spec and history; volumes and secrets stay |
| `secret set\|ls\|rm <service> [KEY]` | `secrets` | The value comes from stdin or a hidden prompt |
| `key init\|gen\|show` | | Create or select a key, print a passwordless CI key, print the public line |
| `doctor` | `read` | Check the key, TLS, the clock (warning above 10 s, failure above 25 s) and a signed request |

**Output.** Tables with colour on a terminal; `--json` prints one JSON
document, or one object per line for streams; `-q` prints only errors and
results; `--no-color` and `NO_COLOR` turn colour off.

**Exit codes.** `0` success, `1` error, `2` usage, `3` denied or locked out,
`4` deploy failed on the server, so CI can tell a bad key from a bad build. An
encrypted key's passphrase is asked once per command.

## Server CLI

Server commands run as root and change local state only. Everything that
grants trust (keys, the volume allowlist) is changed here and never through
the API.

| Command | What it does |
| --- | --- |
| `server init [--domain D] [--email E] [--key FILE]` | Creates `~/_mypaas`, `config.toml`, `authorized_keys`, `acme.json` (0600), the Traefik files and the systemd units, and starts `mypaas-firewall`. `--key` adds the first key with `admin`, because without one nobody can log in. Safe to run again: keys, certificates and secrets stay. |
| `server restart all\|daemon\|router` | `daemon` restarts `mypaasd`; `router` rewrites the Traefik files, restarts the socket proxy, pulls the pinned image and replaces the Traefik container; `all` does both |
| `server deploy DIR` | Deploys a local directory as admin through the same code as the API, for debugging |
| `server key add FILE\|- --name N --scope S… [--expires DATE]` | Validates the key and appends it atomically |
| `server key ls`, `server key rm NAME` | List keys; remove one, effective within 5 seconds |
| `server schedule-reboot [WHEN]` | A systemd timer that reboots the server, default `Sun 06:00:00` |
| `server version` | Version of mypaas and of the pinned Traefik |

`config.toml` is read again at every deploy, so changes to `[limits]`,
`[defaults]`, `[maximums]` and `[volumes]` need no restart. Changes to
`[router]` and `[stats]` need `mypaas server restart all`.

## Decisions log

Each entry: what was decided, and why.

**Transport and trust**

- **HTTPS with signatures instead of SSH.** Deploys work from any CI runner
  without shell access to the server, and the server has no SSH-based
  attack surface of its own.
- **Ed25519 only, in OpenSSH format.** One algorithm keeps the code small;
  OpenSSH format lets people reuse the keys they have. RSA is not supported.
- **Keys are managed on the server only.** A leaked key, even an admin key,
  cannot mint more keys or extend its own scope.
- **Every auth failure looks the same.** A different answer for an unknown key
  would tell an attacker which key ids exist.
- **Backoff per IP, not per key.** Key ids are public; locking a key would let
  anyone lock out its owner.
- **The host is signed.** Without it, a server that a user's key also trusts
  could replay that user's requests to this one.
- **Limits at two layers.** Traefik limits requests per address before
  anything reaches the daemon; the daemon limits requests in flight per
  address again, because its failure counter only moves when a request ends.
- **No `ssh-agent` signing.** The client depends on nothing from SSH; keys come
  from a file or `MYPAAS_PRIVATE_KEY`.

**Uploads**

- **HTTP/2 from the client.** Only a full-duplex connection lets an early
  refusal reach the client before the upload finishes; otherwise a `409 busy`
  would cost a full upload per retry.
- **Tickets instead of signing the upload.** Checking a signature over the body
  would mean reading the body first. A ticket lets the server decide from the
  headers alone.
- **256-bit tickets, looked up by hash.** Guessing one of at most 4 open
  tickets out of 2^256 within 60 seconds is out of reach, so 512 bits would
  only lengthen the header. The table is keyed by the SHA-256 of the token, so
  the lookup never compares secret bytes: any timing difference depends on a
  hash the attacker cannot steer, and a memory dump holds no usable token.
- **The server's limit is the only limit.** The client used to refuse bundles
  over a built-in size; now `max_upload` on the server decides, and the client
  only warns and shows the largest files.
- **Packing ignores machine-specific rules.** Only the project's own
  `.gitignore` files apply, so a bundle has the same hash on every machine.

**Services**

- **The Dockerfile is the only configuration.** No `mypaas.toml` to keep in
  sync; unknown keys fail loudly instead of being ignored.
- **HTTP and HTTPS only; no `publish`.** Host port mappings and TCP or UDP
  routing added parsing, privilege rules and port conflicts for a rare case.
  Non-HTTP services, such as databases, are reached over shared networks.
- **Shared networks are open.** Any service joins a network by naming it.
  Membership lists in `config.toml` made every new consumer of a database a
  server-side change. The cost is that a network is no security boundary, so
  services on one must authenticate their clients.
- **Private networks stay private.** A shared network is always named
  `mypaas-net-<name>`, so no `mypaas.network` value can reach another
  service's own `mypaas-<service>` network.
- **Secrets per service.** One shared secrets table would let any deploy read
  every secret by naming it in `mypaas.env`.
- **Path checks by component, twice.** Comparing paths as strings lets
  `/root2` pass as being under `/root`; checking again after resolving
  symlinks catches links planted inside an allowed root.
- **Empty volumes are chowned, full ones are not.** Handing a new volume to the
  image's user makes non-root images work out of the box; a recursive chown of
  existing data is left to a person.

**Deploys**

- **Build before touching anything.** A failed build never causes downtime.
- **Stop first by default.** `scale=0` never runs two versions at once, which
  is what single-writer stores such as SQLite need.
- **Everything that can fail runs before the swap.** The image id is read and
  the env file written before any container is renamed, so a failure there
  leaves the service as it was, and the stored deploy record always matches
  what runs.
- **The daemon probes health itself.** Images do not need `curl`, and a
  container counts as ready only once it answers.
- **Prune only our own objects.** Only images and containers labelled
  `mypaas.service` are ever removed, never other things on the host.
- **One deploy at a time, no queue.** A second upload gets `409` at once and
  the client retries; no connection is held open waiting.

**Operations**

- **The terminal is the dashboard.** `ls`, `top`, `stats` and `dashboard` cover
  what a web UI would, without a second public surface to secure.
- **History in hourly buckets, no time-series database.** One JSON file per
  service with a fixed retention is enough for one server and needs no extra
  process.
- **Missing samples are not downtime.** When the daemon or the server was down,
  nothing was measured, and the charts say so rather than guessing.
- **Traefik behind a read-only socket, with nothing else.** The
  internet-facing process cannot start containers even if it is compromised,
  and it runs without capabilities beyond binding its ports.
- **Image labels are not trusted.** Routing is decided by mypaas from the
  Dockerfile comments alone, never by labels the image brings along.
- **Strict SNI.** Scanners that connect by address learn nothing about the
  software or the hosted names.
- **Metadata blocked for containers.** On a cloud VM the metadata service is
  the shortest path from an SSRF bug to the cloud account.
- **Pinned Traefik, by digest.** Upgrades are tested with the end-to-end suite
  and move with mypaas releases; a re-pushed tag changes nothing.
- **A .deb that ships only the binary.** The units depend on the domain and
  the config, which the package cannot know, so `server init` keeps writing
  them. The package restarts the daemon and the proxy on upgrade and stops
  them on removal, and leaves `~/_mypaas` and the containers alone.
- **Releases are built by CI from a tag, with provenance.** The build job,
  which runs the project's code, cannot write to the repository; a separate
  job publishes. Each file has a SHA-256 and a GitHub build attestation, and
  every third-party action is pinned by commit.

## Open questions

- **Privileges without an admin key.** `cap_add` and host-path volumes need
  `admin`, which a CI job should not hold, so a service that genuinely needs,
  say, `NET_BIND_SERVICE` cannot be deployed from CI. A proposal is a
  per-service allowance in `config.toml`, edited on the server like every
  other trust decision:

  ```toml
  [privileges."media-*"]
  cap_add = ["NET_BIND_SERVICE"]
  volumes = ["/srv/media"]
  ```

  A key whose deploy scope covers the service could then use exactly these
  settings and nothing more. Not implemented.
- **Rootless Docker or Podman** would make the daemon itself
  non-root-equivalent.
