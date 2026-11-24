# To do

What is still open after the runtime, the console, the operator, the in-cluster gateway and Postgres sessions.

## Next

### Shared `Skill` resource

Skills are now ConfigMaps that one Agent references by name. `render` names them `<agent>-skill-<skill>`, so two agents each get their own copy of the same skill.

- Add a `Skill` CRD (`agents.baeke.info/v1alpha1`) with `name`, `description`, `instructions` and optional `files`, so a skill is declared once and shared.
- Let Agents reference it: `skills: [{ skill: sla-credit }]` next to the current `configMap` form.
- The operator mounts it at `skills/<name>/` as it does now, and a change to a Skill rolls every Agent that uses it. The config hash already covers mounted content.
- Optional: allow subfolders inside a skill, which ConfigMaps cannot hold.

### Sign-in and user identity

Sign-in on the console, agents per user or group, and the user's identity passed to MCP tools: steps 1 to 5 are done (`deploy/auth.sh`). What is next (tool rules per group, agents checking tokens, token exchange) and the loose ends are in [todo-auth.md](todo-auth.md).

### Agents in other namespaces

The operator watches Agents in every namespace, but the NetworkPolicies (`deploy/network-policies.yaml`) only exist in `agents`; the operator creates none. An Agent elsewhere, say `team-a`, runs without any: it can reach the internet and the whole cluster, and any pod can call it. And it does not work: `gateway-ingress` and `postgres-ingress` only admit agents from `agents`, so it gets no models, MCP tools or sessions.

- Let the operator apply an egress and an ingress NetworkPolicy per Agent (owned by it, like the Deployment), the same rules as `agent-egress` and `agent-ingress` but pointing at the gateway, Postgres and console in `agents`.
- Admit those namespaces in `gateway-ingress` and `postgres-ingress`, for example with a namespace label such as `agents.baeke.info/enabled: "true"`.
- Until then: run Agents in `agents` only.

### A2A support in the runtime

The runtime speaks its own API (`/v1/info`, `/v1/chat` as an event stream). A2A, the agent-to-agent protocol, would let other agents and tools call ours, and let us run the runtime as a kagent BYO agent, which must serve A2A on port 8080. See [agent-specs.md](agent-specs.md).

- Publish an A2A Agent Card from what `/v1/info` already knows: name, description, skills, examples.
- Serve A2A next to `/v1`, mapping our events (text, tool calls, plan, files) onto A2A messages, tasks and artifacts.
- Optional: let an agent call another Agent over A2A as a tool, next to the in-pod `task` subagents.

### Argo CD

Everything is applied by `deploy/deploy.sh` today. `rust-agents/docs/gitops-plan.md` has a worked plan for the same move.

- Argo CD manages the platform: the CRD, the operator, the console, the gateway, Postgres and the policies.
- Agents become files in git (for example `agents/*.yaml` rendered with `agent-operator render`), synced by an ApplicationSet.
- Secrets (model keys, the database password) need a git-safe form, such as Sealed Secrets or External Secrets.

### Gateway request logs and analytics

agentgateway can write every model call (model, tokens, cost, duration) to Postgres, which feeds its admin UI pages `/ui/llm/analytics` and `/ui/llm/logs`. rust-agents does this. Prompts and completions are not stored.

- Create a database `agentgateway` on the existing Postgres in `deploy.sh`.
- Set `rawConfig.config.database.url` in `AgentgatewayParameters` (`deploy/gateway/gateway.yaml`), with the password from the Secret `postgres`.
- Allow the gateway to reach Postgres in `deploy/network-policies.yaml` (now only agents may).
- Document how to open the admin UI: `kubectl -n agents port-forward deploy/agentgateway 15000:15000`.

## Smaller

- **Reopen chats in the UI:** a reload starts new chats, although sessions are kept in Postgres. This needs a `GET /v1/sessions` (and, with sign-in, per user) plus a session list in the UI.
- **A deleted agent's open chat:** when the selected agent is deleted, the console keeps showing its chat, and sending fails. Show that the agent is gone.
- **Malformed tool arguments:** gpt-6-luna once sent `task` arguments that were not valid JSON. rig returns the error to the model, which recovers. Watch whether it recurs.
- **Operator high availability:** one operator replica with `Recreate`. For more, add leader election.
- **Images:** everything is `:dev` and loaded with `kind load`. For a real cluster, publish versioned images to a registry, for example from CI.
