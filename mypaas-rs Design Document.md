# mypaas-rs: Design Document

Nov 3, 2026 · @Lorant Meszlenyi

mypaas-rs is a single-server PaaS on Docker and Traefik that exposes one small HTTPS API and nothing else. It keeps the original workflow (`mypaas push`, `# mypaas.*` comments in the Dockerfile) and drops the web dashboard, the stats server, `mypaas.toml`, RSA and any dependency on SSH.

## Goals and threat model

**Goals**

- Same workflow as mypaas: one Dockerfile per service, configured by comments, deployed with `mypaas push`.
- Minimal server surface: one daemon API behind Traefik, no dashboard, no extra listeners.
- HTTP-only transport, so deploys work from any laptop or CI runner without SSH access.
- An unauthenticated caller can never make the server read, store or unpack large data.
- Services are isolated from each other unless sharing is requested explicitly (for example one Postgres for many services).
- The client CLI replaces the dashboard for status, logs and traffic stats.

**Non-goals:** multi-server or cluster setups, a web UI, building images anywhere but on the server, Kubernetes.

**Threat model**

| Actor | Can do | Cannot do |
| --- | --- | --- |
| Internet client without a key | Reach port 443 and send small requests that are rejected cheaply and slowed down exponentially | Upload data, read status, learn whether a key id exists |
| Holder of a scoped deploy key | Deploy services matching its scope; the containers run with dropped capabilities on their own network | Touch other services, mount host paths outside the service directory, run privileged containers |
| Holder of an admin key | Relax restrictions (host paths, extra capabilities, shared networks) | Add or remove keys remotely (keys are edited on the server only) |

A build executes arbitrary Dockerfile `RUN` steps inside Docker, so a deploy key is as trusted as a CI credential. Container-escape bugs in Docker or the kernel and a compromised host are out of scope.

## Architecture

mypaas-rs is one Rust binary that acts as client CLI, server CLI and daemon, running next to Traefik and Docker on a single VM. Traefik is the only process that listens on a public port.

&#91;embedded content: architecture · 1 public listener, 3 services, 1 shared network\]

The CLI and visitors both enter through Traefik. Traefik forwards only `/v1` to the daemon, and each service has its own network, joined to another service only where a shared network such as `db` is declared.

**One binary, three roles**

- `mypaas …` is the client CLI, used on a laptop or in CI.
- `mypaas server …` is the server CLI. It runs as root and changes local state only.
- `mypaas server daemon` is the HTTP API behind Traefik, started by systemd.
- The binary is statically linked, so the server needs no language runtime. Docker and the Traefik image are the only runtime dependencies.

**What lives where**

```
~/_mypaas/
  config.toml          admin domain, email, defaults, shared networks, volume allowlist
  authorized_keys      public keys with scopes, edited on the server only
  traefik.toml  staticroutes.toml  acme.json
  secrets/<service>.toml
  services/<service>/current.json  previous.json
  volumes/<service>/<name>/
  tmp/                 uploads and unpacked bundles, cleaned after every deploy
```

There is no database. What the daemon knows about running services comes from Docker labels and these files.

## Auth and deploy protocol

Every request is authenticated before any bulk data is read: small requests carry an Ed25519 signature, and uploads carry a short-lived single-use ticket in a header that the client obtains first with a signed request.

**Keys**

- Ed25519 only. Keys are OpenSSH-format files, so an existing `~/.ssh/id_ed25519` works. `mypaas key init` can also create a new key, encrypted with a passphrase. In CI the key comes from `MYPAAS_PRIVATE_KEY`.
- The server reads `~/_mypaas/authorized_keys`, one line per key: `ssh-ed25519 AAAA… name scope=deploy:shop-*,read expires=2027-01-01`. It is edited on the server only (`mypaas server key add`), never through the API.
- The key id is the SHA-256 fingerprint of the public key.
- Scopes: `read`, `logs`, `deploy:<glob>`, `secrets:<glob>`, `rm:<glob>`, `admin`. An unknown or expired key gets the same answer as a wrong signature.

