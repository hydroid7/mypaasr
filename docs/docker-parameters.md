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

When deploying a new service, speical comments in docker file are interpreted by mypaars.

# Docker parameters reference

Every option of the Docker CLI commands that matter to mypaas-rs: `docker create`
(and `docker run`, which takes the same options), `docker build`,
`docker network create` and `docker volume create`.

Source: the output of `docker <command> --help` from Docker Engine / CLI
**29.9.3**. Options can change between Docker releases; re-run the help command
when upgrading. Options are listed as the CLI prints them, so the value type
appears after the name (`list`, `string`, `bytes`, …).

The **mypaas** column says how mypaas-rs uses an option:

- **yes**: mypaas passes it (see the source reference in the last section).
- **no**: mypaas never passes it, and the design forbids it where noted.
- blank: the option is available to Docker, but mypaas does not use it.

mypaas creates containers with `docker create` followed by `docker start`,
never `docker run`. Its `docker run` column is the same as `docker create`.

---

## 1. `docker run` / `docker create`

Usage: `docker run [OPTIONS] IMAGE [COMMAND] [ARG...]`

### 1.1 Lifecycle and identity

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `--name` | string | Assign a name to the container | yes |
| `-h, --hostname` | string | Container host name | yes (service name) |
| `--domainname` | string | Container NIS domain name | |
| `-d, --detach` | | Run in background and print the container ID (`run` only) | yes (`start` path) |
| `-i, --interactive` | | Keep STDIN open even if not attached | |
| `-t, --tty` | | Allocate a pseudo-TTY | |
| `-a, --attach` | list | Attach to STDIN, STDOUT or STDERR | |
| `--detach-keys` | string | Override the key sequence for detaching | |
| `--sig-proxy` | | Proxy received signals to the process (default true) | |
| `--stop-signal` | string | Signal used to stop the container | |
| `--stop-timeout` | int | Seconds to wait before killing a stopping container | |
| `--init` | | Run an init process that forwards signals and reaps processes | |
| `--rm` | | Remove the container and its anonymous volumes when it exits (`run` only) | |
| `--cidfile` | string | Write the container ID to a file | |
| `--entrypoint` | string | Overwrite the image's `ENTRYPOINT` | yes |
| `-w, --workdir` | string | Working directory inside the container | |
| `-u, --user` | string | `<name\|uid>[:<group\|gid>]` | |
| `--umask` | umask | Set umask for the container | |
| `--no-healthcheck` | | Disable any `HEALTHCHECK` from the image | |
| `--health-cmd` | string | Command that checks health | |
| `--health-interval` | duration | Time between checks (`ms\|s\|m\|h`) | |
| `--health-retries` | int | Consecutive failures before unhealthy | |
| `--health-start-interval` | duration | Check interval during the start period | |
| `--health-start-period` | duration | Start period before retries count down | |
| `--health-timeout` | duration | Maximum time for one check | |
| `-l, --label` | list | Set metadata on a container | yes |
| `--label-file` | list | Read labels from a line-delimited file | |
| `--platform` | string | Platform, if the server is multi-platform | |
| `--pull` | string | Pull the image before running: `always`, `missing`, `never` (default `missing`) | |
| `-q, --quiet` | | Suppress the pull output | |
| `--runtime` | string | OCI runtime to use | |
| `--isolation` | string | Container isolation technology | |
| `--use-api-socket` | | Bind-mount the Docker API socket and auth | no (forbidden: Docker socket) |

### 1.2 Environment

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `-e, --env` | list | Set environment variables | |
| `--env-file` | list | Read environment variables from a file | yes |

### 1.3 Networking

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `--network` | network | Connect to a network | yes |
| `--network-alias` | list | Network-scoped alias for the container | |
| `--ip` | ip | IPv4 address on the network | |
| `--ip6` | ip | IPv6 address on the network | |
| `--link` | list | Link to another container (legacy) | |
| `--link-local-ip` | list | Container IPv4/IPv6 link-local addresses | |
| `--mac-address` | string | Container MAC address | |
| `--add-host` | list | Custom host-to-IP mapping (`host:ip`) | |
| `--dns` | list | Custom DNS servers | |
| `--dns-option` | list | DNS options | |
| `--dns-search` | list | Custom DNS search domains | |
| `--expose` | list | Expose a port or range without publishing it | |
| `-p, --publish` | list | Publish a port to the host (`[ip:]hostPort:containerPort`) | no (Traefik routes instead) |
| `-P, --publish-all` | | Publish all exposed ports to random host ports | no |
| `--pid` | string | PID namespace (`host` is forbidden) | no (`host` forbidden) |
| `--uts` | string | UTS namespace (`host` is forbidden) | |
| `--ipc` | string | IPC namespace | |
| `--cgroupns` | string | Cgroup namespace: `host`, `private`, or daemon default | |
| `--cgroup-parent` | string | Parent cgroup | |
| `--userns` | string | User namespace to use | |

