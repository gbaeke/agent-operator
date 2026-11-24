#!/usr/bin/env bash
# Keycloak on the kind cluster: namespace `auth`, its own Postgres, the realm
# `agents` (users alice, bob and admin) and a CoreDNS rewrite so pods reach
# Keycloak at the same URL as the browser, http://$AUTH_HOST:32001.
#
# AUTH_HOST is the name your browser uses for this machine. Default: its
# tailnet name if tailscale runs, else keycloak.localtest.me (127.0.0.1).
# The console must be opened on the same host (http://$AUTH_HOST:32000).
# It is fixed when Keycloak is first deployed; to change it, delete and apply.
#
#   deploy/auth.sh            apply, wait until Keycloak is ready, put sign-in on the console
#   deploy/auth.sh users      print the demo users and their password
#   deploy/auth.sh token U    a test access token for user U (agent-cli client)
#   deploy/auth.sh delete     take sign-in off the console; remove Keycloak, its data and the CoreDNS rewrite
#
# In the `agents` namespace it only adds the Secret console-oidc and patches the
# console (deploy/auth/console-patch.yaml); delete undoes both.
# KIND_CLUSTER picks the cluster (default: agents).
set -euo pipefail

CLUSTER="${KIND_CLUSTER:-agents}"
DIR="$(dirname "$(realpath "$0")")/auth"
MARK="# agent-operator: keycloak"
k() { kubectl --context "kind-$CLUSTER" "$@"; }
secret() { k -n auth get secret "$1" -o jsonpath="{.data.$2}" | base64 -d; }

# The deployed host wins, then AUTH_HOST, then the tailnet name, then localtest.me.
deployed_host() { k -n auth get configmap keycloak-host -o jsonpath='{.data.host}' 2>/dev/null || true; }
tailnet_host() { tailscale status --self --json 2>/dev/null | python3 -c 'import json, sys; print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))' 2>/dev/null || true; }
HOST="$(deployed_host)"
if [[ -n "$HOST" && -n "${AUTH_HOST:-}" && "$AUTH_HOST" != "$HOST" ]]; then
  echo "Keycloak is deployed for $HOST; to use $AUTH_HOST, run: $0 delete, then apply again" >&2
  exit 1
fi
HOST="${HOST:-${AUTH_HOST:-$(tailnet_host)}}"
HOST="${HOST:-keycloak.localtest.me}"
ISSUER="http://$HOST:32001/realms/agents"

# Made once and then kept: new values would not match the database or the imported realm.
secrets() {
  local name
  for name in keycloak-db keycloak-admin; do
    k -n auth get secret "$name" >/dev/null 2>&1 ||
      k -n auth create secret generic "$name" --from-literal=password="$(openssl rand -hex 24)"
  done
  k -n auth get secret keycloak-realm >/dev/null 2>&1 ||
    k -n auth create secret generic keycloak-realm \
      --from-literal=demo-password="$(openssl rand -hex 6)" \
      --from-literal=console-client-secret="$(openssl rand -hex 24)"
}

# Pods resolve $HOST to the Keycloak Service instead of to this machine. Nothing in the
# cluster may need the real $HOST; only Keycloak's port 32001 answers on it there.
coredns_add() {
  local corefile
  corefile="$(k -n kube-system get configmap coredns -o jsonpath='{.data.Corefile}')"
  grep -qF "$MARK" <<<"$corefile" && return
  corefile="$(awk -v mark="$MARK" -v host="$HOST" '
    { print }
    /^ *ready$/ {
      print "    " mark
      print "    rewrite stop {"
      print "        name exact " host " keycloak.auth.svc.cluster.local"
      print "        answer auto"
      print "    }"
      print "    " mark " end"
    }' <<<"$corefile")"
  k -n kube-system create configmap coredns --from-literal=Corefile="$corefile" --dry-run=client -o yaml | k apply -f - >/dev/null
  k -n kube-system rollout restart deploy/coredns >/dev/null
  k -n kube-system rollout status deploy/coredns --timeout=60s >/dev/null
  echo "CoreDNS: $HOST -> keycloak.auth.svc.cluster.local"
}

