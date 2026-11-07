# Deploying from CI

`mypaas push` needs no SSH access to the server. It runs anywhere that can
reach the admin domain over HTTPS. A CI job needs three things:

1. the `mypaas` binary,
2. a passwordless Ed25519 key in `MYPAAS_PRIVATE_KEY`, authorized on the server
   with a narrow scope,
3. the admin domain, passed with `-r` or in `MYPAAS_REMOTE`.

## 1. Create a key for the pipeline

Make one key per pipeline or repository, so you can revoke one without
touching the others. Run this on your own machine:

```sh
mypaas key gen > ci.key                                # private key on stdout
MYPAAS_PRIVATE_KEY="$(cat ci.key)" mypaas key show > ci.pub   # its public line
```

Store the contents of `ci.key` as a secret in your CI system (see below), then
delete the local copy. Copy `ci.pub` to the server.

## 2. Authorize it on the server

Keys are added on the server only, never through the API. Give the key only
what the job needs:

```sh
# on the server, as root
mypaas server key add ci.pub --name shop-ci --scope 'deploy:shop-*' --expires 2027-12-31
```

| Scope | The job can |
| --- | --- |
| `deploy:<glob>` | `push` and `rollback` for matching services. This is all a deploy job needs. |
| `read` | `ls`, `status`, `stats` and `doctor`, for example a post-deploy check |
| `logs` | `logs`, for example to attach logs to a failed job |
| `secrets:<glob>` | `secret set\|ls\|rm`. Usually set by a person, not by CI. |
| `rm:<glob>` | `rm`, for example to tear down preview environments |

Avoid giving CI `admin`. The only things that need it are `cap_add` and
host-path volumes. With `--expires` the key stops working on that day, and
`mypaas server key rm shop-ci` revokes it within 5 seconds.

Combine scopes by repeating `--scope`, for example
`--scope 'deploy:shop-*' --scope read`.

## 3. Set the service's secrets once

If the Dockerfile reads secrets (`# mypaas.env=DB_PASSWORD`), set them before
the first deploy. A deploy fails if one is missing. Do this from a machine
with a key that has the `secrets` scope:

```sh
mypaas secret set shop-web DB_PASSWORD      # hidden prompt, or pipe the value in
```

## 4. Get the binary into the job

`mypaas` is one statically linked file with no runtime dependencies. There is
no public release to download yet. Build it once with
`scripts/build-static.sh` and put
`target-musl/x86_64-unknown-linux-musl/release/mypaas` somewhere your CI can
fetch it: a package registry, an object store or an internal URL. The examples
below download it from `$MYPAAS_BIN_URL`, a CI variable you define. Pin the URL
to a version, and check a SHA-256 if your registry provides one.

The client and the server don't need to be exactly the same version, but keep
them close: the API is `/v1` and changes with mypaas releases.

## The job

The job is the same everywhere:

```sh
export MYPAAS_PRIVATE_KEY="..."            # from the CI secret store
mypaas -r paas.example.com push .          # or a Dockerfile path: push Dockerfile.prod
```

- `push` packs the directory using the project's own `.gitignore` files only,
  so the bundle and its hash are the same on every runner.
- The build runs on the server, so the runner needs no Docker.
- If another deploy is running, `push` waits and retries for up to about 45
  seconds, then gives up with exit code 1.
- `--json` prints one JSON object per line instead of text. `-q` prints only
  errors and the result.

### Exit codes

| Code | Meaning | Typical cause |
| --- | --- | --- |
| `0` | Deployed | |
| `1` | Error | Network, server busy for too long, bundle above the server's `max_upload` |
| `2` | Usage error | Wrong arguments |
| `3` | Denied or locked out | Unknown or expired key, missing scope, clock off by more than 30 s |
| `4` | Deploy failed on the server | Build error, missing secret, container not healthy; the old version keeps running |

So a pipeline can tell a credentials problem (`3`) from a broken build (`4`).
If you see `3`, run `mypaas -r paas.example.com doctor` with the same key (it
needs the `read` scope). It checks the key, TLS, the clock and a signed
request.

