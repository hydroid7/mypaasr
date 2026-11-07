# Examples

Each directory is a complete, deployable service. All configuration is in the
`# mypaas.*` comments at the top of each Dockerfile, and each comment block
explains why the service is set up the way it is. Copy the one closest to what
you need, change the `service` and `url` lines, and push:

```sh
mypaas push examples/python-web
mypaas push examples/staging-prod/Dockerfile.prod     # a specific Dockerfile
mypaas push --dry-run examples/go-api                 # what would be uploaded
```

Every example runs with mypaas' defaults: all capabilities dropped,
`no-new-privileges`, a non-root user, and memory and CPU limits.
`cargo test` parses every Dockerfile here, so the examples always match what
mypaas accepts.

| Example | Shows | Key settings |
| --- | --- | --- |
| [static-site](static-site) | Static files with nginx; two domains; never mixing old and new assets | `scale=2 safe`, `readonly`, two `url` lines |
| [python-web](python-web) | A web app with zero-downtime deploys, a secret, and the same app under a path on another domain | `scale=3 roll`, `healthcheck`, `env=API_TOKEN`, `url=https://example.com/api` |
| [sqlite-app](sqlite-app) | Data in SQLite on a named volume, with a single writer | `scale=0` (stop first), `volume=data:/data` |
| [staging-prod](staging-prod) | Staging and production from one project, as two independent services | `Dockerfile.staging`, `Dockerfile.prod`, separate service names |
| [go-api](go-api) | A multi-stage build to a `scratch` image | `FROM … AS build`, `USER 65532`, `healthcheck` |
| [redis-worker](redis-worker) | An internal service with no URL, and a background worker that talks to it over a shared network | `network=cache`, no `url`, a password in each service's secrets |
| [postgres](postgres) | One Postgres shared by several apps, each with its own role and database | `network=db`, `volume`, init script, per-app secrets |

## Before you deploy

- **Secrets.** A line like `# mypaas.env=API_TOKEN` (no `=value`) reads the
  value from the service's secrets on the server. Set it first, or the deploy
  fails and names the missing secret:

  ```sh
  mypaas secret set hello-api API_TOKEN
  ```

- **DNS.** Point each `url` host at the server. Traefik gets a Let's Encrypt
  certificate on the first request.
- **Order.** Deploy a shared service (Redis, Postgres) before the services
  that use it.

## Which strategy?

| You have | Use | Why |
| --- | --- | --- |
| A database file or any single writer | no `scale` (or `scale=0`) | The old container stops before the new one starts |
| A site whose HTML and assets must match | `scale=N safe` | All old replicas stop before new ones start; a few seconds of downtime |
| A stateless app that must stay up | `scale=N roll` with a `healthcheck` | A new replica must be healthy before an old one stops |

The full list of comment keys is in the
[README](../README.md#configuring-a-service) and in the
[design document](../docs/design.md#describing-a-service). For deploying
these from CI, see [docs/ci.md](../docs/ci.md).
