#!/usr/bin/env bash
# Tests mypaas on a real systemd server: a local Debian 13 VM under QEMU/KVM.
#
#   scripts/vm-test.sh up      boot the VM (downloads the cloud image once)
#   scripts/vm-test.sh test    install mypaas in the VM and run the checks
#   scripts/vm-test.sh ssh     open a shell in the VM
#   scripts/vm-test.sh down    stop the VM and delete its disk
#
# Needs qemu-system-x86_64, qemu-img, genisoimage and a static binary from
# scripts/build-static.sh. The VM uses user-mode networking: only loopback
# ports on this machine are forwarded to it, nothing else can reach it.
# Let's Encrypt is not part of this test; Traefik serves its default
# certificate and the client reaches the daemon through an SSH tunnel.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
STATE="$ROOT/target-vm"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/mypaas-vm"
IMAGE_URL="https://cloud.debian.org/images/cloud/trixie/latest"
IMAGE_NAME="debian-13-genericcloud-amd64.qcow2"
BIN="$ROOT/target-musl/x86_64-unknown-linux-musl/release/mypaas"
SSH_PORT="${VM_SSH_PORT:-2229}"
HTTP_PORT="${VM_HTTP_PORT:-8089}"
HTTPS_PORT="${VM_HTTPS_PORT:-8449}"
TUNNEL_PORT="${VM_TUNNEL_PORT:-18888}"
DOMAIN="paas.vm.test"

