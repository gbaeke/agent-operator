#!/usr/bin/env bash
# Deploys the platform to the kind cluster: the Agent CRD, the operator, the
# console, the shared MCP server and the bridge to the model gateway on the
# host. Agents themselves are Agent resources: kubectl apply them.
#
#   deploy/deploy.sh          build the images, load them into kind, apply, wait
#   deploy/deploy.sh apply    apply the manifests only (no build)
#   deploy/deploy.sh delete   remove everything, including all Agents
#
# KIND_CLUSTER picks the cluster (default: agents). Run kind/cluster.sh up first.
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

# The host's address on the kind Docker network, where agentgateway listens on port 4000.
host_ip() {
  docker network inspect kind --format '{{range .IPAM.Config}}{{.Gateway}} {{end}}' \
    | tr ' ' '\n' | grep -E '^[0-9]+(\.[0-9]+){3}$' | head -1
}

apply() {
  k apply -f "$DEPLOY/namespaces.yaml" -f "$DEPLOY/crd.yaml"
  k wait --for=condition=Established crd/agents.agents.baeke.info --timeout=60s >/dev/null
  sed "s/\${HOST_IP}/$(host_ip)/" "$DEPLOY/llm-gateway.yaml" | k apply -f -
  k apply -f "$DEPLOY/mcp-server.yaml" -f "$DEPLOY/operator.yaml" -f "$DEPLOY/console.yaml"
}

restart() {
  # Same :dev tag after a rebuild, so roll the pods to pick up the new images.
  k -n agent-system rollout restart deploy/agent-operator
  k -n agents rollout restart deploy/agent-console deploy/mcp-server
}

wait_ready() {
  k -n agent-system rollout status deploy/agent-operator --timeout=120s
  k -n agents rollout status deploy/agent-console --timeout=120s
  k -n agents rollout status deploy/mcp-server --timeout=120s
  echo
  echo "console: http://localhost:32000"
  k -n agents get agents
}

delete() {
  k delete --ignore-not-found agents --all --all-namespaces --wait=true
  k delete --ignore-not-found -f "$DEPLOY/console.yaml" -f "$DEPLOY/operator.yaml" -f "$DEPLOY/mcp-server.yaml" -f "$DEPLOY/llm-gateway.yaml"
  k delete --ignore-not-found -f "$DEPLOY/crd.yaml"
}

case "${1:-all}" in
  all) build; apply; restart; wait_ready ;;
  apply) apply; wait_ready ;;
  delete) delete ;;
  *) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