**Signed request (all small requests)**

The client signs this string with Ed25519: `mypaas/v1`, method, path and query, timestamp, nonce, SHA-256 of the body, each on its own line. It sends the result in the headers `X-Mypaas-Key`, `X-Mypaas-Time`, `X-Mypaas-Nonce` and `X-Mypaas-Sig`. Bodies are limited to 64 KiB. The server accepts timestamps within 30 seconds and remembers nonces for 60 seconds. Error responses include the server time, so the client can print a clock-skew hint.

**Deploy flow**

1. The client packs the project as `tar.zst` and computes its size and SHA-256.
2. It sends a signed `POST /v1/deploy-tickets` with `{service, size, sha256}`. The server checks signature, nonce, scope `deploy:<service>`, the declared size against the limit, the rate limits and the cap on open tickets.
3. The server returns a ticket: 256 random bits, valid for 60 seconds, usable once, bound to key id, service, size and hash. Tickets live in memory only.
4. The client sends `PUT /v1/deploy` with `Authorization: Bearer <ticket>` and a `Content-Length` equal to the declared size.
5. The server decides from the headers alone. An unknown, expired or used ticket, or a different `Content-Length`, is rejected without reading a byte. Otherwise the ticket is consumed and the body is streamed to a temp file while bytes are counted and hashed. Reading stops as soon as the count exceeds the declared size.
6. After the last byte the hash must match the signed one. On a mismatch the file is deleted and nothing is unpacked.
7. The server unpacks the bundle and checks that `mypaas.service` in the Dockerfile equals the ticket's service, so lying in step 2 cannot bypass the scope. Then it deploys and streams progress lines back in the response.

The ticket exists so the server never has to read a whole upload to find out who sent it. It is a bearer token, but it is bound to one exact bundle, works once, expires after 60 seconds and travels only over TLS, so a stolen ticket cannot be used to upload anything else.

## Daemon API and attack surface

The daemon listens on `127.0.0.1:88` only and Traefik forwards a single path prefix, `/v1`, on the admin domain. Every endpoint requires a signature or a ticket; there is no unauthenticated endpoint, no HTML and no banner.

| Endpoint | Auth | Body | Returns |
| --- | --- | --- | --- |
| `POST /v1/deploy-tickets` | signed, `deploy:<service>` | JSON `{service, size, sha256}` | ticket |
| `PUT /v1/deploy` | ticket | `tar.zst` stream | progress lines, streamed |
| `GET /v1/services` | signed, `read` | none | services, replicas, health, image age, CPU and memory |
| `GET /v1/services/{name}` | signed, `read` | none | containers, URLs, networks, volumes, last deploy |
| `GET /v1/services/{name}/logs` | signed, `logs` | none | log stream (`follow`, `tail`) |
| `GET /v1/services/{name}/stats` | signed, `read` | none | traffic aggregates (phase 4) |
| `POST /v1/services/{name}/rollback` | signed, `deploy:<name>` | none | progress lines |
| `DELETE /v1/services/{name}` | signed, `rm:<name>` | none | result |
| `GET /v1/services/{name}/secrets` | signed, `secrets:<glob>` | none | secret names only |
| `PUT` and `DELETE /v1/services/{name}/secrets/{key}` | signed, `secrets:<glob>` | value, max 16 KiB | result |

**Rules that keep the surface small**

- Traefik uses exact routers, `Path(/v1/deploy)` for the upload and `PathPrefix(/v1)` for the rest. A prefix match on `/v1/deploy` would also catch `/v1/deploy-tickets`.
- No Traefik buffering middleware on this route: it reads the whole request body before forwarding, which would defeat the header-first check. The daemon enforces all limits itself.
- Every body needs a `Content-Length`; chunked uploads are refused. Bodies above 64 KiB are refused on every route except `PUT /v1/deploy`, where the limit is the ticket's declared size.
- HTTP/1.1 only, headers at most 8 KiB, at most 64 concurrent connections. The header read times out after 10 seconds, an upload after 15 seconds without progress or 10 minutes in total.
- Unknown paths return the same 404 for everyone. Failed authentication always returns the same 403 `denied`, whether the key is unknown, expired, out of scope or the signature is wrong.
- The log records key id, endpoint and outcome. It never records tickets or secret values.