ssh_opts=(-i "$STATE/vm_key" -p "$SSH_PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR -o ConnectTimeout=5)
vm() { ssh "${ssh_opts[@]}" mypaas@127.0.0.1 "$@"; }
vm_copy() { scp -q -i "$STATE/vm_key" -P "$SSH_PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR "$1" "mypaas@127.0.0.1:$2"; }

fetch_image() {
  mkdir -p "$CACHE"
  if [[ ! -f "$CACHE/$IMAGE_NAME" ]]; then
    echo "downloading $IMAGE_NAME"
    curl -fL --progress-bar -o "$CACHE/$IMAGE_NAME.part" "$IMAGE_URL/$IMAGE_NAME"
    curl -fsSL -o "$CACHE/SHA512SUMS" "$IMAGE_URL/SHA512SUMS"
    want=$(grep " $IMAGE_NAME\$" "$CACHE/SHA512SUMS" | cut -d' ' -f1)
    have=$(sha512sum "$CACHE/$IMAGE_NAME.part" | cut -d' ' -f1)
    [[ -n "$want" && "$want" == "$have" ]] || { echo "error: checksum mismatch for $IMAGE_NAME" >&2; exit 1; }
    mv "$CACHE/$IMAGE_NAME.part" "$CACHE/$IMAGE_NAME"
  fi
}

wait_for_ssh() {
  local deadline=$((SECONDS + ${1:-300}))
  until vm true 2>/dev/null; do
    (( SECONDS < deadline )) || { echo "error: the VM did not answer on ssh; see $STATE/console.log" >&2; exit 1; }
    sleep 3
  done
}

cmd_up() {
  if [[ -f "$STATE/vm.pid" ]] && kill -0 "$(cat "$STATE/vm.pid")" 2>/dev/null; then
    echo "the VM is already running"
    return
  fi
  fetch_image
  mkdir -p "$STATE"
  [[ -f "$STATE/vm_key" ]] || ssh-keygen -q -t ed25519 -N '' -C mypaas-vm-test -f "$STATE/vm_key"
  if [[ ! -f "$STATE/disk.qcow2" ]]; then
    qemu-img create -q -f qcow2 -b "$CACHE/$IMAGE_NAME" -F qcow2 "$STATE/disk.qcow2" 20G
  fi
  cat > "$STATE/user-data" <<EOF
#cloud-config
hostname: mypaas-vm
users:
  - name: mypaas
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    ssh_authorized_keys:
      - $(cat "$STATE/vm_key.pub")
package_update: true
packages: [docker.io, docker-buildx, curl]
runcmd:
  - systemctl enable --now docker
EOF
  printf 'instance-id: mypaas-vm\nlocal-hostname: mypaas-vm\n' > "$STATE/meta-data"
  genisoimage -quiet -output "$STATE/seed.iso" -volid cidata -joliet -rock "$STATE/user-data" "$STATE/meta-data"
  qemu-system-x86_64 \
    -name mypaas-vm -machine accel=kvm -cpu host -smp 4 -m 4096 \
    -drive file="$STATE/disk.qcow2",if=virtio \
    -drive file="$STATE/seed.iso",if=virtio,format=raw,readonly=on \
    -netdev user,id=n0,hostfwd=tcp:127.0.0.1:$SSH_PORT-:22,hostfwd=tcp:127.0.0.1:$HTTP_PORT-:80,hostfwd=tcp:127.0.0.1:$HTTPS_PORT-:443 \
    -device virtio-net-pci,netdev=n0 \
    -display none -serial file:"$STATE/console.log" \
    -daemonize -pidfile "$STATE/vm.pid"
  echo "booting; waiting for ssh on 127.0.0.1:$SSH_PORT"
  wait_for_ssh 300
  echo "waiting for cloud-init to install Docker"
  vm "cloud-init status --wait >/dev/null; docker --version; docker buildx version" || true
  echo "the VM is up"
}

cmd_down() {
  if [[ -f "$STATE/vm.pid" ]]; then
    kill "$(cat "$STATE/vm.pid")" 2>/dev/null || true
    rm -f "$STATE/vm.pid"
  fi
  rm -f "$STATE/disk.qcow2" "$STATE/seed.iso" "$STATE/console.log"
  echo "the VM is stopped and its disk deleted (the downloaded image stays in $CACHE)"
}

cmd_ssh() { vm -t "$@"; }

# ---- checks ----

failures=0
pass() { printf '\033[32mPASS\033[0m %s\n' "$1"; }
fail() { printf '\033[31mFAIL\033[0m %s\n' "$1"; failures=$((failures + 1)); }
check() { local name="$1"; shift; if "$@" >/dev/null 2>&1; then pass "$name"; else fail "$name"; fi; }
# Like check, but the command's output must contain $2.
expect() {
  local name="$1" want="$2"; shift 2
  local got; got=$("$@" 2>&1) || true
  if [[ "$got" == *"$want"* ]]; then pass "$name"; else fail "$name (wanted '$want', got: ${got:0:300})"; fi
}

client() {
  MYPAAS_PRIVATE_KEY="$(cat "$STATE/client.key")" XDG_CONFIG_HOME="$STATE/client-config" \
    "$BIN" -r "http://127.0.0.1:$TUNNEL_PORT" "$@"
}

tunnel_up() {
  tunnel_down
  ssh "${ssh_opts[@]}" -f -N -o ExitOnForwardFailure=yes -L "127.0.0.1:$TUNNEL_PORT:127.0.0.1:88" mypaas@127.0.0.1
}

tunnel_down() { pkill -f -- "-L 127.0.0.1:$TUNNEL_PORT:127.0.0.1:88" 2>/dev/null || true; }

http() { curl -s --max-time 5 -H "Host: $1" "http://127.0.0.1:$HTTP_PORT$2"; }

wait_until() {
  local seconds="$1"; shift
  local deadline=$((SECONDS + seconds))
  until "$@" >/dev/null 2>&1; do
    (( SECONDS < deadline )) || return 1
    sleep 2
  done
}

project() {
  local dir="$STATE/projects/$1"
  mkdir -p "$dir"
  cat > "$dir/Dockerfile"
  echo "$dir"
}

cmd_test() {
  [[ -x "$BIN" ]] || { echo "error: build the static binary first: scripts/build-static.sh" >&2; exit 1; }
  wait_for_ssh 60
  vm "cloud-init status --wait >/dev/null"

  echo "== install"
  "$BIN" key gen > "$STATE/client.key" 2> "$STATE/client.txt"
  tail -1 "$STATE/client.txt" > "$STATE/client.pub"
  vm_copy "$BIN" /tmp/mypaas
  vm_copy "$STATE/client.pub" /tmp/client.pub
  vm "sudo install -m 0755 /tmp/mypaas /usr/local/bin/mypaas"
  expect "server init writes the systemd units" "wrote the systemd units" \
    vm "sudo mypaas server init --domain $DOMAIN --email '' --key /tmp/client.pub"
  expect "server init is safe to run again" "already authorized" \
    vm "sudo mypaas server init --domain $DOMAIN --email '' --key /tmp/client.pub"
  vm "sudo grep -q '^\[networks.db\]' /root/_mypaas/config.toml || printf '\n[networks.db]\nmembers = [\"postgres\", \"shop\", \"blog\"]\n' | sudo tee -a /root/_mypaas/config.toml >/dev/null"
  expect "restart all starts Traefik and the services" "restarted mypaasd" vm "sudo mypaas server restart all"

  echo "== systemd"
  wait_until 30 vm "systemctl is-active mypaasd mypaas-socket-proxy" || true
  check "mypaasd is active" vm "systemctl is-active mypaasd"
  check "mypaas-socket-proxy is active" vm "systemctl is-active mypaas-socket-proxy"
  check "both units are enabled for boot" vm "systemctl is-enabled mypaasd mypaas-socket-proxy"
  check "the router container is running" vm "sudo docker inspect -f '{{.State.Running}}' mypaas-router | grep -q true"
  check "the daemon listens on loopback only" vm "sudo ss -Hltn 'sport = :88' | grep -q '127.0.0.1:88'"
  expect "the daemon unit is sandboxed" "PrivateTmp=yes" vm "systemctl show mypaasd -p PrivateTmp --value | sed 's/^/PrivateTmp=/'"
  expect "the proxy has no network" "yes" vm "systemctl show mypaas-socket-proxy -p PrivateNetwork --value"
  vm "systemd-analyze security mypaasd mypaas-socket-proxy --no-pager 2>/dev/null | tail -2" | sed 's/^/     /'

  echo "== Traefik in front of the daemon"
  expect "unsigned API request gets the uniform 403" '"error":"denied"' \
    curl -sk --max-time 5 --resolve "$DOMAIN:$HTTPS_PORT:127.0.0.1" "https://$DOMAIN:$HTTPS_PORT/v1/services"
  expect "unknown paths on the admin domain are 404" "404" \
    curl -sk -o /dev/null -w '%{http_code}' --resolve "$DOMAIN:$HTTPS_PORT:127.0.0.1" "https://$DOMAIN:$HTTPS_PORT/admin"
  expect "plain HTTP redirects to HTTPS" "301" curl -s -o /dev/null -w '%{http_code}' -H "Host: $DOMAIN" "http://127.0.0.1:$HTTP_PORT/v1/services"
  expect "the daemon answers on port 443 from outside the VM" "denied" \
    curl -sk --max-time 5 -X PUT -H "Authorization: Bearer $(printf 'ab%.0s' {1..32})" -H "Content-Length: 999999999" \
    --resolve "$DOMAIN:$HTTPS_PORT:127.0.0.1" "https://$DOMAIN:$HTTPS_PORT/v1/deploy"

  echo "== client"
  tunnel_up
  expect "doctor accepts the key" "a signed GET /v1/services was accepted" client doctor
  local web
  web=$(project web <<EOF
# mypaas.service=web
# mypaas.url=http://web.vm.test
# mypaas.port=8080
# mypaas.scale=2 roll
# mypaas.healthcheck=/ 2s 1s
# mypaas.volume=data:/data
FROM busybox:1.36
RUN mkdir /www && echo hello-from-the-vm > /www/index.html
USER 65534
CMD ["sh", "-c", "id -u > /data/owner && exec httpd -f -p 8080 -h /www"]
EOF
)
  expect "push builds and deploys under systemd" "result: ok" client push "$web"
  if wait_until 30 sh -c "curl -s --max-time 5 -H 'Host: web.vm.test' http://127.0.0.1:$HTTP_PORT/ | grep -q hello-from-the-vm"; then
    pass "Traefik routes to the service through the socket proxy"
  else
    fail "Traefik routes to the service through the socket proxy"
  fi
  expect "an empty volume is given to the image's user" "65534" vm "sudo cat /root/_mypaas/volumes/web/data/owner; sudo stat -c %u /root/_mypaas/volumes/web/data"
  expect "ls shows the service healthy" "healthy" client ls
  # Ask the proxy what Traefik would see, from a throwaway container.
  local proxy_curl="sudo docker run --rm --user 0 --network none -v /root/_mypaas/run:/r:ro curlimages/curl:8.11.1 -s --unix-socket /r/docker.sock"
  expect "the socket proxy hides environment variables from Traefik" '"Env":[]' vm "$proxy_curl http://d/v1.45/containers/web.1/json"
  expect "the socket proxy blocks writes" "blocked" vm "$proxy_curl -X POST http://d/v1.45/containers/create"
  expect "the socket proxy blocks image listing" "blocked" vm "$proxy_curl http://d/v1.45/images/json"

  echo "== shared Postgres (phase 3 exit test)"
  for k in POSTGRES_PASSWORD:admin-pw SHOP_DB_PASSWORD:shop-pw BLOG_DB_PASSWORD:blog-pw; do
    echo "${k#*:}" | client -q secret set postgres "${k%%:*}"
  done
  echo shop-pw | client -q secret set shop PGPASSWORD
  echo blog-pw | client -q secret set blog PGPASSWORD
  mkdir -p "$STATE/projects/shop" "$STATE/projects/blog"
  cp "$ROOT/examples/postgres/client/Dockerfile" "$STATE/projects/shop/Dockerfile"
  sed 's/service=shop/service=blog/' "$ROOT/examples/postgres/client/Dockerfile" > "$STATE/projects/blog/Dockerfile"
  expect "postgres deploys with all capabilities dropped" "result: ok" client push "$ROOT/examples/postgres"
  expect "logs stream from the containers" "[postgres] " client logs postgres --tail 5
  client -q push "$STATE/projects/shop" >/dev/null; client -q push "$STATE/projects/blog" >/dev/null
  wait_until 60 vm "sudo docker exec shop psql -h postgres -U shop -d shop -tAc 'select 1'" || true
  expect "shop reaches its own database" "shop@shop" \
    vm "sudo docker exec shop psql -h postgres -U shop -d shop -tAc \"select current_user || '@' || current_database()\""
  expect "shop cannot reach blog's database" "CONNECT privilege" \
    vm "sudo docker exec shop psql -h postgres -U shop -d blog -tAc 'select 1' 2>&1"
  if vm "sudo docker exec web.1 wget -q -T 3 -O- http://postgres:5432/" >/dev/null 2>&1; then
    fail "web, not on the db network, cannot reach postgres"
  else
    pass "web, not on the db network, cannot reach postgres"
  fi

  echo "== traffic stats"
  for _ in $(seq 20); do http web.vm.test / >/dev/null; done
  for _ in $(seq 5); do http web.vm.test /missing >/dev/null; done
  if wait_until 40 sh -c "MYPAAS_PRIVATE_KEY=\"\$(cat '$STATE/client.key')\" XDG_CONFIG_HOME='$STATE/client-config' '$BIN' -r http://127.0.0.1:$TUNNEL_PORT --json stats web | grep -q '\"4xx\":5'"; then
    pass "stats count the requests sent through Traefik"
  else
    fail "stats count the requests sent through Traefik: $(client --json stats web | head -c 300)"
  fi
  expect "the access log holds no client addresses or paths" "clean" \
    vm "sudo grep -qE 'ClientHost|RequestPath|/missing' /root/_mypaas/logs/access.log && echo dirty || echo clean"
  expect "the daemon logs to the journal" "mypaas daemon listening" vm "sudo journalctl -u mypaasd --no-pager -b"

  echo "== reboot"
  tunnel_down
  vm "sudo systemctl reboot" || true
  sleep 10
  wait_for_ssh 180
  wait_until 90 vm "systemctl is-active mypaasd mypaas-socket-proxy" || true
  check "mypaasd is back after a reboot" vm "systemctl is-active mypaasd"
  check "the socket proxy is back after a reboot" vm "systemctl is-active mypaas-socket-proxy"
  if wait_until 90 sh -c "curl -s --max-time 5 -H 'Host: web.vm.test' http://127.0.0.1:$HTTP_PORT/ | grep -q hello-from-the-vm"; then
    pass "the service is served again after a reboot"
  else
    fail "the service is served again after a reboot"
  fi
  tunnel_up
  expect "stats survive the reboot" '"4xx":5' client --json stats web
  expect "rollback after a reboot sees the deploy history" "no previous version" client rollback web
  expect "schedule-reboot creates a timer" "reboots at" vm "sudo mypaas server schedule-reboot 'Sun 06:00:00'"
  check "the reboot timer is active" vm "systemctl is-active mypaas-reboot.timer"
  vm "sudo systemctl disable --now mypaas-reboot.timer" >/dev/null 2>&1 || true

  echo "== cleanup"
  for s in shop blog postgres web; do client -q rm "$s" >/dev/null || true; done
  tunnel_down
  echo
  if (( failures == 0 )); then echo "all checks passed"; else echo "$failures check(s) failed"; exit 1; fi
}

case "${1:-}" in
  up) cmd_up ;;
  test) cmd_test ;;
  ssh) shift; cmd_ssh "$@" ;;
  down) cmd_down ;;
  *) sed -n '2,13p' "$0"; exit 2 ;;
esac
