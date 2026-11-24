# Agent specs and kagent

Research from 23 November 2026: is there a standard way to define an agent, and how does this project compare
with kagent?

There is no single accepted standard. A few specs each cover one part (a portable definition, how a running agent
describes itself, a catalogue record, skills), and several frameworks have their own format. Our `agent.yaml` is
one of the framework formats.

## Specs

| Spec | What it describes | Fit with this project |
|---|---|---|
| [Open Agent Specification](https://oracle.github.io/agent-spec/docs_home.html) (Oracle) | A portable definition of agents (system prompt, LLM config, tools) and flows, in JSON or YAML | The closest thing to a portable `agent.yaml`. Mostly an Oracle project so far |
| [A2A Agent Card](https://a2a-protocol.org) | How a running agent describes itself: name, description, endpoints, skills with examples, security | Describes the interface, not the build. Our `/v1/info` holds most of it |
| [OASF](https://docs.agntcy.org/oasf/open-agentic-schema-framework/) (AGNTCY, Linux Foundation) | Agent records for directories: skills, domains, metadata, stored as OCI artifacts | Catalogue metadata, not a runtime definition |
| [Agent Skills](https://agentskills.io) | `SKILL.md` folders: frontmatter with a name and description, plus files | We use it for skills |
| [AGENTS.md](https://openai.com/index/agentic-ai-foundation/) (Agentic AI Foundation) | Free-form markdown instructions for coding agents in a repository, with no schema | A different problem |

### Open Agent Specification

- It calls itself a portable, platform-agnostic configuration language for agents and flows, and compares itself to
  ONNX for ML models ([arXiv 2510.04173](https://arxiv.org/abs/2510.04173)).
- SDKs: `pyagentspec` (Python) and a TypeScript SDK, which serialize agents to and from JSON or YAML.
- An `Agent` has `name`, `description`, `system_prompt`, `llm_config` (for example `OpenAiCompatibleConfig`,
  `OllamaConfig`) and `tools` ([reference sheet](https://oracle.github.io/agent-spec/26.1.2/misc/reference_sheet.html)).
  Components are versioned (for example 25.4.1 and 26.1.x).
- Runtimes map the components onto their own: WayFlow is the reference runtime, and there are adapters for
  LangGraph, AutoGen and CrewAI ([repository](https://github.com/oracle/agent-spec)). It also has a conformance
  test suite.
- We found no sign of wide adoption outside Oracle.

### A2A Agent Card

- A JSON document a running agent publishes, so clients can discover it and know how to call it.
- The main fields are `name`, `description`, `version`, the endpoints (`supportedInterfaces` in v1.0; a top-level
  URL and protocol version in 0.3), `skills`, security schemes and signatures.
- A skill has an `id`, a `name` and a `description` (all required), plus `tags` and `examples`
  ([Google's trimmed schema](https://docs.cloud.google.com/gemini/data-agents/reference/rest/Shared.Types/AgentCard),
  [Atlassian's schema](https://developer.atlassian.com/cloud/rovo-a2a/agent-card-schema/)).
- Our runtime has the data for one: `name`, `description` and `examples` from agent.yaml, and the skills and tools
  from `/v1/info`.

### OASF

- The schema layer of AGNTCY. Its central object is the agent record, with skill and domain annotations for
  discovery, and modules to extend it ([docs](https://docs.agntcy.org/oasf/open-agentic-schema-framework/)).
- The AGNTCY Agent Directory Service stores records as OCI artifacts
  ([IETF draft](https://datatracker.ietf.org/doc/html/draft-mp-agntcy-ads-02)).

## Framework formats

These do the same job as our `agent.yaml`: one file that defines an agent for one runtime.

| Format | Shape |
|---|---|
| [Microsoft Agent Framework declarative agents](https://learn.microsoft.com/agent-framework/agents/declarative) | YAML with `kind: Prompt`, `name`, `description`, `instructions`, a `model` block (values like `=Env.AZURE_OPENAI_MODEL`), and an optional output schema. Loaded with `agent-framework-declarative` (Python) or `Microsoft.Agents.AI.Declarative` (.NET) |
| [Docker Agent](https://docs.docker.com/ai/docker-agent/reference/config/) (formerly cagent) | YAML or HCL with a `version` (v7), agents under `agents:` (a `root` agent with a model such as `openai/gpt-4o`), toolsets, RAG sources, permissions and hooks. It publishes a JSON schema for editors |
| [kagent](https://www.kagent.dev/docs/kagent/resources/api-ref) | A Kubernetes `Agent` CRD (below) |
| agent-operator (this repo) | `agent.yaml`, or the same fields as the spec of an `Agent` resource (`agents.baeke.info/v1alpha1`) |

## kagent compared with agent-operator

kagent (`kagent.dev/v1alpha2`, a CNCF project from the agentgateway team) runs agents on Kubernetes from CRDs. It
covers most of what this project does and more. Taken from its
[API reference](https://www.kagent.dev/docs/kagent/resources/api-ref):

- **Agent:** `spec.type` is `Declarative` (default) or `BYO`. A declarative agent has `systemMessage` (or
  `systemMessageFrom` a ConfigMap or Secret, optionally a Go template with `promptTemplate`), `modelConfig` (the
  name of a ModelConfig, default `default-model-config`), `tools`, `a2aConfig.skills`, `memory`, `context`
  (compaction), `runtime` (`go` by default, or `python`), and a `deployment` block with the usual pod settings.
  A BYO agent is your own image, which must serve A2A on port 8080.
- **Models:** a `ModelConfig` resource per model: `model`, `provider` (OpenAI, Anthropic, AzureOpenAI, Ollama,
  Gemini, Bedrock, Foundry and more), and the key from a Secret or passed through.
- **MCP:** `RemoteMCPServer` resources (`url`, `protocol`, headers from Secrets, TLS), whose status lists the
  discovered tools. An agent's tool entry names the server and lists `toolNames`, and `requireApproval` for tools
  a person must approve. A tool can also be another Agent.
- **Skills:** `refs` (OCI images), `gitRefs` (repository, ref, path) or `s3Refs`, mounted under `/skills`.
- **A2A:** every agent gets an Agent Card from `description`, `version`, `provider` and the A2A skills.
- **Other:** sandbox network rules (`allowedDomains`), `allowedNamespaces`, share-link tools, and a UI.
- **Status:** `observedGeneration` and `conditions`.

| | agent-operator | kagent |
|---|---|---|
| Agent resource | `Agent`, spec = agent.yaml | `Agent`, declarative or BYO |
| Models | shared AgentgatewayModels, checked by the operator | a `ModelConfig` per model |
| MCP | URLs through the gateway | `RemoteMCPServer` resources, tool allowlist, approval |
| Skills | ConfigMaps | OCI, git or S3 |
| Agent to agent | `task` subagents inside one pod | other Agents as tools, over A2A |
| Built-in tools | knowledge search (BM25), plan, files, subagents, skills | from MCP servers and other agents |
| Extras | Agent Studio, `render`, one format for folder, Docker and cluster | memory, context compaction, sandbox, its own UI |
| How an agent runs | a Deployment per agent: always-on pods | with Agent Substrate: an actor per conversation in a gVisor sandbox, on shared worker pods (below) |
| Status | `ready`, `message`, `url`, `model` | `conditions` |
| Maturity | one repository, 28 tests | CNCF project with a community |

### How agents run: a pod per agent, or an actor per conversation

This is the biggest difference. Here every agent is a container: the operator gives each Agent its own Deployment,
and its pods run whether anyone is chatting or not. kagent 1.0 on [Agent Substrate](https://github.com/kagent-dev/substrate)
runs agents as sandboxed actors instead (from [kagent-substrate](https://github.com/gbaeke/kagent-substrate), a
local test of kagent 1.0.0 and Substrate 1.0.0):

- **The unit is a conversation, not an agent.** Each conversation becomes its own Substrate actor.
- **Actors share worker pods.** A WorkerPool of pre-warmed gVisor worker pods runs all the actors; 10 parallel
  conversations ran as 10 actors on 2 pods. Scaling means more worker pods (`kubectl scale workerpool …`).
- **Idle costs nothing.** An actor is snapshotted and suspended after every reply, and resumed (about 100 ms) on the
  next message. Snapshots go to an S3 bucket.
- **Isolation is a sandbox per actor.** gVisor sits between each actor and the host kernel, and every actor gets
  its own egress policy. An egress proxy injects secrets such as API keys, so they never enter the actor.
- **The runtime is a Harness.** Every Agent references a shared Harness (for example the Go ADK runtime, or Claude
  Code) through `harnessRef`, and the runtime image comes from the release's catalog.

| | agent-operator | kagent on Agent Substrate |
|---|---|---|
| Unit | one agent = one Deployment | one conversation = one actor |
| Placement | its own pods | actors on a shared, pre-warmed worker pool |
| Isolation | a container: restricted pod security, NetworkPolicies | a gVisor sandbox per actor, an egress policy per actor |
| State between messages | the pod keeps running; sessions in Postgres | the actor is snapshotted and suspended; resumed on the next message |
| Idle cost | every agent's pods, all the time | none for suspended actors, only the worker pool |
| Scaling | `replicas` per agent | worker pods for all agents |
| Secrets | the gateway holds the model keys | the egress proxy injects them |

The pod-per-agent model is simpler to understand and debug (it's a Deployment like any other), which suits a
learning project. The actor model is built for many agents and many conversations, most of them idle, and for
running code an agent writes, which needs a real sandbox.

### What that means here

- **This project was built to learn how a Kubernetes operator for agents works.** kagent is similar in nature and
  is the better choice for production: it does more, it is maintained by others, it pairs with agentgateway
  already, and with Agent Substrate it isolates and suspends every conversation.
- **What this project still has:**
  - One format everywhere: the same `agent.yaml` runs from a folder, in Docker and in the cluster, and `render`
    moves it between them. In kagent a declarative agent only exists as a Kubernetes resource.
  - Built-in research tools: knowledge search, a plan, a scratch filesystem and subagents ship with the runtime.
  - It is small enough to read in an afternoon, which helps for learning and writing about it.
- **A middle path:** run our runtime as a kagent BYO agent. kagent then handles models, MCP and its UI, and we
  keep the runtime. That needs A2A on port 8080 in the runtime ([#3](https://github.com/gbaeke/agent-operator/issues/3)).
