# A static page announced on the local network as http://lan-app.local.
#
# Nothing here reaches the internet: the name exists only when LAN mode is on
# (see the README, "LAN mode"), and only devices on the same network resolve
# it. The name must end in .local, and LAN mode does not serve HTTPS for it, so
# the URL is http://. Plain HTTP on a LAN is readable by the other devices on
# that network, so do not put secrets in this service.

# mypaas.service=lan-app
# mypaas.url=http://lan-app.local
# mypaas.port=8080
# mypaas.readonly=true
# mypaas.maxmem=64m
# mypaas.maxcpu=0.25

FROM python:3.13-slim
WORKDIR /app
COPY index.html .
USER 1000
CMD ["python", "-m", "http.server", "8080"]