## Abuse protection

Failed authentication costs an attacker time that grows exponentially, and no step before authentication costs the server more than one signature check.

**Exponential backoff for failures**

- Failures are counted per source IP (IPv6 per /64). Every 403 counts: unknown key, expired key, out-of-scope request, bad signature.
- The first 3 failures cost nothing. After that the lockout is 2^(n-3) seconds, capped at one hour. During a lockout the daemon answers `429` with `Retry-After` and does no cryptographic work.
- The lockout check runs first, before any header is parsed. A success from that IP clears its counter, and entries expire after 24 hours without a failure. The table holds at most 10,000 IPs and drops the least recently used.
- The key id is deliberately not locked out. A key id is public, so locking it would let an attacker shut out its owner. Ed25519 signatures cannot be guessed, so the per-IP backoff plus a global cap of 50 signature checks per second is enough.
- Behind Traefik the client IP is the last entry of `X-Forwarded-For`, trusted only from `127.0.0.1`. With another proxy in front (a CDN, for example), Traefik's `forwardedHeaders.trustedIPs` must name it.

**Other guards**

- Nonces are remembered for 60 seconds (at most 10,000), so a captured request cannot be replayed.
- At most 4 tickets are open at once and at most 2 per key. More requests get `429`.
- One deploy runs at a time. A second `PUT /v1/deploy` is answered at once with `409 busy`, the ticket stays valid, and no connection waits in a queue.
- Temporary files live in `~/_mypaas/tmp` and are removed after each deploy, after a failure and at daemon start.

**Upload and unpack limits**

| Limit | Default | Enforced |
| --- | --- | --- |
| Compressed bundle | 50 MiB (`max_upload`) | client before the upload, ticket request, byte count while streaming |
| Unpacked size | 200 MiB | counted while decompressing, so a bomb stops at the limit |
| Files per bundle | 20,000 | while unpacking |
| Path depth | 32 | while unpacking |
| Free disk | 1 GiB plus 4 times the declared size | at the ticket request |
| Build time | 15 minutes | around `docker build` |

The unpacker accepts regular files and directories only. It rejects absolute paths, `..`, symlinks, hardlinks and device files, and it clears setuid and setgid bits. The client warns above 20 MiB and names the largest files, since a stray binary is almost always a missing `.gitignore` entry.

## Packaging and Dockerfile configuration

A service is one Dockerfile. Its `# mypaas.*` comments are the only configuration, and `.gitignore` decides what gets uploaded, so there is no `mypaas.toml`.

**Packing**

- The client walks the project with the rules of the `.gitignore` files inside it. Global ignore files and `.git/info/exclude` are not read, so every machine packs the same bundle. `.git` is always left out. A packed `.dockerignore` still applies to the build on the server.
- The bundle is a `tar.zst` with owner and timestamps normalized, so the same tree gives the same hash. The chosen Dockerfile is stored as `Dockerfile`.
- `mypaas push --dry-run` prints the file count, the size and the ten largest files without uploading.

**Comment keys**

A line counts when it starts with `#` followed by `mypaas.`, with spaces or tabs allowed around the `#`. Writing `#--- mypaas.url=` switches a line off. Unknown keys fail the deploy, and every value is validated.

