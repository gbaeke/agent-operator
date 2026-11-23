# agent-operator

One container image, many agents. `agent-runtime` is a generic "deep agent" in Rust ([rig](https://github.com/0xPlaygrounds/rig) 0.46). Everything that makes an agent unique lives in a **folder**:

```
agents/my-agent/
  agent.yaml        model, instructions, tools, MCP servers, subagents, UI examples
  instructions.md   optional: the system prompt, if agent.yaml has no `instructions`
  skills/<name>/    optional: Agent Skills (SKILL.md + any reference files)
  knowledge/        optional: markdown documents the agent can search
```

Only `agent.yaml` is required, and the smallest one is a single line: `instructions: You are a helpful assistant.`

A separate **console** gives one web UI for all agents (see [Console](#console)).

This is phase 1 of the plan: the generic runtime and the console. Phase 2 adds a Kubernetes operator. There, an `Agent` custom resource becomes this same folder (a ConfigMap), mounted into this same image, and the console finds agents through the Kubernetes API.

## Quick start

```sh
cargo run -p mcp-server                          # terminal 1: shared tools (calculate, time, weather, fetch)
cargo run -p agent-runtime -- agents/quokka      # terminal 2
open http://127.0.0.1:3000
```

The samples send model calls to the local agentgateway on `http://localhost:4000/v1`, which holds the Azure key (`openai/azure-gpt-6-luna`). Start it with `agentgateway -f ~/.config/agentgateway/config.yaml`.

Check a folder without starting anything:

```sh
cargo run -q -p agent-runtime -- agents/quokka --check
```

## agent.yaml

```yaml
name: Trip Planner                     # UI title; defaults to the folder name
description: Plans short trips.        # UI subtitle
model: openai/azure-gpt-6-luna         # vendor/model; default anthropic/claude-sonnet-5-5
baseUrl: http://localhost:4000/v1      # optional: a gateway or proxy
maxTokens: 8192                        # per model call
temperature: 0.7                       # optional; reasoning models may reject it
maxTurns: 40                           # model calls per user message

instructions: |                        # or put it in instructions.md
  You are a cheerful, practical trip planner...

tools: [planning, files, knowledge, skills, subagents]   # built-in groups; absent = all

mcpServers:
  - url: ${MCP_URL:-http://127.0.0.1:8081/mcp}
    tools: [get_weather, current_time]  # optional allowlist

subagents:                             # optional named specialists for the `task` tool
  - name: researcher
    description: Researches one question and returns a sourced report.
    instructions: You are a research subagent...
    model: openai/azure-gpt-6.1-sol    # optional, defaults to the agent's model
    maxTurns: 15

examples:                              # clickable prompts in the UI
  - Make me a packing list for three days in Lisbon.
```

Unknown keys are an error, so a typo like `modle:` fails loudly.

**Environment variables:** `${VAR}` and `${VAR:-default}` are substituted before parsing. If `${VAR}` has no default and `VAR` is unset, loading fails. That's how one folder works on a laptop, in compose and in Kubernetes. The samples use `AGENT_MODEL`, `MODEL_BASE_URL` and `MCP_URL`.

**Models:** `vendor/model` goes to rig's catalog. The key comes from the vendor's usual variable, such as `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`, and so does `ANTHROPIC_BASE_URL`. With `baseUrl` set, the key may be unset: the gateway holds the real one, and the runtime sends a placeholder.

### Built-in tool groups

| Group | Tools | Notes |
|---|---|---|
| `planning` | `write_todos` | A final answer with open todos is sent back once or twice to finish the plan. |
| `files` | `ls`, `read_file`, `write_file`, `edit_file` | A scratch filesystem per chat session, shown in the UI's Files tab. |
| `knowledge` | `search_knowledge_base`, `read_document`, `list_documents` | BM25 over `knowledge/*.md`, split per section. Only active when the folder has documents. |
| `skills` | `load_skill` | Only active when `skills/` has skills. |
| `subagents` | `task` | Hands a self-contained job to a subagent with a fresh context. Without `subagents:` in the YAML there is one `general-purpose` subagent. Subagents get the knowledge, skills and MCP tools, but no plan, files or subagents of their own. |

### Skills

They use the [Agent Skills](https://agentskills.io) format:

```markdown
---
name: sla-credit
description: Work out the SLA service credit a customer is owed after an outage.
---

# SLA credit
1. Find the outage in the postmortems...
```

Only the name and description of each skill go into the system prompt. The agent calls `load_skill` when a request matches. The result lists the skill's other files, which it can read with `load_skill(name, file)`. Skills are read into memory at startup. Scripts can be read but not run, because the image has no shell. Hidden entries such as the `..data` folders of Kubernetes ConfigMap volumes are skipped.

## Containers

```sh
docker build --target agent-runtime -t agent-runtime .   # ~47 MB, distroless, no agent inside
docker build --target mcp-server -t mcp-server .
```

There are two ways to give the image an agent:

```sh
# 1. Mount a folder at /agent (from the repo root; a host path that does not
#    exist makes Docker mount an empty folder)
docker run -p 3000:3000 -v "$PWD/agents/quokka:/agent:ro" agent-runtime

# 2. Bake it in: a two-line Dockerfile
docker build -f agents/Dockerfile.example -t quokka agents/quokka
docker run -p 3000:3000 quokka
```

`compose.yaml` runs the three samples from the one image, with a shared MCP server and the console:

```sh
docker compose up -d --build
open http://localhost:8080   # the console: every agent in one UI
# each agent's own UI, for debugging: 3001 Quokka, 3002 Trip Planner, 3003 Captain Byte
```

The containers reach agentgateway on the host at `host.docker.internal:4000`. With ufw, Docker networks need a rule for that: `sudo ufw allow from 172.16.0.0/12 to any port 4000 proto tcp`.

## Runtime settings (environment)

These are deployment settings, not part of what makes an agent unique, so they stay out of `agent.yaml`.

| Variable | Default |
|---|---|
| `AGENT_DIR` | the CLI argument, else `.`; `/agent` in the image |
| `AGENT_ADDR` | `127.0.0.1:3000`; `0.0.0.0:3000` in the image |
| `DATABASE_URL` | unset: sessions in memory |
| `SESSION_TTL_DAYS` | `7`; `0` keeps sessions forever |
| `RUST_LOG` | `info` |

`.env` in the working directory (or a parent directory) is read at startup. Variables that are already set win.

## Agent API (v1)

Every agent serves this API. The console, the built-in UI and later the operator use nothing else.

| Route | |
|---|---|
| `GET /v1/info` | `{name, description, examples, model, documents, skills, subagents, mcp_tools, store, pod}` |
| `POST /v1/chat` | `{session_id, message}` → `text/event-stream`, one JSON event per `data:` line (below) |
| `POST /v1/reset` | `{session_id}`: forgets the session's history, plan and files |
| `GET /healthz` | liveness |
| `GET /readyz` | readiness, including the session database |
| `GET /` | the built-in UI for this one agent |

`session_id` is any string the client picks (the UI uses a UUID). The agent keeps that session's history. A second message to a session that is still busy gets `409 Conflict`.

Chat events, by `type`:

| `type` | Fields | |
|---|---|---|
| `text` | `delta` | the next piece of the answer |
| `tool_call` | `id, name, args` | the model calls a tool |
| `tool_result` | `id, name, content, is_error` | the tool answered (`content` clipped at 4,000 characters) |
| `subagent` | `task, kind, detail` | a subagent `started`, made a `tool_call`, is `done` or hit an `error` |
| `workspace` | `todos, files` | the session's plan and files, after every tool result |
| `discard_draft` | | drop the answer text so far; the model answers again |
| `done` | `input_tokens, output_tokens, requests` | the turn finished |
| `error` | `message` | the turn failed |

## Console

`agent-console` serves one UI for many agents. The browser only talks to the console, which proxies to the agents. So agents need no public address and no CORS, and the console is the one place to add sign-in later.

| Route | |
|---|---|
| `GET /` | the UI: the agent list on the left, one chat per agent |
| `GET /api/agents` | `[{id, healthy, info?, error?}]`: every agent's `/v1/info`, asked in parallel, 3s timeout |
| `GET /api/agents/{id}/info` | proxied to the agent's `/v1/info` |
| `POST /api/agents/{id}/chat` | proxied to `/v1/chat`; the event stream is passed through as it arrives |
| `POST /api/agents/{id}/reset` | proxied to `/v1/reset` |

| Variable | Default |
|---|---|
| `CONSOLE_AGENTS` | `id=url` pairs separated by commas, e.g. `quokka=http://quokka:3000,trip-planner=http://trip-planner:3000` |
| `CONSOLE_ADDR` | `127.0.0.1:8080`; `0.0.0.0:8080` in the image |

```sh
CONSOLE_AGENTS=quokka=http://127.0.0.1:3000 cargo run -p agent-console   # http://127.0.0.1:8080
```

The UI refreshes the agent list every 15 seconds and marks unreachable agents. Each agent keeps its own conversation while you switch agents, even while it is still answering. The URL (`#quokka`) remembers the selected agent. Conversations are kept in the page, so a reload starts new chats.

Agent and console serve the same `ui/index.html`. The console switches it to console mode, and all API paths in it are relative, so it also works behind a path prefix.

## Code map

- `runtime/src/config.rs`: the agent folder, `agent.yaml` and `${VAR}` substitution.
- `runtime/src/skills.rs`: loading skills and the system prompt section.
- `runtime/src/main.rs`: CLI, model connection, agent assembly, axum server, SSE.
- `runtime/src/tools/`: the built-in tools; `task.rs` holds the subagents.
- `runtime/src/kb.rs`, `store.rs`: BM25 knowledge base and session storage, from rust-agents.
- `console/src/main.rs`: the console: agent registry, health checks and the streaming proxy.
- `ui/index.html`: the one UI, in agent or console mode.
- `mcp-server/`: the sample MCP server, copied unchanged from rust-agents.
- `agents/`: the sample agents.

`cargo test --workspace` tests config loading (including every folder in `agents/`), skills, retrieval, the console's agent list and the calculator.

## Known issues

- Now and then the model sends a tool call whose arguments aren't valid JSON. This was seen once with `task` on gpt-6-luna. rig sends the error back to the model, and the model recovers.
