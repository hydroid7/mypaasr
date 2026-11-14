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
- [LAN mode](#lan-mode)
- [Security model](#security-model)
- [Development](#development)
- [Status](#status)
- [License](#license)

## Quick start

### 1. Prepare the server

You need a Linux server with Docker and systemd, and a DNS name for the admin
API (here `paas.example.com`) pointing at it. Ports 80 and 443 must be free.

Install mypaas from the [latest release](https://github.com/hydroid7/mypaasr/releases/latest).
On Debian or Ubuntu (amd64), as root, take the `.deb` (replace `0.1.0` with
the release's version):

```sh
curl -fLO https://github.com/hydroid7/mypaasr/releases/download/v0.1.0/mypaas_0.1.0_amd64.deb
apt install ./mypaas_0.1.0_amd64.deb      # /usr/bin/mypaas; recommends Docker
```

On any other x86_64 Linux, take the static binary, which runs whatever the
glibc:

```sh
curl -fL https://github.com/hydroid7/mypaasr/releases/latest/download/mypaas-x86_64-linux -o /usr/local/bin/mypaas
chmod +x /usr/local/bin/mypaas
```

Each release has a `SHA256SUMS` and build provenance, so you can check a
download with `sha256sum -c SHA256SUMS --ignore-missing` or
`gh attestation verify <file> --repo hydroid7/mypaasr`. To build both
yourself, see [Development](#development). The client on your laptop is the
same binary; on macOS or Windows, build it with `cargo build --release`.

Upgrading with the `.deb` restarts the daemon and the socket proxy on the new
binary; services and Traefik keep running. Removing it stops the daemon and
leaves `~/_mypaas`, the containers and the unit files in place.

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
`[stats]` take effect after `mypaas server restart all`. Changes to `[lan]`
take effect after `mypaas server init` and `mypaas server restart all`. The
defaults:

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

[lan]                           # see LAN mode; the release binaries have it
enabled = false
interfaces = []                 # empty: every up, non-loopback, non-Docker interface
allow = ["*.local"]             # names services may claim
plain_admin = false             # admin API over plain HTTP; see LAN mode
```

The server refuses to start if a `volumes.allow` entry overlaps `/`,
`~/.ssh`, `~/_mypaas` or `/var/run`.

### Server commands

| Command | What it does |
| --- | --- |
| `server init [--domain D] [--email E] [--key FILE] [--lan]` | Create `~/_mypaas`, the Traefik files and the systemd units; `--lan` turns on [LAN mode](#lan-mode) |
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

## LAN mode

LAN mode lets devices on the same local network open a service by a `.local`
name, such as `http://shop.local`, with no DNS server and no hosts-file edits.
It is off by default, and it changes nothing for public URLs.

There are two ways to use it:

- **Public server with LAN names**: the admin API stays on HTTPS under a
  public domain, and services may add `.local` URLs. See
  [Turn it on](#turn-it-on).
- **LAN-only server**: no public domain at all, for a home server or a lab.
  The admin API is reached over plain HTTP on the LAN. See the walkthrough
  below.

### Walkthrough: set up a LAN-only server

This sets up a server that is reachable only on your local network, as
`paas.local`, with services such as `http://lan-app.local`.

**1. Prepare the machine.** You need Linux with Docker and systemd on the same
network (and subnet) as the devices that will use it. Install mypaas as in
[Quick start](#1-prepare-the-server). Check that:

- TCP port 80 is free and reachable from the LAN. Port 443 is not used for
  `.local` names, but Traefik still listens on it.
- UDP port 5353 (mDNS) is allowed in from the LAN by the host firewall. With
  `ufw`: `ufw allow from 192.168.1.0/24 to any port 5353 proto udp` and
  `ufw allow from 192.168.1.0/24 to any port 80 proto tcp`.
- The admin name you pick is not the machine's own hostname. If the machine is
  called `nas`, Avahi already announces `nas.local`; use `paas.local` instead.
  A running `avahi-daemon` is fine otherwise.
- Find your LAN's address ranges, for example with `ip addr` (an address
  `192.168.1.20/24` means the range `192.168.1.0/24`). If the machine also has
  an IPv6 address starting with `fd` (a unique local address, which most home
  routers hand out), its first four groups give the second range, for example
  `fd12:3456:789a::/64`. Clients may connect over it.

**2. Create your key** on your laptop, as in the Quick start:

```sh
mypaas key init
mypaas key show > mykey.pub     # copy this file to the server
```

**3. Initialize the server** as root. `--lan` with a `.local` domain turns on
LAN mode and the plain-HTTP admin API; it prints a warning about what that
means. No Let's Encrypt account is needed, so skip the email:

```sh
mypaas server init --domain paas.local --lan --email "" --key mykey.pub
```

**4. Restrict the admin API to your LAN.** Edit `~/_mypaas/config.toml` (of
the user that ran `init`, normally `/root/_mypaas/config.toml`):

```toml
[router]
api_allow = ["192.168.1.0/24", "fd12:3456:789a::/64"]  # your LAN's ranges from step 1
```

Then write the files again and start everything:

```sh
mypaas server init --domain paas.local --email ""   # keeps [lan]; rewrites Traefik files
mypaas server restart all
```

**5. Check name resolution** from another machine on the LAN:

```sh
avahi-resolve -4 -n paas.local  # Linux; on macOS: dns-sd -G v4 paas.local
ping paas.local
```

If it does not resolve, see [Client support](#client-support) and the
firewall step above.

**6. Connect the client** on your laptop. The `http://` is required:

```sh
mypaas remote add lan http://paas.local
mypaas -r lan doctor            # warns that the connection is not encrypted
```

**7. Deploy a service** with a `.local` URL, for example
[examples/lan-app](examples/lan-app):

```sh
mypaas -r lan push examples/lan-app
mypaas -r lan ls                # shows lan-app.local as announced
```

Open `http://lan-app.local` from any device on the network.
[examples/lan-dashboard](examples/lan-dashboard) is a start page at
`http://dashboard.local` that lists links to your other LAN services. Secrets need the
extra flag, because they cross the LAN unencrypted:

```sh
mypaas -r lan secret set lan-app API_TOKEN --plain-secrets
```

Do not forward port 80 from your internet router to this server: the admin API
is on that port. Read [docs/security.md](docs/security.md#lan-mode) for what
plain HTTP exposes. Later changes to `[lan]` take effect after
`mypaas server init` and `mypaas server restart all`.

### Turn it on

On the server, as root:

```sh
mypaas server init --lan          # sets [lan] enabled = true, rewrites the units
mypaas server restart all
```

You can also set `enabled = true` under `[lan]` in `config.toml` and run the
same two commands. The release binaries and the `.deb` include LAN mode. A
binary built with `--no-default-features` does not; it refuses to load a config
that enables it.

### Name a service

Use the `url` key with `http://` and a `.local` host:

```dockerfile
# mypaas.service=lan-app
# mypaas.url=http://lan-app.local
# mypaas.port=8080
```

[examples/lan-app](examples/lan-app) is a complete one. After `mypaas push`,
open `http://lan-app.local` from any device on the same network.

- Only `.local` names are announced. `https://….local` is refused at deploy,
  because there is no certificate for these names yet.
- A name with more than one label before `.local`, such as `api.shop.local`, is
  accepted with a warning. Some clients only resolve `name.local`.
- `allow` in `[lan]` limits which names may be claimed. It is `["*.local"]` by
  default.
- If another device already has the name, mypaas does not announce it, and
  `mypaas ls` and `status` show `conflict`. Other states are `announced` and
  `error`.
- A name is announced while its service is deployed, even if its containers are
  down. Removing the service (`rm`) withdraws it.
- Changes take effect within a few seconds of a deploy, rollback or removal.

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Turns LAN mode on |
| `interfaces` | `[]` | Network interfaces to announce on. Empty means every up, non-loopback, non-Docker interface. |
| `allow` | `["*.local"]` | Globs of names services may claim |
| `plain_admin` | `false` | Serves the admin API over plain HTTP; see below |

### Manage the server from the LAN (plain HTTP)

A server with no public domain can be run from the LAN without an SSH tunnel
or a certificate. Its admin domain must be a `.local` name:

```sh
mypaas server init --domain paas.local --lan --key mykey.pub     # prints a warning
mypaas server restart all
```

This sends the admin API over plain HTTP. Anyone on the same network can read
the traffic, including secret values, logs and bundles, and anyone who wins the
name `paas.local` on that network receives your requests. Keys still protect
requests from being forged, but nothing protects them from being read. Read
[docs/security.md](docs/security.md#lan-mode) before you use it.

On your laptop, add the remote with an explicit `http://`:

```sh
mypaas remote add lan http://paas.local
mypaas -r lan doctor            # warns that the connection is not encrypted
mypaas -r lan push .
```

A bare `paas.local` stays HTTPS, so the scheme is always visible in the remote.
`secret set` refuses to send a value over this remote unless you pass
`--plain-secrets`:

```sh
mypaas -r lan secret set shop API_TOKEN --plain-secrets
```

Then restrict the admin API to your network. In `config.toml`:

```toml
[router]
api_allow = ["192.168.1.0/24", "fd12:3456:789a::/64"]  # your LAN's IPv4 and unique local IPv6 ranges
```

and run `mypaas server restart all`. Traefik answers everyone else with 403.
`server init` and the daemon warn when `plain_admin` is on and `api_allow` is
empty. Never forward port 80 from your router to a server that runs
`plain_admin`.

### Client support

Announcing a name is not the same as a device resolving it:

- Apple devices (macOS, iOS) resolve `.local` names.
- Linux needs mDNS in name resolution: nss-mdns, or systemd-resolved with mDNS
  turned on. The default nss-mdns setup handles only `name.local` names.
- Windows and Android depend on the version and the browser. Android browsers
  often do not resolve `.local` names at all. Check the devices you use.
- Wi-Fi access points and VLANs can filter multicast, and client isolation
  stops devices from seeing each other. mDNS does not cross subnets.
- A host running `avahi-daemon` also uses UDP port 5353. It coexists with mypaas.
  The server's own `<hostname>.local`, which Avahi announces, is reserved: in
  the spike a name claimed by both mypaas and Avahi on the same machine was not
  reported as a conflict. The server must let UDP 5353 through on the LAN side.

Known limits:

- A name is announced while its service's spec is on disk, even if the
  containers are down.
- Stopping or restarting the daemon does not send goodbye packets, so other
  devices may keep a name cached for up to two minutes. A restart announces the
  same names again; `mypaas rm` withdraws a name at once.
- Conflicts are detected against other devices, not against Avahi on the same
  machine (hence the reserved `<hostname>.local`).

Not in this version: HTTPS for `.local` names and mDNS across subnets. Names
get AAAA records too, so clients may connect over IPv6. Only link-local and
unique local (`fd…`) addresses are announced, never global ones, so a client
always arrives from your IPv4 range or your unique local prefix.

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
- **LAN mode is opt-in.** When on, names are announced on the local network and
  `plain_admin` can serve the admin API over plain HTTP. Both weaken the rules
  above for that network; see [docs/security.md](docs/security.md#lan-mode).

A deploy key runs arbitrary `RUN` steps inside Docker, so treat it like any
other CI credential. The design document has the full threat model.

## Development

```sh
cargo test                                  # unit tests, no Docker needed
cargo test --test docker -- --ignored       # end-to-end tests against Docker
cargo clippy --all-targets
cargo build --no-default-features           # without LAN mode (no mdns-sd)
cargo test --no-default-features
```

The end-to-end tests start real daemons, socket proxies and Traefik
containers. They need Docker and the `busybox:1.36` and `traefik:v3.7.17`
images, use unique names and clean up after themselves.

### Builds and releases

```sh
scripts/build-static.sh     # target-musl/x86_64-unknown-linux-musl/release/mypaas
scripts/build-deb.sh        # target-deb/mypaas_<version>_amd64.deb
```

Both need only Docker on your machine. GitHub Actions runs
[`ci.yml`](.github/workflows/ci.yml) on every push and pull request: clippy,
the unit tests, the Docker end-to-end tests, and both builds with the `.deb`
installed and removed on a clean Debian 13.

To release, set `version` in `Cargo.toml`, commit, and push a matching tag:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

[`release.yml`](.github/workflows/release.yml) checks that the tag matches
`Cargo.toml`, tests, builds, attests and publishes `mypaas-x86_64-linux`,
`mypaas_<version>_amd64.deb` and `SHA256SUMS`. A tag with a hyphen
(`v0.2.0-rc.1`) becomes a pre-release.

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
| `src/server/lan/` | LAN mode: name derivation and checks (`hosts.rs`), the reconcile loop (`announce.rs`), the mDNS backend (`mdns.rs`), and the facade (`mod.rs`) |

## Status

All four phases of the design are implemented and covered by tests, including
a full run on a local systemd VM. Still to do:

- A trial on a public server: real Let's Encrypt certificates and deploys
  from CI.

Signing through `ssh-agent` is deliberately not supported: the client reads
its key from a file or `MYPAAS_PRIVATE_KEY` and depends on nothing from SSH.

## License

MIT, see [LICENSE](LICENSE).
