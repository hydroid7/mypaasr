# One Postgres shared by several services over the `db` network. Every
# service that names `db` joins it; nothing is configured on the server. The
# network only connects the containers: each app still gets its own role,
# which may connect to its own database only.
#
# Secrets (mypaas secret set postgres <KEY>):
#   POSTGRES_PASSWORD, SHOP_DB_PASSWORD, BLOG_DB_PASSWORD
#
# Each app in MYPAAS_DB_APPS gets its own role and database, and may connect
# to that database only. Roles are created when the volume is first initialized.

# mypaas.service=postgres
# mypaas.network=db
# mypaas.volume=data:/var/lib/postgresql/data
# mypaas.env=POSTGRES_PASSWORD
# mypaas.env=SHOP_DB_PASSWORD
# mypaas.env=BLOG_DB_PASSWORD
# mypaas.env=MYPAAS_DB_APPS=shop,blog
# mypaas.maxmem=1g

FROM postgres:17-alpine
COPY init-app-roles.sh /docker-entrypoint-initdb.d/
# Running as postgres means the container needs no capabilities; mypaas hands
# the empty data volume to this user on the first deploy.
USER postgres
