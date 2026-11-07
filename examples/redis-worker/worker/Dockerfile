# A background worker: no url and no port, it only talks to Redis over the
# shared network `cache`. Without a url there is nothing to health-probe, so
# the deploy checks that the container is still running after 5 seconds.
#
# Its secret has its own name space: set it for this service too,
#   mypaas secret set ticker REDIS_PASSWORD
# because a service can only read its own secrets.

# mypaas.service=ticker
# mypaas.network=cache
# mypaas.env=REDIS_PASSWORD
# mypaas.env=REDIS_HOST=redis
# mypaas.maxmem=32m
# mypaas.maxcpu=0.1
# mypaas.readonly=true

FROM redis:7.4-alpine
USER nobody
# Counts a tick every 10 seconds; `mypaas logs ticker -f` shows the count.
CMD ["sh", "-c", "export REDISCLI_AUTH=\"$REDIS_PASSWORD\"; while true; do redis-cli -h \"$REDIS_HOST\" INCR ticks; sleep 10; done"]
