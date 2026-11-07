# Redis for internal use: no url, so Traefik never routes to it, and no host
# port. Services reach it as `redis` on the shared network `cache`, which any
# service can join by naming it, so Redis requires a password
# (mypaas secret set redis REDIS_PASSWORD, and the same value for the worker).
#
# Deploy this first, then the worker:
#   mypaas push examples/redis-worker
#   mypaas push examples/redis-worker/worker

# mypaas.service=redis
# mypaas.network=cache
# mypaas.volume=data:/data
# mypaas.env=REDIS_PASSWORD
# mypaas.maxmem=256m

FROM redis:7.4-alpine
USER redis
# The password comes from the environment, so it is not part of the image.
CMD ["sh", "-c", "exec redis-server --appendonly yes --dir /data --requirepass \"$REDIS_PASSWORD\""]
