#!/usr/bin/env python3
"""Prints the Azure models of a standalone agentgateway config as Kubernetes
objects for the in-cluster gateway: one Secret `gateway-model-keys` with a
key per model, and one AgentgatewayModel per chat model.

    deploy/gateway/models-from-host.py [config.yaml] | kubectl apply -f -

The config defaults to $AGENTGATEWAY_CONFIG, then ~/.config/agentgateway/config.yaml.
The output holds API keys: pipe it into kubectl, never into a file or git.

Each model keeps the name clients use (e.g. `azure-gpt-6-luna`) through
`match.model`; the resource is named after the Azure deployment it calls,
and a transformation puts that deployment name in the request.
"""

import json
import os
import sys

import yaml

NAMESPACE = "agents"
SECRET = "gateway-model-keys"


def main() -> None:
    default = os.environ.get("AGENTGATEWAY_CONFIG", os.path.expanduser("~/.config/agentgateway/config.yaml"))
    path = sys.argv[1] if len(sys.argv) > 1 else default
    with open(path) as file:
        config = yaml.safe_load(file)

    keys, models = {}, []
    for model in config.get("llm", {}).get("models", []):
        params = model.get("params", {})
        if model.get("provider") != "azure" or "embedding" in model["name"]:
            continue
        if not params.get("apiKey"):
            print(f"skipping {model['name']}: no apiKey", file=sys.stderr)
            continue
        keys[model["name"]] = params["apiKey"]
        models.append({
            "apiVersion": "agentgateway.dev/v1alpha1",
            "kind": "AgentgatewayModel",
            "metadata": {
                "name": params["model"],
                "namespace": NAMESPACE,
                "labels": {"app.kubernetes.io/part-of": "agent-platform"},
            },
            "spec": {
                "parentRefs": [{
                    "group": "gateway.networking.k8s.io", "kind": "Gateway",
                    "name": "agentgateway", "sectionName": "http",
                }],
                "match": {"model": model["name"]},
                "provider": "Azure",
                "azure": {
                    "resourceType": params.get("azureResourceType", "foundry").capitalize(),
                    "resourceName": params["azureResourceName"],
                    **({"projectName": params["azureProjectName"]} if params.get("azureProjectName") else {}),
                },
                "policies": {
                    # Clients ask for the name they know; Azure wants its deployment name.
                    "transformations": [{"field": "model", "expression": json.dumps(params["model"])}],
                    "auth": {
                        "secretRef": {"name": SECRET, "key": model["name"]},
                        "location": {"header": {"name": "api-key"}},
                    },
                },
            },
        })

    if not models:
        sys.exit(f"no Azure chat models with an apiKey in {path}")
    secret = {
        "apiVersion": "v1", "kind": "Secret", "type": "Opaque",
        "metadata": {"name": SECRET, "namespace": NAMESPACE},
        "stringData": keys,
    }
    # JSON documents are valid YAML; kubectl reads a stream of them.
    for document in [secret, *models]:
        print("---")
        print(json.dumps(document))
    names = ", ".join(m["spec"]["match"]["model"] for m in models)
    print(f"models: {names}", file=sys.stderr)


if __name__ == "__main__":
    main()
