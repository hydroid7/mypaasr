# An app that keeps its data in SQLite on a named volume.
#
# SQLite wants a single writer, so this service keeps the default scale=0:
# the old container stops before the new one starts, and two processes never
# open the database at once. (With scale above 0 the deploy warns, because the
# replicas would share the volume.)
#
# The volume lives on the server in ~/_mypaas/volumes/guestbook/data and
# survives deploys and rollbacks. It starts empty and is handed to the image's
# user (1000) on the first deploy.

# mypaas.service=guestbook
# mypaas.url=https://guestbook.example.com
# mypaas.port=8000
# mypaas.volume=data:/data
# mypaas.env=DB_PATH=/data/guestbook.db
# mypaas.readonly=true
# mypaas.maxmem=128m

FROM python:3.13-slim
WORKDIR /app
COPY app.py .
USER 1000
CMD ["python", "-u", "app.py"]
