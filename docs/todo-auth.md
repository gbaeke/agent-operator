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

### 6. Tool rules per group (small, config only)

The gateway decides which tools a group may use: for example `fetch_url` only for `engineering`. Other users don't see the tool in `tools/list`, and a direct `tools/call` is refused.

- Add `backend.mcp.authorization` to an `AgentgatewayPolicy` for the `mcp-server` backend: CEL rules on `jwt.groups` and the tool name (`Allow`/`Require`; avoid `Deny`, which fails open on expression errors).
- Put it in `deploy/auth/gateway-policy.yaml`, so `deploy/auth.sh delete` removes it too.
- Demo: alice asks quokka to fetch a web page; the tool is not there for her, but it is for bob.

### 7a. Agents check the token themselves (small to medium)

Today the runtime reads the user (`sub`) from the token without checking it, and relies on the network policy that only lets the console in. Any pod that gets past that policy could pretend to be any user.

- Check the signature, issuer, audience and expiry in the runtime, as the console and the MCP server do (`OIDC_ISSUER`, switched on by `deploy/auth.sh` through a patch or the operator).
- Optionally the operator passes `spec.access` to the agent, so the agent enforces it too, not only the console.
- Consider one shared crate for the token check: the console and the MCP server now each have a copy.

### 7b. Token exchange instead of passing the token on (medium)

One token (`aud: agent-platform`) now travels all the way to the MCP server. The MCP spec says a server should only accept tokens issued for it.

- The gateway exchanges the user's token for one with `aud: mcp-server` (`backend.auth.oauthTokenExchange`), using Keycloak's standard token exchange.
- Keycloak: a client `mcp-server` (the audience) and token-exchange permission for the client the gateway uses.
- The MCP server then expects audience `mcp-server`.
- This also helps with long turns (see below): the gateway can get a fresh token per call.

## Loose ends

- **Merge:** the work is on branch `auth-keycloak`, not pushed. Merge to `main` when it is accepted.
- **A real rollback:** `deploy/auth.sh delete` has been run for Keycloak (step 1), but its undo of the console, MCP server and gateway changes (steps 2 to 5) has only been checked as a server-side dry run. Run `delete` and `apply` for real once.
- **`deploy.sh` doesn't know about sign-in:** after recreating the cluster, run `deploy/auth.sh` after `deploy/deploy.sh`. Add an `AUTH=1` option, or a line in the quick start.
- **HTTPS:** everything is plain HTTP on the tailnet, so cookies are not `Secure` and Keycloak warns. `tailscale serve` can give real `*.ts.net` certificates.

## Known limits

- **Long turns can outlive the token:** the token is sent when a turn starts and access tokens last 30 minutes. Token exchange (7b) or refreshing in the runtime fixes it properly.
- **No MCP tools before the first chat:** behind sign-in an agent cannot list its MCP tools at startup, so `/v1/info` shows none until a signed-in user has chatted with it.
- **The box's name means Keycloak inside the cluster:** the CoreDNS rewrite sends `AUTH_HOST` to Keycloak, so nothing in the cluster can use that name for anything else.
- **The `access` lists are only on the live cluster:** samples are generated from the agent folders and `access` is a Kubernetes-only field; set it with `kubectl patch` or Agent Studio.
- **Keep tokens out of logs, session storage and the Agent's status:** true today; keep it that way.

## Recommended order

Step 6 next (quick, and a good demo), then a real rollback test and the merge to `main`; 7a and 7b after that, or not at all for a demo.
