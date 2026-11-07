#!/bin/sh
# Creates one login role and one database per app listed in MYPAAS_DB_APPS.
# The password of app "shop" comes from the secret SHOP_DB_PASSWORD.
set -eu

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres \
  -c "REVOKE CONNECT ON DATABASE postgres FROM PUBLIC;"

for app in $(echo "${MYPAAS_DB_APPS:-}" | tr ',' ' '); do
  var="$(echo "$app" | tr 'a-z-' 'A-Z_')_DB_PASSWORD"
  password="$(printenv "$var" || true)"
  if [ -z "$password" ]; then
    echo "init-app-roles: $var is not set, skipping $app" >&2
    continue
  fi
  psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres \
    -v app="$app" -v pw="$password" <<'SQL'
CREATE ROLE :"app" LOGIN PASSWORD :'pw';
CREATE DATABASE :"app" OWNER :"app";
REVOKE ALL ON DATABASE :"app" FROM PUBLIC;
SQL
  echo "init-app-roles: created role and database $app"
done
