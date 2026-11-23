#!/usr/bin/env bash
# Deploys the platform to the kind cluster: the Agent CRD, the operator, the
# console, agentgateway with the models, the shared MCP server, Postgres for
# sessions and the network policies. Agents themselves are Agent resources:
# kubectl apply them (deploy/samples/ has three).
#
#   deploy/deploy.sh          build the images, load them into kind, apply, wait
#   deploy/deploy.sh apply    apply the manifests only (no build)
#   deploy/deploy.sh models   copy the models (and keys) from the host's agentgateway again
#   deploy/deploy.sh delete   remove everything, including all Agents and the session database
#
# KIND_CLUSTER picks the cluster (default: agents). Run kind/cluster.sh up first.
# The models come from the standalone agentgateway config on this machine
# ($AGENTGATEWAY_CONFIG, default ~/.config/agentgateway/config.yaml).
set -euo pipefail

CLUSTER="${KIND_CLUSTER:-agents}"
ROOT="$(dirname "$(realpath "$0")")/.."
DEPLOY="$ROOT/deploy"
TAG=dev
IMAGES=(agent-runtime agent-operator agent-console mcp-server)
k() { kubectl --context "kind-$CLUSTER" "$@"; }

build() {
  for image in "${IMAGES[@]}"; do
    echo "building $image:$TAG"
    docker build -q --target "$image" -t "$image:$TAG" "$ROOT" >/dev/null
  done
  kind load docker-image --name "$CLUSTER" "${IMAGES[@]/%/:$TAG}"
}

gateway_controller() {
  k get gatewayclass agentgateway >/dev/null 2>&1 || KIND_CLUSTER="$CLUSTER" "$ROOT/kind/agentgateway.sh" install
}

# The keys go from the host config straight into a Secret, never to the terminal or a file.
models() {
  "$DEPLOY/gateway/models-from-host.py" | k apply -f -
}

secrets() {
  # Made once and then kept: a new password would lock the agents out of the existing database.
  if ! k -n agents get secret postgres >/dev/null 2>&1; then
    local password
    password="$(openssl rand -hex 24)"
    k -n agents create secret generic postgres --from-env-file=/dev/stdin <<<"password=$password"
    k -n agents create secret generic agent-sessions --from-env-file=/dev/stdin \
      <<<"url=postgres://agent:$password@postgres.agents:5432/sessions"
  fi
}

apply() {
  gateway_controller
  k apply -f "$DEPLOY/namespaces.yaml" -f "$DEPLOY/crd.yaml"
  k wait --for=condition=Established crd/agents.agents.baeke.info --timeout=60s >/dev/null
  secrets
  k apply -f "$DEPLOY/postgres.yaml" -f "$DEPLOY/gateway/gateway.yaml" -f "$DEPLOY/mcp-server.yaml" \
    -f "$DEPLOY/network-policies.yaml" -f "$DEPLOY/operator.yaml" -f "$DEPLOY/console.yaml"
  models
}

restart() {
  # Same :dev tag after a rebuild, so roll the pods to pick up the new images.
  k -n agent-system rollout restart deploy/agent-operator
  k -n agents rollout restart deploy/agent-console deploy/mcp-server
}

wait_ready() {
  k -n agents rollout status statefulset/postgres --timeout=180s
  k -n agents rollout status deploy/agentgateway --timeout=120s
  k -n agent-system rollout status deploy/agent-operator --timeout=120s
  k -n agents rollout status deploy/agent-console --timeout=120s
  k -n agents rollout status deploy/mcp-server --timeout=120s
  echo
  echo "console: http://localhost:32000"
  k -n agents get agentgatewaymodels
  k -n agents get agents
}

delete() {
  k delete --ignore-not-found agents --all --all-namespaces --wait=true
  k delete --ignore-not-found -f "$DEPLOY/console.yaml" -f "$DEPLOY/operator.yaml" -f "$DEPLOY/mcp-server.yaml" \
    -f "$DEPLOY/network-policies.yaml" -f "$DEPLOY/gateway/gateway.yaml" -f "$DEPLOY/postgres.yaml"
  k -n agents delete --ignore-not-found agentgatewaymodels --all
  k -n agents delete --ignore-not-found secret gateway-model-keys postgres agent-sessions
  k -n agents delete --ignore-not-found pvc data-postgres-0
  k delete --ignore-not-found -f "$DEPLOY/crd.yaml"
}

case "${1:-all}" in
  all) build; apply; restart; wait_ready ;;
  apply) apply; wait_ready ;;
  models) models ;;
  delete) delete ;;
  *) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
