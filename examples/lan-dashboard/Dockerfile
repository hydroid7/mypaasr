# A start page for the local network at http://dashboard.local: a grid of
# links to other services, edited in the browser.
#
# The links live in one JSON file on a named volume, so they survive deploys
# and rollbacks. On the server it is
# ~/_mypaas/volumes/dashboard/data/links.json; it can also be edited by hand,
# and the page picks up the change on the next load. The app replaces the file
# atomically (temp file and rename) and keeps the previous version as
# links.json.bak.
#
# One file and one writer, so the default scale=0 applies: the old container
# stops before the new one starts.
#
# Anyone who can reach dashboard.local can change the links. Plain HTTP on a
# LAN is readable by the other devices there, so keep this to links and do not
# add secrets. The app refuses requests from other web pages (it checks the
# Origin and requires a JSON content type) so a random site you visit cannot
# rewrite the dashboard through your browser.
#
# A link without an emoji shows the linked site's /favicon.ico, loaded by the
# browser, or the first letter of its name if there is none.
#
# Needs LAN mode on the server (see the README, "LAN mode").

# mypaas.service=dashboard
# mypaas.url=http://dashboard.local
# mypaas.port=8080
# mypaas.volume=data:/data
# mypaas.env=DATA_FILE=/data/links.json
# mypaas.readonly=true
# mypaas.maxmem=64m
# mypaas.maxcpu=0.25

FROM python:3.13-slim
WORKDIR /app
COPY app.py index.html ./
USER 1000
CMD ["python", "-u", "app.py"]
