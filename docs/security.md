# Security review and hardening

This document records a review of mypaas-rs as a server exposed to the
internet: what an attacker can reach, what was found and fixed, the risk that
remains, and what the operator of a server should do on top. The threat model
and the design behind it are in [design.md](design.md).

Review date: 2026-11-07, against mypaas-rs 0.1.0 with Traefik 3.7.17.

## Attack surface

From the internet, only Traefik is reachable, on ports 80 and 443.

| Who | Reaches | Through |
| --- | --- | --- |
| Anyone | Every routed service; the admin API on `/v1/` of the admin domain | Traefik |
| A key holder | The API's authenticated endpoints, within the key's scopes | Traefik, then the daemon on `127.0.0.1:88` |
| A deploy key | Arbitrary code at build time and in its containers | `docker build`, the service's containers |
| A compromised service | Its own network, shared networks it joined, the internet | Docker networking |
| A device on the LAN (LAN mode only) | The `.local` services over plain HTTP and their names; with `plain_admin`, the admin API over plain HTTP | Wi-Fi or the LAN, then Traefik on port 80. See [LAN mode](#lan-mode). |

## Findings and fixes

Severity reflects what an attacker could do before the fix. Every fix is
covered by a test; the last column names it.

| # | Severity | Finding | Fix | Test |
| --- | --- | --- | --- | --- |
| 1 | Critical | **Route hijacking through image labels.** Docker copies an image's `LABEL`s onto its containers, and Traefik routes by container labels. A Dockerfile with `LABEL traefik.http.routers.x.rule=Host(...)` and a high priority let any deploy key take over any domain on the server, including the admin domain: it would receive admins' signed requests, upload bundles and secret values in transit. | After the build, the deploy inspects the image and refuses any label starting with `traefik.` or `mypaas.` (except the `mypaas.service` mypaas sets itself). The image is removed and the running version stays. | `deploy::image_labels_cannot_route`, `image_labels_cannot_hijack_routes` (Docker), VM test |
| 2 | High | **Service URLs on the admin domain.** A service could route `https://<admin domain>/…`. With a long enough path, its rule's default priority (the rule's length) could beat the API routers for chosen paths, for example a secret's URL. | URLs on the admin domain are refused, even for `admin`. URL paths are limited to 128 characters, and the API routers' priorities are raised to 100000 and above, out of reach of any generated rule. | `deploy::privileges_and_networks`, `spec::url_paths_are_bounded`, `setup::traefik_files_parse_as_toml`, VM test |
| 3 | High | **Signatures were not bound to a server.** The signed string covered method, path, time, nonce and body, but not the host. With one key authorized on two servers, the operator of one (or an attacker who controls it) could relay a user's signed request, say a `DELETE`, to the other within 30 seconds. | The Host header is part of the signed string. Traefik only routes `Host(<admin domain>)` to the daemon, so a request signed for another server fails with the usual `403 denied`. | `auth::sign_and_verify`, `daemon::signatures_are_bound_to_the_host` |
| 4 | Medium | **One address could tie up the API.** Failures are counted when a request ends, so a single address could open many slow requests at once (headers, then a body trickled for 10 seconds), all before its first failure, and hold the daemon's 64 connections. Key ids are fingerprints of public keys and may be public, for example for keys also published on GitHub. | Traefik limits each address to 10 requests per second (bursts of 50) and 16 in flight on the API routers. The daemon also allows at most 8 requests in flight per address, before any parsing or cryptography. | `guard::in_flight_per_address`, `setup::traefik_files_parse_as_toml` |
| 5 | Medium | **A 10-minute window for slow clients on every site.** Port 443 had `readTimeout = 600s` so uploads could finish. Traefik has no separate header timeout, so any client could also trickle headers for 10 minutes, on every service. Headers could be 1 MiB. | `readTimeout` is now `router.read_timeout` (default 300 s, enough for 100 MiB at about 3 Mbit/s), and the daemon uses the same limit. Port 80 has 60 s. Both have `idleTimeout = 60s` and 32 KiB headers; HTTP/2 allows 100 streams per connection. | `setup::traefik_files_parse_as_toml` |
| 6 | Medium | **Traefik ran as full root.** The internet-facing process had Docker's default capabilities, a writable root filesystem and no resource limits. The image was pinned by tag only, which a registry can re-point. | Traefik runs with all capabilities dropped except `NET_BIND_SERVICE`, `no-new-privileges`, a read-only root filesystem with a small `/tmp`, 512 MiB of memory and 512 processes. The image is pinned by digest (`TRAEFIK_DIGEST`). | `traffic_stats_match_the_requests_sent` (Docker) runs Traefik with these flags; VM test |
| 7 | Medium | **The default certificate gave the server away.** A TLS handshake with any name, or none (scanners connecting by IP), got Traefik's self-signed default certificate, which names the software. TLS settings for those connections were Traefik's defaults. | `tls.options.default` sets `sniStrict`, TLS 1.2 or newer and the same six AEAD suites as the service routes, so handshakes for unknown names fail. `router.strict_sni = false` turns this off, for testing without real certificates only. | `setup::traefik_files_parse_as_toml`, VM test |
| 8 | Medium | **Containers could reach the cloud metadata service.** On AWS, GCP and Azure, `169.254.169.254` hands out the VM's cloud credentials. An SSRF bug in any hosted app, or any `RUN` step, could fetch them. | `server init` installs `mypaas-firewall.service`, which rejects traffic from containers to `169.254.0.0/16` (and AWS's IPv6 `fd00:ec2::254`) in Docker's `DOCKER-USER` chain, and re-applies the rule whenever Docker restarts. | `setup::firewall_unit_blocks_metadata_idempotently`, VM test |
| 9 | Low | **Early refusals arrived late.** The daemon refuses a bad upload from its headers, but Traefik's HTTP/1.1 side passes the answer on only after the whole body has been sent (Go's HTTP/1.1 server is not full duplex). The client spoke HTTP/1.1 only, so a `409 busy` cost a full upload per retry. Nothing reached the disk. | The client negotiates HTTP/2 with Traefik, where the refusal arrives at once and the client stops sending. | VM test: a slowly sent upload with a bad ticket is refused before its body is complete |
| 10 | Low | **No way to keep strangers away from the API.** | Optional `router.api_allow`: a list of client networks. If set, Traefik answers everyone else with 403 before the daemon sees the request. Services are not affected. | `config::cidrs`, `setup::traefik_files_parse_as_toml` |