### 1.4 Storage and mounts

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `-v, --volume` | list | Bind mount a volume (`[src:]dst[:opts]`) | |
| `--mount` | mount | Attach a filesystem mount (`type=…,src=…,dst=…`) | yes |
| `--volumes-from` | list | Mount volumes from other containers | |
| `--volume-driver` | string | Optional volume driver | |
| `--read-only` | | Mount the root filesystem read-only | yes (readonly service) |
| `--tmpfs` | list | Mount a tmpfs directory (`path:options`) | yes (`/tmp` when readonly) |
| `--shm-size` | bytes | Size of `/dev/shm` | |
| `--storage-opt` | list | Storage driver options | |

### 1.5 Security and privileges

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `--privileged` | | Extended privileges | no (forbidden) |
| `--cap-add` | list | Add Linux capabilities | yes (only from `mypaas.cap_add`) |
| `--cap-drop` | list | Drop Linux capabilities | yes (`ALL`) |
| `--security-opt` | list | Security options (`no-new-privileges`, `seccomp=…`, `apparmor=…`, `label=…`) | yes (`no-new-privileges`) |
| `--group-add` | list | Additional groups to join | |
| `--device` | list | Add a host device | no (forbidden) |
| `--device-cgroup-rule` | list | Add a cgroup device rule | |
| `--device-read-bps` | list | Read-rate limit from a device | |
| `--device-read-iops` | list | Read IOPS limit from a device | |
| `--device-write-bps` | list | Write-rate limit to a device | |
| `--device-write-iops` | list | Write IOPS limit to a device | |
| `--gpus` | gpu-request | GPU devices (`all` for every GPU) | |
| `--sysctl` | map | Sysctl options | |
| `--ulimit` | ulimit | Ulimit options (`name=soft[:hard]`) | |
| `--oom-kill-disable` | | Disable the OOM killer | |
| `--oom-score-adj` | int | Host OOM preference (-1000 to 1000) | |
| `--annotation` | map | OCI annotations passed to the runtime | |

### 1.6 Resources

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `-m, --memory` | bytes | Memory limit | yes |
| `--memory-reservation` | bytes | Memory soft limit | |
| `--memory-swap` | bytes | Memory plus swap limit (`-1` for unlimited swap) | yes (equal to memory, so no swap) |
| `--memory-swappiness` | int | Swappiness (0 to 100) | |
| `--pids-limit` | int | Maximum processes (`-1` for unlimited) | yes |
| `--cpus` | decimal | Number of CPUs | yes |
| `-c, --cpu-shares` | int | Relative CPU weight | |
| `--cpu-period` | int | CFS period (microseconds) | |
| `--cpu-quota` | int | CFS quota (microseconds) | |
| `--cpu-rt-period` | int | Real-time period (microseconds) | |
| `--cpu-rt-runtime` | int | Real-time runtime (microseconds) | |
| `--cpuset-cpus` | string | CPUs allowed (`0-3`, `0,1`) | |
| `--cpuset-mems` | string | Memory nodes allowed | |
| `--blkio-weight` | uint16 | Block I/O relative weight (10 to 1000, 0 disables) | |
| `--blkio-weight-device` | list | Per-device block I/O weight | |

### 1.7 Logging

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `--log-driver` | string | Logging driver | yes (`json-file`) |
| `--log-opt` | list | Logging driver options (`max-size=…`, `max-file=…`) | yes |

### 1.8 Restart

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `--restart` | string | `no`, `on-failure[:max-retries]`, `always`, `unless-stopped` | yes (`unless-stopped`) |

---

## 2. `docker build`

Usage: `docker build [OPTIONS] PATH | URL | -`

