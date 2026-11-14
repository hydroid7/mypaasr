# Dockerfile comments reference

A mypaas service is configured entirely by `# mypaas.<key>=<value>` comment
lines in its Dockerfile. This page describes each key: its syntax, its
validation, its default and what the deploy does with it.

Source of truth: `src/spec.rs` (parsing and validation), `src/server/deploy.rs`
(what the values become) and `src/config.rs` (server defaults and maximums).
The short table in [README.md](../README.md#configuring-a-service) and the
"Comment keys" section of [design.md](design.md) must agree with this file.

---

## 1. How lines are read

- A line is configuration when, after optional spaces or tabs, it starts with
  `#` and then optional spaces or tabs, then `mypaas.`. Any other line is
  ignored.
- `#--- mypaas.url=…` is **not** configuration. Putting anything other than
  spaces between `#` and `mypaas.` switches the line off. Use this to disable a
  key without deleting it.
- The form is `mypaas.<key>=<value>`. A line with no `=` fails the deploy
  (`expected mypaas.<key>=<value>`).
- Keys are case-sensitive. An unknown key fails the deploy, and the error names
  the line number.
- Whitespace around the value is trimmed.
- Errors name the Dockerfile line number and the offending line.
- A key marked **once** below may appear only once; a second line fails the deploy.
- A key marked **repeatable** may appear several times.
- Duplicates that are silently ignored: a repeated `network` or `cap_add` value
  is dropped, not reported.

The deploy also reads the `FROM` lines. If a base image has no tag, uses
`:latest`, or is not pinned by digest, the build pulls the base image again
(`docker build --pull`). A `FROM` that names an earlier build stage is not
pulled.

## 2. Summary

| Key | Once or repeatable | Default | Needs `admin` |
| --- | --- | --- | --- |
| `service` | once (required) | none | no |
| `url` | repeatable | none (internal service) | no |
| `port` | once | `80` | no |
| `volume` | repeatable | none | only for host paths |
| `env` | repeatable | none | no |
| `scale` | once | `0 roll` | no |
| `healthcheck` | once | none | no |
| `maxcpu` | once | `1` | no |
| `maxmem` | once | `512m` | no |
| `network` | repeatable | none | no |
| `readonly` | once | `false` | no |
| `cap_add` | repeatable | none (all capabilities dropped) | yes |

Server limits that the values must fit: `maxcpu` ≤ 4 and `maxmem` ≤ 4g by
default (`[maximums]` in `config.toml`). Scale is at most 32.

---

## 3. Keys

### 3.1 `service` (required, once)

```dockerfile
# mypaas.service=shop
```

The name of the service. It is the identity of a deployment: a deploy with the
same name replaces the running service, and a deploy with a new name creates a
new one. The directory name and the Dockerfile name do not matter. Two
Dockerfiles in one project with different service names are two independent
services.

Validation: 1 to 63 characters, lowercase letters, digits, `-` and `_`, and the
first character must be a letter or digit.

The service name ends up in container, image, network and file names. The
Dockerfile's `service` must equal the service name the deploy was made for
(`design.md`, bundle checks), or the deploy is refused.

### 3.2 `url` (repeatable)

```dockerfile
# mypaas.url=https://shop.example.com
# mypaas.url=http://shop.example.com/legacy
```

A public address for the service, routed by Traefik. Without any `url` the
service has no public route and is reachable only over shared networks.

Format: `http://` or `https://`, a host, and an optional path.

- **Host**: a DNS name, lowercase, up to 253 characters, labels of 1 to 63
  characters with no leading or trailing `-`. IP addresses are not accepted.
- **Path**: optional. A trailing `/` is removed. Up to 128 characters from
  letters, digits and `- _ . ~ /`. No `//`, and no `.` or `..` segments.
  Traffic is matched by path prefix.
- The same scheme, host and path may not appear twice.

What it produces:

- `https://` routes get a certificate from the `default` certificate resolver,
  the `intermediate` TLS options, and a redirect from `http://` on the same host
  and path (`https-redirect` middleware).
- `http://` routes have no TLS and no redirect.
- Each route gets a Traefik router name made from the host and path, with every
  character except letters and digits replaced by `-`.
- Traefik sends traffic to the container port given by `port`.

Routing from the image is refused: labels that start with `traefik.` or
`mypaas.` in the image are rejected, so only these comments can add routes.

### 3.3 `port` (once, default `80`)

```dockerfile
# mypaas.port=8080
```

The port the process listens on **inside the container**. Traefik sends
requests to it. It is not published on the host. Valid values are 1 to 65535.

### 3.4 `volume` (repeatable)

```dockerfile
# mypaas.volume=data:/var/lib/app
# mypaas.volume=uploads:/srv/uploads:ro
# mypaas.volume=/srv/media:/media:ro
```

Format: `source:/container/path[:ro|:rw]`. The source decides the kind of volume:

- **Named volume**: a source that does not start with `/` or `~`. It must be a
  valid name (the same rules as `service`). It maps to
  `~/_mypaas/volumes/<service>/<name>`, which is created if missing. Only this
  service can use it, and a symlink cannot point it elsewhere.
- **Host path**: a source that starts with `/` or `~`. It needs the `admin`
  scope and an entry in `[volumes] allow` in the server's `config.toml`. It is
  the only way two services share a directory. The path must be normalized,
  with no `.` or `..` components. The server refuses allowlist entries that
  overlap `/`, `/run`, `/var/run`, `~/.ssh` or `~/_mypaas`. Host paths are
  checked before and after symlinks are followed.

The target path (container side) must be absolute and normalized, and may not
be `/`. Paths may not contain `,` or `:` except as the separator. Two volumes
may not mount on the same container path.

`:ro` mounts read-only. `:rw` is accepted and means the default.

Named volumes are created owned by root. If the image runs as a non-root `USER`,
an empty named volume is handed to that user (`[volumes] chown`, on by
default). A volume that already holds data owned by someone else is left alone,
and the deploy prints the `chown -R` command to run.

Anything a container writes outside its volumes is lost on redeploy.

### 3.5 `env` (repeatable)

```dockerfile
# mypaas.env=MODE=prod
# mypaas.env=DB_PASSWORD
```

- `KEY=value` sets a plain environment variable. The value is everything after
  the first `=`.
- `KEY` alone takes the value from this service's secrets file. A missing secret
  fails the deploy and names the key, never a value.

KEY: letters, digits and `_`, not starting with a digit, up to 128 characters.
The value may not contain NUL or carriage-return characters. Each KEY may appear
once; a second line with the same KEY fails the deploy.

Plain values are visible in the Dockerfile, so keep passwords in secrets. Values
reach the container as environment variables at deploy time; they never appear
in logs, progress lines or error messages. Anyone with root on the server can see
them with `docker inspect`. A changed secret takes effect at the next deploy or
rollback.

Secrets are set with `mypaas secret set|ls|rm` (needs the `secrets` scope). The
API returns names only; a value cannot be read back.

### 3.6 `scale` (once, default `0 roll`)

```dockerfile
# mypaas.scale=0
# mypaas.scale=3 safe
```

Format: `N [roll|safe]`, where N is 0 to 32.

- **`0`** (default): one container. The deploy stops the old container first,
  then starts the new one. There is a short outage. Use this for a single
  writer, for example SQLite on a volume. The strategy word is ignored.
- **`N` with `roll`** (default strategy): N replicas named `<service>.1` to
  `<service>.N`. Each new replica must be ready before an old one stops, so
  there is no outage. Replicas are replaced one at a time.
- **`N` with `safe`**: stops all old containers, then starts the N new replicas.
  There is an outage, but no old and new versions run side by side. Use this
  when the old and new versions cannot run together, for example a static site
  or a schema change.

Replicas do not coordinate. If any writable volume is used with `scale` above 0,
the deploy prints a warning, because the replicas will share it.

With `scale` above 0 and a [`healthcheck`](#37-healthcheck-once), Traefik
health-checks the replicas and routes only to those that pass.

### 3.7 `healthcheck` (once)

```dockerfile
# mypaas.healthcheck=/health 10s 2s
```

Format: `/path interval timeout`.

- **path**: starts with `/`, then letters, digits and `- _ . ~ / ? = & %`.
- **interval**: at least `1s`.
- **timeout**: at least `100ms`, and no longer than the interval.
- Durations take `ms`, `s`, `m`, `h` or `d`, for example `500ms`, `10s`, `5m`.

Used only when `scale` is above 0. It becomes a Traefik health check on the
service's port, so unhealthy replicas get no traffic. With `scale` 0 the line is
checked but has no effect.

### 3.8 `maxcpu` (once, default `1`)

```dockerfile
# mypaas.maxcpu=0.5
```

The CPU limit for each container, as a number of CPUs. Valid values are 0.01 to
1024. The value may not exceed the server maximum (`[maximums] cpu`, default 4).
Exceeding it fails the deploy.

### 3.9 `maxmem` (once, default `512m`)

```dockerfile
# mypaas.maxmem=256m
# mypaas.maxmem=1g
```

The memory limit for each container. Format: a whole number followed by an
optional `k`, `m` or `g` (case-insensitive), with an optional `b` after it, or
plain bytes. Examples: `512m`, `1g`, `64k`, `536870912`.

The minimum is `6m`. The value may not exceed the server maximum (`[maximums]
mem`, default `4g`). The swap limit equals the memory limit, so the container
gets no swap.

### 3.10 `network` (repeatable)

```dockerfile
# mypaas.network=db
```

Joins a shared network named `mypaas-net-<name>`, created on the first deploy
that names it. Each service on that network is reachable from the others by its
service name, including when it is scaled.

Every service may join any shared network. A shared network is a convenience,
not a security boundary. For example, a shared Postgres gives each application
its own role and database, with the passwords in each service's own secrets;
that is what keeps one application out of another's data.

Each service always has a private network of its own, `mypaas-<service>`. Shared
networks are added on top. The name follows the same rules as `service`.

### 3.11 `readonly` (once, default `false`)

```dockerfile
# mypaas.readonly=true
```

`true` mounts the container's root filesystem read-only, with a `tmpfs` at
`/tmp` (`rw,nosuid,nodev`). Use it when the process must not write to the image
filesystem. Writes that must persist go in a volume.

Only `true` or `false` are accepted.

### 3.12 `cap_add` (repeatable, needs `admin`)

```dockerfile
# mypaas.cap_add=net_bind_service
```

Adds one Linux capability back. Containers start with every capability dropped
and `no-new-privileges` set, so this is the only way to add one. The name is
case-insensitive, and the `CAP_` prefix is optional. It may contain only letters
and `_`. `ALL` is refused. The deployer needs the `admin` scope; without it the
deploy fails with `these settings need the admin scope`.

A repeated capability is dropped. Prefer running as a non-root `USER` and using a
port above 1024 over adding `net_bind_service`.

---

## 4. What the values become

The parsed configuration is stored with each deploy, so a rollback can recreate
the containers without the Dockerfile. The main effects:

| Key | Docker or Traefik effect |
| --- | --- |
| `service` | Container name `<service>` or `<service>.N`; image `mypaas-<service>:<deploy id>`; label `mypaas.service=<service>`; network `mypaas-<service>` |
| `url` | Traefik router with `Host(...)` and optional `PathPrefix(...)` rule; for https, certificate resolver, TLS options and an http-to-https redirect |
| `port` | Traefik `loadbalancer.server.port` |
| `volume` | `--mount` on `docker create` (bind for host paths and named volumes) |
| `env` | `--env-file` with the values, from the Dockerfile or secrets |
| `scale` | Number of containers; `roll` or `safe` ordering of stops and starts |
| `healthcheck` | Traefik `loadBalancer.healthCheck` labels, when `scale` > 0 |
| `maxcpu` | `--cpus` |
| `maxmem` | `--memory` and `--memory-swap` (equal, so no swap) |
| `network` | `docker network connect --alias <service>` to `mypaas-net-<name>` |
| `readonly` | `--read-only` and `--tmpfs /tmp:rw,nosuid,nodev` |
| `cap_add` | `--cap-add <CAP>` |

Every container also gets, from mypaas and not from these comments: `--cap-drop
ALL`, `--security-opt no-new-privileges`, `--restart unless-stopped`,
`--pids-limit` (512 by default), `--log-driver json-file` with 3 files of 10 MiB,
and no published host ports. See the "Container hardening" section of [design.md](design.md).

---

## 5. Example

```dockerfile
# mypaas.service=shop
# mypaas.url=https://shop.example.com
# mypaas.url=https://www.shop.example.com
# mypaas.port=8080
# mypaas.volume=uploads:/srv/uploads
# mypaas.env=MODE=prod
# mypaas.env=DB_PASSWORD
# mypaas.scale=2 roll
# mypaas.healthcheck=/health 10s 2s
# mypaas.maxcpu=0.5
# mypaas.maxmem=256m
# mypaas.network=db
# mypaas.readonly=true

FROM python:3.13-slim
WORKDIR /app
COPY . .
USER 1000
CMD ["python", "-m", "http.server", "8080"]
```

Complete, tested services for each pattern are in [examples/](../examples/README.md).
The unit test `spec::tests::examples_are_valid` parses every example Dockerfile,
so the examples always match the parser.

## 6. Errors you will see

| Message | Cause |
| --- | --- |
| `the Dockerfile has no # mypaas.service=<name> line` | `service` missing |
| `unknown key mypaas.<key>` | Typo, or a key that does not exist |
| `mypaas.<key> given more than once` | A once-key appears twice |
| `invalid service name` / `invalid network name` / `invalid volume name` | Name rules broken |
| `url … must start with http:// or https://` | Missing scheme |
| `two volumes are mounted on the same container path` | Duplicate target |
| `the same url is given twice` | Duplicate scheme, host and path |
| `scale N is above the maximum of 32` | Too many replicas |
| `healthcheck: interval must be at least 1s…` | Bad interval or timeout |
| `maxcpu … is above the server maximum` | Above `[maximums] cpu` |
| `these settings need the admin scope: …` | `cap_add` or a host volume without `admin` |