## Checked and found sound

- **Authentication before data.** Uploads are decided from the headers; small
  bodies are read only after the key is known and inside a 64 KiB cap and a
  10-second timeout. Chunked bodies are refused, which also closes request
  smuggling between Traefik and the daemon.
- **Uniform answers.** Unknown keys, expired keys, missing scopes and bad
  signatures all get the same `403`; locked-out addresses get `429` before any
  parsing.
- **Forwarded addresses.** Traefik does not trust incoming `X-Forwarded-For`
  by default and sets its own, and the daemon trusts that header only from
  loopback, so a client cannot choose the address its failures count against.
- **Tickets.** 256 random bits, single use, 60 seconds, bound to the bundle's
  size and hash, stored only as a SHA-256.
- **Bundles.** The unpacker refuses links, devices, absolute paths and `..`,
  including through GNU long names and PAX headers, and stops at the size and
  file limits while decompressing.
- **The socket proxy.** Only five read-only calls pass, with no request body,
  and environment variables are removed from inspect answers.
- **Builds cannot use the host's network.** Docker refuses
  `RUN --network=host` without the `network.host` entitlement, which mypaas
  never grants (checked against Docker 29).
- **Daemon.** Listens on loopback only, and its systemd unit is sandboxed. It
  calls Docker with argument vectors, never a shell, and every value from a
  Dockerfile is validated before it reaches an argument, a label or a path.