| Key | Meaning | Default |
| --- | --- | --- |
| `service` | Name and identity of the service. A deploy replaces the service with the same name. Required. | none |
| `url` | `http(s)://host[/path]`, repeatable. Without a URL the service is internal only. | none |
| `port` | Port the process listens on inside the container | 80 |
| `publish` | Host port mapping like `docker --publish`, repeatable. The bind address defaults to `127.0.0.1`; a public bind needs the `admin` scope. | none |
| `volume` | `name_or_path:/container/path`, repeatable (see Volumes and secrets) | none |
| `env` | `KEY=value`, or just `KEY` to take the value from the server's secrets | none |
| `scale` | `N` plus `roll` or `safe`. `0` means a single container. | 0, `roll` |
| `healthcheck` | `/path interval timeout`, for example `/health 10s 2s`. Only used with `scale` above 0. | none |
| `maxcpu`, `maxmem` | Limits per container, within the server maximums | 1 CPU, 512m |
| `network` | Name of a shared network to join, repeatable (see Container isolation) | none |
| `readonly` | `true` mounts the root filesystem read-only with a tmpfs on `/tmp` | false |
| `cap_add` | Linux capabilities to add back; needs the `admin` scope | none |

**Several instances of one project**

The identity of a deployment is `mypaas.service`, not the directory. Two Dockerfiles in one repository with different service names are two independent deployments, for example `mypaas push Dockerfile.dev` and `mypaas push Dockerfile.prod`. They can go to the same server or to different ones, and they share nothing unless a network is shared on purpose.

## Container isolation and networking

Every service runs on its own Docker network and can reach no other service, unless the server's admin has declared a shared network and listed the service as a member. This is how one Postgres can serve many services without opening everything to everyone.

**Networks**

- Each service gets a private bridge network `mypaas-<service>`. Traefik reaches it through a `traefik.docker.network=mypaas-<service>` label, which tells Traefik which of the container's addresses to use.
- A shared network `mypaas-net-<name>` is declared on the server in `~/_mypaas/config.toml`, never by a deploy:

```toml
[networks.db]
members = ["postgres", "shop-*", "blog"]
```

- A service joins with `# mypaas.network=db`. The deploy is refused if its name does not match a member pattern, so a deploy key for another service cannot reach the database by writing the same comment.
- On a shared network the container is also reachable by its service name (a network alias), so `postgres` resolves for every member even when the service is scaled.
- The container is created, connected to its networks and then started, because `docker run` attaches only one network.
- The daemon listens on loopback only, so containers cannot reach it through the bridge gateway.
- A shared network removes the network barrier only. Inside Postgres, each application should still get its own database and a role that may connect to that database alone. The role passwords live in `secrets`.

**Container hardening defaults**

| Setting | Default | Override |
| --- | --- | --- |
| Capabilities | all dropped | `cap_add`, `admin` scope |
| Privilege escalation | `no-new-privileges` | none |
| Privileged mode, host PID or network, devices, Docker socket | never allowed | none |
| Root filesystem | writable | `readonly=true` |
| Process limit | 512 | server config |
| CPU and memory | 1 CPU, 512m | `maxcpu`, `maxmem` within the server maximums |
| Logs | `json-file`, 3 files of 10 MiB | server config |
| User | the image's own; the deploy prints a warning if it is root | `USER` in the Dockerfile |

**The Docker socket**

- Only Traefik and the daemon touch the socket, and no service can mount it.
- Traefik should reach it through a socket proxy that allows read-only calls for containers, networks, events and version, so an internet-facing process is not root-equivalent. The exact list of calls needs to be verified against the Traefik version in phase 3.
- The daemon stays root-equivalent by nature. It runs as a systemd unit with `NoNewPrivileges`, `PrivateTmp` and `ProtectSystem=strict`, with write access only to `~/_mypaas` and the socket.
- Rootless Docker or Podman would remove this last gap; it is out of scope for the first version.

## Volumes and secrets

A service can write only to its own directory tree and can read only its own secrets. Anything else needs an explicit entry in the server config.

**Volumes**

