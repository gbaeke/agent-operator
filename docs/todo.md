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

The console is open to anyone who can reach port 32000, and agents and MCP servers act as nobody in particular. Users should sign in, see only the agents they may use, and their identity should reach every MCP tool call, so a tool can act as that user.

```
browser ──login──► Keycloak (users, groups)
   │ cookie
oauth2-proxy ─► console ── Bearer <user token> ──► agent ──► agentgateway ──► mcp-server
                checks JWT,                    token on      checks JWT,      checks JWT,
                filters agents                 MCP calls     tool rules       tools read claims
```

Decided:

- **Keycloak** in its own namespace `auth`, realm `agents` imported from git, with its own small Postgres so it survives restarts and removes cleanly. Users `alice` (sales), `bob` (engineering), `admin` (platform-admins).
- **One issuer URL everywhere:** Keycloak's hostname is the name the browser uses for this machine, port 32001 (`AUTH_HOST`: the tailnet name, else `keycloak.localtest.me`, which is 127.0.0.1), and a CoreDNS rewrite sends the same name to the Keycloak Service inside the cluster, which also listens on 32001.
- **oauth2-proxy as a sidecar** of the console handles login, refresh and logout, with the session in encrypted cookies so console replicas share nothing. The console still validates the JWT (issuer, audience `agent-platform`, expiry). Without OIDC settings the console runs open, as now.
- **`spec.access` on the Agent:** `groups` and `users`; absent means every signed-in user. `platform-admins` sees everything. The console filters `/api/agents` and refuses `forward` for others. Agent Studio gets the field.
- **Token on every MCP call:** the runtime opens an MCP connection per chat turn with the user's token (today one connection is made at startup and shared). Sessions store their owner (`sub`) and refuse other users.
- **Gateway:** an `AgentgatewayPolicy` on `/mcp` with `jwtAuthentication` (Strict, `preserveToken`), and `backend.mcp.authorization` rules on `jwt.groups` per tool.
- **MCP server:** validates the JWT again and exposes the claims to tools; new tools `whoami` and `my_tickets` (fake per-user data).

Steps, each one demoable:

1. Keycloak on kind: namespace, Postgres, realm, CoreDNS rewrite (`deploy/auth.sh`). **Done.**
2. oauth2-proxy sidecar and JWT check in the console; the UI shows who is signed in and has sign-out. **Done:** `deploy/auth.sh` patches the console (`deploy/auth/console-patch.yaml`) and `delete` takes it off again.
3. `spec.access` in the CRD, operator and Studio; console filtering. **Done.**
4. Runtime: token on per-turn MCP connections, session owners. **Done:** the console sends `Authorization: Bearer`; with it the runtime connects its MCP servers per turn as the user, and sessions (`agent_workspaces.owner`) refuse other users with 403. The runtime reads `sub` without checking the token (only the console can reach it); step 7 checks it.
5. Gateway JWT policy on `/mcp`, MCP server JWT check, `whoami` and `my_tickets`. **Done:** `deploy/auth/gateway-policy.yaml` requires a user token on `/mcp` (kept for the server) and on `/learn/mcp` (removed before it leaves the cluster); the MCP server checks it again; the `helpdesk` sample shows tools acting as the user.
6. MCP tool authorization per group in the gateway.
7. Later: token exchange at the gateway (`backend.auth.oauthTokenExchange`, audience `mcp-server`) instead of passthrough, and agents validating the token themselves.

Watch out for: a token is sent when a turn starts, so a long turn can outlive it (access tokens last 30 minutes for now; token exchange or refresh fixes it properly). Keep tokens out of logs, session storage and the Agent's status.

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