## Remaining risk

These are known and accepted, or need work beyond configuration:

- **The daemon is root-equivalent.** It controls Docker. A bug in its
  authentication would hand over the server. The surface is kept small, and
  rootless Docker or Podman is the long-term answer.
- **A deploy key runs code on the server.** Builds and containers run
  arbitrary code with outbound network access: they can scan the internet or
  reach other machines on the server's private network, and they can reach
  the host's own ports through the bridge gateway (see the checklist).
- **Distributed attacks on the API.** Per-address limits do not stop many
  addresses, for example an IPv6 /48 split into /64s or a botnet, from using
  up the global budget of 50 signature checks per second when they know a key
  id. The API becomes slow or unavailable; the services keep running. Use
  `router.api_allow` where possible.
- **Slow clients within the read timeout.** Traefik has no limit on
  connections per address. A host firewall rule closes that gap (see below).
- **Secrets are environment variables.** Root on the server sees them with
  `docker inspect`, and so does anything in the container.
- **Shared networks are open.** Any service can join any shared network by
  naming it, so services on one must authenticate their clients.
- **Base images are trusted.** A Dockerfile's `FROM` images come from their
  registries as they are.

## LAN mode

LAN mode (see [design.md](design.md#lan-mode)) is off by default. It is not
covered by the review above, which assumes the server is on the internet. The
risks below are the ones LAN mode adds. The risks of the internet review still
apply on a LAN, and a LAN is not a trusted network: anyone on Wi-Fi can
observe it.

| Surface | Risk | Mitigation |
| --- | --- | --- |
| UDP 5353 responder in the root daemon | A parser bug is reachable by any device on the LAN, and it shares a process with Docker control | A memory-safe library on its own thread, outside the async runtime; the daemon unit stays sandboxed; LAN mode is off by default. The trait boundary lets the responder move into its own unit later. |
| Name spoofing | A deploy key claims a name that another device already has, such as `printer.local` | Probing: a name that is already held is withdrawn, not taken over. `lan.allow` limits the names. The admin and server names are reserved. A deploy key may claim any free name in `allow`, as it may claim a free public host today. |
| Information disclosure | mDNS shows which names exist | Only hosts listed in a `url` are announced, never services without one. Treat the names as public on that network. |
| Plain HTTP to services | Other devices on the LAN read the traffic | Documented limit. Do not put secrets in a service that uses `http://….local`. HTTPS for `.local` is not offered yet. |
| Plain-HTTP admin API (`plain_admin`) | Devices on the LAN read secrets, logs, bundles and signed requests; whoever wins the admin name receives the requests | Off unless set on the server; needs a `.local` domain and LAN mode. Signatures and tickets still hold. The client refuses to send secrets over it unless `--plain-secrets` is given. Set `router.api_allow`. Never forward port 80 from the internet. |
| Sandbox | `AF_NETLINK` is allowed to the daemon for interface listing | Added to the unit only when `lan.enabled` is set. |

### What stays protected with `plain_admin`

- **Authentication.** Signatures cover the host, method, path, timestamp, nonce
  and body hash. An observer cannot change a request, or reuse one after the
  server has seen it, because the nonce is spent and the time window closes.
- **Bundles.** The hash is signed in the ticket request and checked after the
  upload is streamed, so a tampered upload is deleted unopened. A ticket seen on
  the wire is bound to one exact size and hash, is single-use and expires after
  60 seconds, so at most an observer can upload the same bundle again.

### What is lost with `plain_admin`

- **Confidentiality of the API traffic.** Secret values from `secret set`, logs,
  `status` output, deploy logs and the bundle contents, which can include `.env`
  files, are readable by any device on the network.
- **Integrity of responses.** A client cannot tell a forged `result: ok` from a
  real one.
- **Server identity.** Whoever wins the admin name on the network (through mDNS
  spoofing, or while the server is offline) receives the client's requests,
  including secret values. Signatures stop it from reusing captured requests
  later, but not from reading them, or from relaying one within its 30-second
  window.
- **Exposure beyond the LAN.** If port 80 is reachable from the internet, so is
  the admin API. Traefik serves both on port 80, so a public `http://` service on
  the same server does not hide the API from outside: `router.api_allow` is then
  the only guard.

## Operator checklist

mypaas configures Docker containers and Traefik. The host itself is yours to
harden:

1. **Firewall the host.** Allow inbound 80 and 443 to everyone, SSH only from
   where you administer, and nothing else. Containers reach the host through
   their bridge gateway, so also drop traffic from Docker networks to host
   ports other than 80 and 443, for example with nftables:

   ```
   table inet filter {
     chain input {
       type filter hook input priority 0; policy drop;
       ct state established,related accept
       iif lo accept
       tcp dport { 80, 443 } ct state new add @conn_per_ip { ip saddr ct count over 256 } drop
       tcp dport { 80, 443 } accept
       ip saddr 203.0.113.0/24 tcp dport 22 accept   # your admin network
       icmp type echo-request limit rate 5/second accept
       icmpv6 type { nd-neighbor-solicit, nd-neighbor-advert, nd-router-advert, echo-request } accept
     }
     set conn_per_ip { type ipv4_addr; flags dynamic; }
   }
   ```

   The `ct count` line caps open connections per IPv4 address on 80 and 443,
   which Traefik itself cannot do. Test rules from a second session before you
   rely on them, and keep Docker's own chains: Docker manages the `FORWARD`
   path for containers, and published ports are not used by mypaas.
2. **Use dedicated keys.** Make a key per person and per pipeline with
   `mypaas key init` or `mypaas key gen`, not the `~/.ssh/id_ed25519` you also
   publish elsewhere, so key ids stay unknown. Give CI `deploy:<glob>`, never
   `admin`, and set `--expires`.
3. **Restrict the API** with `router.api_allow` if deploys come from known
   networks, then run `mypaas server restart router`.
4. **Keep the clock right** (systemd-timesyncd or chrony): signatures are
   valid for 30 seconds.
5. **Update the host**: unattended security upgrades for the kernel, Docker
   and OpenSSH, and new mypaas releases for Traefik.
6. **Harden SSH**: keys only, no root login, or no public SSH at all.
7. **Watch the logs**: `journalctl -u mypaasd` shows denied requests with
   their addresses, and `journalctl -u mypaas-socket-proxy` shows any blocked
   Docker call.
8. **Behind a CDN or another proxy**, set Traefik's
   `forwardedHeaders.trustedIPs` to that proxy, and give the rate and in-flight
   limits a `sourceCriterion` for its client header. Otherwise every request
   seems to come from the proxy, and one client's failures lock out everyone.
9. **Back up `~/_mypaas`**, encrypted: it holds the keys' scopes, the secrets
   and the volumes.
10. **LAN mode: never forward port 80 to a `plain_admin` server.** Do not
    publish port 80 through the router, and make the firewall rule in item 1
    accept 80 only from the LAN, if the server runs `plain_admin`. A server with
    public `http://` services on port 80 needs a `router.api_allow` for the
    admin name, because Traefik routes by name.
11. **LAN mode: set `router.api_allow`** to your LAN's networks, for example
    `["192.168.1.0/24", "fd12:3456:789a::/64"]` (the IPv4 range and, if the LAN
    has one, its unique local IPv6 prefix, since names get AAAA records for
    it), then run `mypaas server restart router`. Traefik then
    answers everyone else with 403. Without it, `server init` and the daemon
    warn.
12. **LAN mode: use `--plain-secrets` knowingly.** It sends a secret value over
    the LAN in clear text. Use it only on a network you trust, and change the
    secret afterwards if it went over a network you do not.
