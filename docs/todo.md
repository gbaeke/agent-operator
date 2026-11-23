# To do

What is still open after the runtime, the console, the operator, the in-cluster gateway and Postgres sessions.

## Next

### Shared `Skill` resource

Skills are now ConfigMaps that one Agent references by name. `render` names them `<agent>-skill-<skill>`, so two agents each get their own copy of the same skill.

- Add a `Skill` CRD (`agents.baeke.info/v1alpha1`) with `name`, `description`, `instructions` and optional `files`, so a skill is declared once and shared.
- Let Agents reference it: `skills: [{ skill: sla-credit }]` next to the current `configMap` form.
- The operator mounts it at `skills/<name>/` as it does now, and a change to a Skill rolls every Agent that uses it. The config hash already covers mounted content.
- Optional: allow subfolders inside a skill, which ConfigMaps cannot hold.

### Sign-in, and the user's token passed on to agents and MCP servers

The console is open to anyone who can reach port 32000, and agents and MCP servers act as nobody in particular. Users should sign in, and their identity should reach every tool call, so an MCP server can act as that user (their mail, their files, their tickets) and enforce their permissions.

- **Sign in on the console:** OIDC, for example Entra ID, or WorkOS as in app-blueprint. The console is the only way into the agents, so it is the place for it.
- **Pass the identity on:** the console sends the user's token, or a token derived from it, with every call to an agent (`/v1/chat`), instead of only a session id.
- **From the agent to its tools:** the runtime passes that token to the gateway on every model and MCP call. The gateway checks it, and for MCP servers that need the user's own access it forwards the token or exchanges it for one meant for that server (on-behalf-of, token exchange). agentgateway has policies for this (JWT checks, `oauthTokenExchange`, passthrough).
- **Agents themselves:** an agent should accept only calls from the console, with a valid user token, and not just rely on the network policy.
- **Sessions per user:** with an identity, sessions belong to a user and can be listed and reopened (see "Reopen chats in the UI" below).
- **Who sees which agent:** optionally per user or group, for example from an annotation on the Agent.
- Keep tokens out of logs, session storage and the Agent's status.

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