The legacy builder is deprecated in this Docker version. The options below are
the legacy CLI's; BuildKit (`docker buildx build`) accepts more.

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `-t, --tag` | list | Name and optional tag (`name:tag`) | yes |
| `-f, --file` | string | Name of the Dockerfile (default `PATH/Dockerfile`) | yes |
| `--build-arg` | list | Build-time variables | |
| `--label` | list | Metadata for the image | yes (`mypaas.service`) |
| `--target` | string | Build stage to stop at | |
| `--no-cache` | | Do not use the cache | |
| `--cache-from` | strings | Images to use as cache sources | |
| `--pull` | | Always attempt to pull a newer base image | yes (when `info.pull`) |
| `--rm` | | Remove intermediate containers after success (default true) | |
| `--force-rm` | | Always remove intermediate containers | |
| `-q, --quiet` | | Suppress build output; print image ID on success | |
| `--progress` | string | Progress output: `auto`, `plain`, `tty` (buildx) | yes (`plain`) |
| `--iidfile` | string | Write the image ID to a file | |
| `--add-host` | list | Custom host-to-IP mapping | |
| `--network` | string | Network mode for `RUN` steps (default `default`) | |
| `--platform` | string | Target platform | |
| `--compress` | | Gzip the build context | |
| `--security-opt` | strings | Security options for `RUN` steps | |
| `--isolation` | string | Isolation technology | |
| `--ulimit` | ulimit | Ulimit options | |
| `-m, --memory` | bytes | Memory limit for `RUN` steps | |
| `--memory-swap` | bytes | Memory plus swap limit | |
| `--shm-size` | bytes | Size of `/dev/shm` | |
| `-c, --cpu-shares` | int | Relative CPU weight | |
| `--cpu-period` | int | CFS period | |
| `--cpu-quota` | int | CFS quota | |
| `--cpuset-cpus` | string | CPUs allowed | |
| `--cpuset-mems` | string | Memory nodes allowed | |
| `--cgroup-parent` | string | Parent cgroup for `RUN` steps | |

---

## 3. `docker network create`

Usage: `docker network create [OPTIONS] NETWORK`

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `-d, --driver` | string | Network driver (default `bridge`) | yes (`bridge`) |
| `--label` | list | Metadata on the network | yes |
| `-o, --opt` | map | Driver-specific options | |
| `--subnet` | strings | Subnet in CIDR form | |
| `--gateway` | ipSlice | IPv4 or IPv6 gateway for the subnet | |
| `--ip-range` | ipNetSlice | Allocate container IPs from a sub-range | |
| `--aux-address` | map | Auxiliary addresses reserved for the driver | |
| `--ipv4` | | Enable or disable IPv4 (default true) | |
| `--ipv6` | | Enable IPv6 | |
| `--internal` | | Block external access to the network | |
| `--attachable` | | Allow manual container attachment (swarm) | |
| `--ingress` | | Create a swarm routing-mesh network | |
| `--scope` | string | Network scope | |
| `--ipam-driver` | string | IPAM driver (default `default`) | |
| `--ipam-opt` | map | IPAM driver options | |
| `--config-from` | string | Copy configuration from another network | |
| `--config-only` | | Create a configuration-only network | |

`docker network connect --alias <name> <network> <container>` is used by mypaas
to join shared networks. The `--alias` option is listed under 1.3 above.

---

## 4. `docker volume create`

Usage: `docker volume create [OPTIONS] [VOLUME]`

| Option | Type | Description | mypaas |
| --- | --- | --- | --- |
| `-d, --driver` | string | Volume driver (default `local`) | |
| `-o, --opt` | map | Driver-specific options | |
| `--label` | list | Metadata on the volume | |

mypaas-managed volumes are not created with `docker volume create`. They are
bind-mounted with `--mount` (see 1.4), and the owner is set with `chown` from
the server (`src/server/owner.rs`).

---

## 5. Where mypaas uses these options

| Area | File | Docker call |
| --- | --- | --- |
| Container creation | `src/server/deploy.rs` (`Run::create`) | `docker create` with `--name`, `--hostname`, `--network`, `--restart unless-stopped`, `--cap-drop ALL`, `--security-opt no-new-privileges`, `--pids-limit`, `--cpus`, `--memory`, `--memory-swap`, `--log-driver json-file`, `--log-opt`, `--env-file`, `--label`, `--cap-add`, `--read-only`, `--tmpfs`, `--mount`, `--label` (Traefik) |
| Connect shared networks | `src/server/deploy.rs` | `docker network connect --alias <service> <network> <container>` |
| Image build | `src/server/deploy.rs` | `docker build --progress plain [--pull] --tag … --label mypaas.service=… --file … <dir>` |
| Image inspection | `src/server/docker.rs` | `docker image inspect --format {{.Id}}` |
| Network existence and creation | `src/server/docker.rs` | `docker network inspect`, `docker network create --driver bridge --label …` |
| Container state | `src/server/docker.rs` | `docker inspect --format {{.State.Status}} {{.RestartCount}}` |
| Stats | `src/server/docker.rs` | `docker stats --no-stream --format …` |
| Logs | `src/server/api.rs` | `docker logs --tail …` |

The rules that go with these calls are in [design.md](design.md) and
[security.md](security.md):

- Docker is called with argument vectors only, never through a shell.
- Every value that reaches a Docker argument is validated in `spec.rs` or
  `config.rs` first.
- `--privileged`, host PID or network namespaces, `--device` and the Docker
  socket are never used.
- Each service has its own network, `--cap-drop ALL` and `no-new-privileges`.