- Syntax: `source:/container/path[:ro]`. A source without a leading `/` or `~` is a name, for example `data:/var/lib/app`, and maps to `~/_mypaas/volumes/<service>/data`. The directory is created if it is missing.
- An absolute host path needs two things: the `admin` scope and an entry in the server's allowlist (`[volumes] allow = ["/srv/media"]` in `config.toml`). The server refuses to start with an allowlist entry that contains `/`, `~/.ssh`, `~/_mypaas` or `/var/run`. This is also the only way two services can share a directory.
- Path check: the path is normalized without touching the disk (`.` and `..` resolved), compared component by component against the allowed root, created, then resolved again with symlinks followed and checked a second time. The original compared strings, so `/root2` passed as being under `/root`.
- Anything a container writes outside its volumes is lost on redeploy and reboot.
- Backup scope is one tree: `~/_mypaas` (volumes, config, secrets, keys).
- Directories are created owned by root. A service whose image runs as a non-root user needs a writable directory for that user; how to handle this is an open question for phase 3.

**Secrets**

- Secrets are stored per service in `~/_mypaas/secrets/<service>.toml` with mode 0600, written to a temp file and renamed into place. The original kept one shared `[env]` table, so a deploy for one service could read another service's secrets by naming them in `mypaas.env`.
- `# mypaas.env=KEY` reads from the deploying service's own file only. A missing key fails the deploy and names the key, never a value.
- `mypaas secret set`, `ls` and `rm` work through the API under the `secrets:<glob>` scope. The API returns names only; a value cannot be read back after it is set.
- Values reach the container as environment variables at deploy time, so they are visible to root through `docker inspect`. A changed secret takes effect at the next deploy or rollback.
- Secret values never appear in logs, progress lines or error messages.

## Traefik labels and non-HTTP services

Traefik is configured by two static files written at `server init` and by labels the daemon puts on each container. The daemon never talks to Traefik directly, and the version is pinned (3.6.1, as in the original) and moves with the mypaas release.

**Static configuration**

- `traefik.toml`: entrypoints `web` (80) and `web-secure` (443), the Docker provider with `exposedByDefault = false` and `watch = true`, the file provider for `staticroutes.toml`, Let's Encrypt over the HTTP challenge with EC256 keys, log level WARN. The dashboard, basic auth and InfluxDB metrics are gone.
- `staticroutes.toml`: the daemon routers from the API section, the middlewares `https-redirect` and `hsts-header`, and the TLS options `intermediate` (TLS 1.2 or newer, six AEAD ECDHE suites, as in the original).

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

- A path in the URL adds `&& PathPrefix(...)` to the rule. An `http://` URL gets only the `web` router, with no TLS and no redirect.
- With `scale` above 0 and a `healthcheck`, three more labels (`...healthCheck.path`, `.interval`, `.timeout`) keep Traefik from using a container before it answers.
- Router names come from host and path with every other character turned into a dash. If another service already uses the same rule, the deploy fails and names that service.
- Every container also carries `mypaas.service=<name>` and `mypaas.spec=<hash>`, so `mypaas ls` can read the state from Docker without a database.

**Services that are not HTTP**

- A service without `url` gets no Traefik labels at all. It is reachable only on the networks it joined, by service name. A database is therefore reached through a shared network, never through Traefik.
- To reach it from outside the server, use `# mypaas.publish`, for example `127.0.0.1:5432:5432`, or `8125:8125/udp` for UDP. The bind address defaults to loopback, and a public bind needs the `admin` scope.
- Traefik routes HTTP and HTTPS only in the first version, as in the original. TCP and UDP routing through Traefik needs extra entrypoints in `traefik.toml` and a router restart whenever they change, so it is deferred.

## Deploy strategies and rollback

A deploy builds the new image first, so a failed build changes nothing. Only then are containers replaced, by one of three strategies, and any failure after that puts the old containers back.

| Mode | Set by | Downtime | What happens |
| --- | --- | --- | --- |
| Single | `scale` absent or 0 | a few seconds | The old container stops, then the new one starts. Two processes never write to the same volume. |
| Safe | `scale N safe` | a few seconds | All old containers stop, then N new ones start. Clients never see a mix of old and new assets. |
| Roll | `scale N roll` | none | One new container starts and must be healthy before one old container stops; repeat N times. |