## GitHub Actions

Under Settings → Secrets and variables → Actions:

- add a secret `MYPAAS_PRIVATE_KEY` with the full contents of `ci.key`,
  including the `BEGIN` and `END` lines,
- add a variable `MYPAAS_BIN_URL`.

```yaml
# .github/workflows/deploy.yml
name: deploy
on:
  push:
    branches: [main]

concurrency: deploy-${{ github.ref }}   # one deploy at a time per branch

jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: production             # optional: require approval, scope the secret
    steps:
      - uses: actions/checkout@v4
      - name: Install mypaas
        run: |
          curl -fsSL "${{ vars.MYPAAS_BIN_URL }}" -o /usr/local/bin/mypaas
          chmod +x /usr/local/bin/mypaas
          mypaas version
      - name: Deploy
        env:
          MYPAAS_PRIVATE_KEY: ${{ secrets.MYPAAS_PRIVATE_KEY }}
          MYPAAS_REMOTE: paas.example.com
        run: mypaas push .
```

To deploy two services from one repository, for example staging and
production with different Dockerfiles (as in
[examples/staging-prod](../examples/staging-prod)), use one step each with
`mypaas push Dockerfile.staging` and `mypaas push Dockerfile.prod`. Give them
separate keys and environments if production needs approval.

## GitLab CI

GitLab cannot mask a variable that spans several lines, and a private key
always does. Store it as a **File** variable instead: GitLab writes the value
to a temporary file and puts that file's path in the variable. Under Settings →
CI/CD → Variables:

- add `MYPAAS_KEY_FILE`, type **File**, **Protected**, with the contents of
  `ci.key`,
- add `MYPAAS_BIN_URL`.

```yaml
# .gitlab-ci.yml
deploy:
  stage: deploy
  image: alpine:3.22
  resource_group: production            # one deploy at a time
  rules:
    - if: $CI_COMMIT_BRANCH == $CI_DEFAULT_BRANCH
  variables:
    MYPAAS_REMOTE: paas.example.com
  before_script:
    - apk add --no-cache curl
    - curl -fsSL "$MYPAAS_BIN_URL" -o /usr/local/bin/mypaas && chmod +x /usr/local/bin/mypaas
  script:
    - export MYPAAS_PRIVATE_KEY="$(cat "$MYPAAS_KEY_FILE")"
    - mypaas push .
```

Protected variables reach jobs on protected branches only, so a merge request
from a fork cannot read the key.

## Other CI systems

Anything that can run a shell step works. Put the key in a secret
environment variable named `MYPAAS_PRIVATE_KEY` (or write it to a file and
`export MYPAAS_PRIVATE_KEY="$(cat file)"`), then run `mypaas -r <domain> push`.
Make sure the key never reaches the build log: `mypaas` never prints it, but
`set -x` or `env` would.

## Rollback and checks from CI

```sh
mypaas -r paas.example.com rollback shop-web    # needs deploy:shop-web; only one step back
mypaas -r paas.example.com status shop-web --json   # needs read
mypaas -r paas.example.com logs shop-web --tail 100 # needs logs
```

## Security notes

- A deploy key runs arbitrary Dockerfile `RUN` steps on the server. Treat it
  like any other production CI credential: expose it to protected branches
  only, never to builds of untrusted pull requests.
- A scoped key cannot touch services outside its glob: it cannot deploy, roll
  back, remove or read the secrets of other services. But a service it deploys
  can join any shared network, so services on a shared network must
  authenticate their clients (one Postgres role per application, for example).
- After repeated failed requests, each source IP is locked out for a time that
  doubles with every failure, up to one hour. A misconfigured key that retries
  in a loop can lock out the runner's address. Fix the key, then wait, or
  restart the daemon.
- Signed requests are valid for 30 seconds. Runners with a badly drifting
  clock get `3`; `doctor` shows the difference.
