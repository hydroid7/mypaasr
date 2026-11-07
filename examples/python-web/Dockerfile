# A small web app with zero-downtime deploys.
#
# Three replicas in roll mode: a new replica must answer /health before an old
# one stops, so the site stays up during a deploy. The app is also served under
# a path on another domain; Traefik passes the /api prefix through, so the app
# handles both / and /api/.
#
# Before the first deploy, set the secret:
#   mypaas secret set hello-api API_TOKEN

# mypaas.service=hello-api
# mypaas.url=https://api.example.com
# mypaas.url=https://example.com/api
# mypaas.port=8000
# mypaas.scale=3 roll
# mypaas.healthcheck=/health 10s 2s
# mypaas.env=GREETING=hello
# mypaas.env=API_TOKEN
# mypaas.readonly=true
# mypaas.maxmem=128m

FROM python:3.13-slim
WORKDIR /app
COPY app.py .
USER 1000
CMD ["python", "-u", "app.py"]