**Steps of every deploy**

1. Build the image `mypaas-<service>:<deploy id>`, with `--pull` when the `FROM` line uses `:latest`.
2. Rename the running containers to `<name>.old.<unix time>.<n>`.
3. Start and stop containers according to the mode.
4. On success remove the old containers, move the tag `previous` to the old image and `current` to the new one, and prune only objects labelled `mypaas.service`. The original pruned every stopped container and dangling image on the host, including ones that were not its own.
5. On failure remove the new containers, rename the old ones back, start them and report the error. The tags stay as they were.

**Health in roll mode**

The original assumed a container boots within 5 seconds. Here the daemon probes `http://<container address>:<port><path>` itself, using the path from `mypaas.healthcheck`, and stops an old container only after the new one has answered 2xx or 3xx (timeout 60 seconds, then the deploy fails and rolls back). Without a healthcheck it waits a fixed 5 seconds, as the original does. No `curl` is needed inside the image.

**Rollback**

- `mypaas rollback <service>` redeploys the `previous` image with the spec it was deployed with. The Dockerfile comments are not part of the image, so the daemon stores the parsed spec for `current` and `previous` in `~/_mypaas/services/<service>/`.
- Exactly one previous version is kept. After a rollback, no furtherrollback can be done, so rolling back twice returns and error.
- The spec stores secret names, not values; a rollback uses the secrets as they are at that moment.
- If the daemon dies in the middle of a deploy, it checks at startup for services that have renamed `.old.` containers and no running new ones, renames them back and starts them.

## Client CLI (replaces the dashboard)

The client shows everything the original dashboard showed (service status, logs, traffic) from any machine that has a key, and every command can print JSON for scripts.

**Remotes**

- `mypaas remote add <name> <admin-domain>`, `remote ls` and `remote rm` manage servers in `~/.config/mypaas/remotes.toml`.
- `-r <name>` picks a remote. With a single remote it is the default, and `MYPAAS_REMOTE` overrides it. A bare `https://domain` also works in place of a name.

**Commands**

| Command | Scope | What it does |
| --- | --- | --- |
| `push [PATH]` | `deploy` | `PATH` is a project directory or a Dockerfile. Shows the bundle size and an upload progress bar, then the live deploy log. `--dry-run` lists files and sizes only. |
| `ls` | `read` | One row per service: replicas running of wanted, health, image age, CPU, memory, URLs |
| `status <service>` | `read` | Containers, URLs, networks, volumes, last deploy (time and key name), whether a previous version exists |
| `logs <service>` | `logs` | `-f`, `--tail N`, `--since 10m`, `--container N` |
| `top` | `read` | Full-screen live view refreshed every 2 seconds: CPU, memory, replicas and health per service. `q` quits. |
| `stats <service>` | `read` | Requests, status classes and p50 and p95 latency per hour or day with a sparkline, `--since 7d` (phase 4) |
| `rollback <service>` | `deploy` | Redeploys the previous version |
| `rm <service>` | `rm` | Removes containers, networks and the service's stored spec; volumes stay |
| `secret set\|ls\|rm <service> [KEY]` | `secrets` | The value comes from stdin or a hidden prompt and is never echoed |
| `key init\|gen\|show` | none | `init` creates a new Ed25519 key with a passphrase or selects an existing OpenSSH key. `gen` prints a passwordless key for `MYPAAS_PRIVATE_KEY` in CI. `show` prints the public line for `authorized_keys`. |
| `doctor` | `read` | Checks the key file, the TLS certificate, the clock difference to the server (from its `Date` header) and that a signed `GET /v1/services` is accepted |
| `version`, `help` | none |  |

**Output and exit codes**

