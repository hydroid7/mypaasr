# A static website served by nginx, as a non-root user.
#
# Two replicas in safe mode: on a deploy all old containers stop before the
# new ones start, so a visitor never gets new HTML with old CSS. Use roll
# mode instead if a few seconds of downtime per deploy are not acceptable.

# mypaas.service=website
# mypaas.url=https://www.example.com
# mypaas.url=https://example.com
# mypaas.port=8080
# mypaas.scale=2 safe
# mypaas.healthcheck=/ 10s 2s
# mypaas.readonly=true
# mypaas.maxmem=64m
# mypaas.maxcpu=0.25

FROM nginxinc/nginx-unprivileged:1.27-alpine
COPY public/ /usr/share/nginx/html/
