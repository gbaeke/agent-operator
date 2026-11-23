#!/usr/bin/env bash
# Regenerates deploy/samples/<agent>.yaml from each folder in agents/ with
# `agent-operator render`. A test fails while a sample is stale.
#
#   deploy/samples.sh                              regenerate all
#   kubectl apply -f deploy/samples/quokka.yaml    deploy one
set -euo pipefail
ROOT="$(dirname "$(realpath "$0")")/.."
mkdir -p "$ROOT/deploy/samples"
cargo build -q -p agent-operator --manifest-path "$ROOT/Cargo.toml"
for dir in "$ROOT"/agents/*/; do
  name="$(basename "$dir")"
  "$ROOT/target/debug/agent-operator" render "$dir" > "$ROOT/deploy/samples/$name.yaml"
  echo "deploy/samples/$name.yaml"
done