- Human-readable tables by default, with colour on a terminal. `--json` prints one JSON document, or one JSON object per line for streams. `--no-color` and `-q` are global flags.
- The HTTP status is sent before a deploy runs, so a deploy stream ends with a line `result: ok` or `result: failed`, and the exit code follows that line.
- Exit codes: `0` success, `1` error, `2` usage, `3` denied or locked out (the retry time is printed), `4` deploy failed on the server. CI can tell a bad key from a bad build.
- The passphrase is asked once per command. Signing through `ssh-agent` is a possible later step.

## Server CLI

Server commands run on the server as root and change local state only. Everything that edits trust (keys, shared networks, volume allowlist) lives here and is never reachable through the API. Names accept both `schedule-reboot` and `schedule_reboot`, as in the original.

| Command | What it does |
| --- | --- |
| `mypaas server init [--key FILE]` | Asks for the admin domain and an optional Let's Encrypt email; there is no username or password any more. Creates `~/_mypaas/` with `config.toml`, `authorized_keys`, `acme.json` (0600), `traefik.toml`, `staticroutes.toml` and the directories `volumes/`, `secrets/`, `services/` and `tmp/`. Writes the systemd unit. `--key` adds the first admin key, because without one nobody can authenticate. |
| `restart all\|daemon\|router` | `daemon` restarts the systemd service `mypaasd`. `router` pulls the pinned Traefik image, replaces the container and starts it with ports 80 and 443. `all` does both. |
| `deploy DIR` | Deploys a local directory through the same code as the API, without authentication. For debugging. |
| `key add FILE\|- --name N --scope S [--expires DATE]` | Validates the key and appends it to `authorized_keys` with an atomic write. `--scope` is repeatable. |
| `key ls` | Name, fingerprint, scopes and expiry of every key |
| `key rm NAME` | Removes a key. The daemon re-reads the file at most every 5 seconds, so removal takes effect within that time. |
| `schedule-reboot [WHEN]` | Creates a systemd timer and service that reboot the server, default `Sun 06:00:00` |
| `daemon` | Hidden. Runs the HTTP daemon; this is what the systemd unit starts. |
| `version`, `help` | Version of mypaas and of the pinned Traefik |

- The systemd unit runs `mypaas server daemon` with `Restart=always` and `RestartSec=2` and the hardening options from the isolation section.
- `config.toml` is read again at every deploy, so changes to `[defaults]`, `[networks]` and `[volumes]` need no restart.
- `server init` is safe to run again: it leaves keys, certificates and secrets alone and rewrites only the Traefik files.

## Phases

The work splits into four phases, each with an exit test that decides whether the next one can start.

1. **Core.** `server init`, `key add`, tickets, `push`, the three deploy strategies, per-service and shared networks, hardening defaults, upload limits and the failure backoff. Exit test: a sample service deploys from a laptop and from CI, and an oversized upload and a bad signature are both rejected without the server reading the body.
2. **Client CLI.** Remotes, `ls`, `status`, `logs`, `rollback`, `rm`, `secret`, `doctor`, `--json` and the exit codes. Exit test: daily operation needs no SSH session.
3. **Hardening.** Docker socket proxy for Traefik, systemd options for the daemon, the volume allowlist, ownership of volumes for non-root images. Exit test: a service cannot reach another service's network, and one Postgres serves two applications, each with its own role.
4. **Traffic stats.** The daemon aggregates Traefik's JSON access log into hourly buckets with 90 days of retention; `top` and `stats` read them. Exit test: request counts, status classes and latency match what the original analytics showed.

**Open questions**

- Should the Docker socket proxy for Traefik be on by default? It adds a container but takes root equivalence away from the internet-facing process.
- Volume ownership for non-root images: chown to the numeric `USER` of the image, or document the manual step?
- Public `publish` and `cap_add` need an `admin` key, which a CI job should not hold. Should the server's config carry an allowlist of services that may publish publicly instead?
- Is signing through `ssh-agent` and hardware keys worth the extra client code?
- The limits in this document (50 MiB upload, 200 MiB unpacked, 1 CPU, 512m, 4 open tickets) are starting values to tune against real projects.
