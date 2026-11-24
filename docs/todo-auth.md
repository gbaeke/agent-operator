# Sign-in and user identity: status and next steps

Users sign in to the console, see only the agents they may use, and their identity reaches every MCP tool call, so a tool can act as that user. Everything is switched on with `deploy/auth.sh` and off again with `deploy/auth.sh delete`; the rest of the platform does not depend on it.

```
browser ──login──► Keycloak (users, groups)
   │ cookie
oauth2-proxy ─► console ── Bearer <user token> ──► agent ──► agentgateway ──► mcp-server
                checks JWT,                    token on      checks JWT,      checks JWT,
                filters agents                 MCP calls     (tool rules)     tools read claims
```

## Done

| Step | What | Where |
|---|---|---|
| 1 | Keycloak in namespace `auth` with its own Postgres; realm `agents` with alice (sales), bob (engineering), admin (platform-admins); one issuer URL for browser and pods (`AUTH_HOST`, default the tailnet name, plus a CoreDNS rewrite) | `deploy/auth.sh`, `deploy/auth/keycloak.yaml`, `deploy/auth/realm.json` |
| 2 | oauth2-proxy sidecar on the console (login, refresh, sign-out with Keycloak logout); the console checks the token on every `/api` call; the UI shows the user and a sign-out link | `deploy/auth/console-patch.yaml`, `console/src/auth.rs` |
| 3 | `spec.access` (`groups`, `users`) on the Agent; the console lists only allowed agents and answers 404 for others; `platform-admins` (`CONSOLE_ADMIN_GROUP`) sees all; fields in Agent Studio | `operator/src/crd.rs`, `console/src/registry.rs` |
| 4 | The console passes the user's token to the agent; the runtime connects its MCP servers per turn with it; sessions have an owner and refuse other users (403) | `runtime/src/main.rs`, `runtime/src/store.rs` |
| 5 | agentgateway requires a valid token on `/mcp` (kept for the server) and `/learn/mcp` (removed before it leaves the cluster); the MCP server checks it again; tools `whoami` and `my_tickets`; `helpdesk` sample | `deploy/auth/gateway-policy.yaml`, `mcp-server/src/auth.rs`, `agents/helpdesk/` |

## Next

Tracked as GitHub issues:

- [#11](https://github.com/gbaeke/agent-operator/issues/11) Tool rules per group on the gateway (small, config only)
- [#12](https://github.com/gbaeke/agent-operator/issues/12) Agents check the user's token themselves
- [#13](https://github.com/gbaeke/agent-operator/issues/13) Token exchange instead of passing the user's token on (also fixes long turns)
- [#14](https://github.com/gbaeke/agent-operator/issues/14) Test a real rollback with `deploy/auth.sh delete`
- [#15](https://github.com/gbaeke/agent-operator/issues/15) Let `deploy.sh` install sign-in too
- [#16](https://github.com/gbaeke/agent-operator/issues/16) HTTPS for the console and Keycloak
- [#17](https://github.com/gbaeke/agent-operator/issues/17) An agent lists no MCP tools until a signed-in user has chatted with it

Recommended order: #11 (quick, and a good demo), then #14; #12 and #13 after that, or not at all for a demo.

## Known limits

- **Long turns can outlive the token:** the token is sent when a turn starts and access tokens last 30 minutes ([#13](https://github.com/gbaeke/agent-operator/issues/13)).
- **No MCP tools before the first chat:** behind sign-in an agent cannot list its MCP tools at startup, so `/v1/info` shows none until a signed-in user has chatted with it ([#17](https://github.com/gbaeke/agent-operator/issues/17)).
- **The box's name means Keycloak inside the cluster:** the CoreDNS rewrite sends `AUTH_HOST` to Keycloak, so nothing in the cluster can use that name for anything else.
- **The `access` lists are only on the live cluster:** samples are generated from the agent folders and `access` is a Kubernetes-only field; set it with `kubectl patch` or Agent Studio.
- **Keep tokens out of logs, session storage and the Agent's status:** true today; keep it that way.