coredns_remove() {
  local corefile
  corefile="$(k -n kube-system get configmap coredns -o jsonpath='{.data.Corefile}')"
  grep -qF "$MARK" <<<"$corefile" || return 0
  corefile="$(awk -v mark="$MARK" 'index($0, mark) { skip = !skip; next } !skip' <<<"$corefile")"
  k -n kube-system create configmap coredns --from-literal=Corefile="$corefile" --dry-run=client -o yaml | k apply -f - >/dev/null
  k -n kube-system rollout restart deploy/coredns >/dev/null
  echo "CoreDNS rewrite removed"
}

apply() {
  k apply -f "$DIR/keycloak.yaml" >/dev/null
  k -n auth create configmap keycloak-host --from-literal=host="$HOST" --dry-run=client -o yaml | k apply -f - >/dev/null
  secrets
  k -n auth create configmap keycloak-realm --from-file=agents-realm.json="$DIR/realm.json" --dry-run=client -o yaml | k apply -f - >/dev/null
  coredns_add
  k -n auth rollout status statefulset/postgres --timeout=180s
  k -n auth rollout status deploy/keycloak --timeout=300s
  console_on
  echo
  echo "Keycloak admin: http://$HOST:32001/admin  (kcadmin / $(secret keycloak-admin password))"
  echo "issuer:         $ISSUER"
  echo "console:        http://$HOST:32000 (sign in as one of the users below)"
  users
}

users() {
  echo "users:          alice (sales), bob (engineering), admin (platform-admins), password $(secret keycloak-realm demo-password)"
}

token() {
  curl -fsS "$ISSUER/protocol/openid-connect/token" \
    --data-urlencode grant_type=password --data-urlencode client_id=agent-cli \
    --data-urlencode username="$1" --data-urlencode password="$(secret keycloak-realm demo-password)" |
    python3 -c 'import json, sys; print(json.load(sys.stdin)["access_token"])'
}

# oauth2-proxy in front of the console, and the console checking tokens.
console_on() {
  if ! k -n agents get secret console-oidc >/dev/null 2>&1; then
    k -n agents create secret generic console-oidc \
      --from-literal=OAUTH2_PROXY_CLIENT_SECRET="$(secret keycloak-realm console-client-secret)" \
      --from-literal=OAUTH2_PROXY_COOKIE_SECRET="$(openssl rand -hex 16)" \
      --from-literal=OAUTH2_PROXY_OIDC_ISSUER_URL="$ISSUER" \
      --from-literal=OAUTH2_PROXY_REDIRECT_URL="http://$HOST:32000/oauth2/callback" \
      --from-literal=OAUTH2_PROXY_BACKEND_LOGOUT_URL="$ISSUER/protocol/openid-connect/logout?id_token_hint={id_token}"
  fi
  k -n agents patch deployment agent-console --type strategic --patch-file "$DIR/console-patch.yaml" >/dev/null
  k -n agents patch service agent-console --type strategic -p '{"spec":{"ports":[{"port":8080,"targetPort":4180}]}}' >/dev/null
  k -n agents rollout status deploy/agent-console --timeout=120s
}

console_off() {
  k -n agents get deployment agent-console >/dev/null 2>&1 || return 0
  k -n agents patch service agent-console --type strategic -p '{"spec":{"ports":[{"port":8080,"targetPort":8080}]}}' >/dev/null
  k -n agents patch deployment agent-console --type strategic --patch-file "$DIR/console-unpatch.yaml" >/dev/null
  k -n agents delete --ignore-not-found secret console-oidc
  k -n agents rollout status deploy/agent-console --timeout=120s
}

delete() {
  console_off
  k delete --ignore-not-found namespace auth
  coredns_remove
}

case "${1:-apply}" in
  apply) apply ;;
  users) users ;;
  token) token "${2:?usage: $0 token <user>}" ;;
  delete) delete ;;
  *) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
