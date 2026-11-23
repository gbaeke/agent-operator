//! An agent is a folder. Only `agent.yaml` is required:
//!
//! ```text
//! my-agent/
//!   agent.yaml        model, instructions, tools, MCP servers, subagents, UI examples
//!   instructions.md   optional: the instructions, if agent.yaml has none
//!   skills/<name>/    optional: one folder per skill, each with a SKILL.md
//!   knowledge/        optional: markdown documents the agent can search
//! ```
//!
//! `${VAR}` and `${VAR:-default}` in agent.yaml are replaced by environment
//! variables before it is parsed, so one folder works on a laptop, in
//! compose and in Kubernetes.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::kb::KnowledgeBase;
use crate::skills::Skill;

pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5-5";

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentConfig {
    /// Shown in the UI. Defaults to the folder name.
    pub name: Option<String>,
    pub description: Option<String>,
    /// `vendor/model`, e.g. `anthropic/claude-sonnet-5-5`, `openai/gpt-5.5`,
    /// `ollama/qwen3:4b`. The key comes from the vendor's usual variable.
    #[serde(default = "default_model")]
    pub model: String,
    /// Send model calls here instead, e.g. a gateway. Empty means unset.
    /// Behind a gateway the vendor's API key may be unset.
    pub base_url: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
    pub temperature: Option<f64>,
    /// Model calls per user message.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// The system prompt. If absent, `instructions.md` is used.
    pub instructions: Option<String>,
    /// The built-in tool groups to enable. Absent means all of them.
    /// `knowledge` and `skills` only appear when their folder has content.
    pub tools: Option<Vec<Builtin>>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    /// Specialists the `task` tool can hand work to. Without any, `task`
    /// starts a general-purpose subagent.
    #[serde(default)]
    pub subagents: Vec<SubagentConfig>,
    /// Example prompts for the UI.
    #[serde(default)]
    pub examples: Vec<String>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Builtin {
    /// `write_todos`, plus a check that the plan is finished before answering.
    Planning,
    /// A scratch filesystem in the session: `ls`, `read_file`, `write_file`, `edit_file`.
    Files,
    /// Search over `knowledge/`: `search_knowledge_base`, `read_document`, `list_documents`.
    Knowledge,
    /// `load_skill` for the skills in `skills/`.
    Skills,
    /// `task`: hand a self-contained job to a subagent with its own context.
    Subagents,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServer {
    pub url: String,
    /// Only these tools of the server; absent means all.
    pub tools: Option<Vec<String>>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentConfig {
    pub name: String,
    /// Tells the main agent when to use this subagent.
    pub description: String,
    pub instructions: String,
    /// Defaults to the agent's model.
    pub model: Option<String>,
    #[serde(default = "default_subagent_turns")]
    pub max_turns: usize,
}

fn default_model() -> String {
    DEFAULT_MODEL.to_owned()
}
fn default_max_tokens() -> u64 {
    8192
}
fn default_max_turns() -> usize {
    40
}
fn default_subagent_turns() -> usize {
    15
}

/// A loaded agent folder.
pub struct Bundle {
    pub dir: PathBuf,
    pub name: String,
    pub config: AgentConfig,
    pub instructions: String,
    pub skills: Vec<Skill>,
    pub knowledge: Option<KnowledgeBase>,
}

impl Bundle {
    /// Loads an agent folder, or an agent.yaml whose folder holds the rest.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let (dir, file) = if path.is_dir() {
            let file = ["agent.yaml", "agent.yml"]
                .iter()
                .map(|name| path.join(name))
                .find(|file| file.is_file())
                .with_context(|| match std::fs::read_dir(path).map(|mut entries| entries.next().is_none()) {
                    // What Docker mounts when the host path of `-v` does not exist.
                    Ok(true) => format!(
                        "{} is empty: no agent.yaml. In a container, check that the host path of the volume exists",
                        path.display()
                    ),
                    _ => format!("no agent.yaml in {}", path.display()),
                })?;
            (path.to_owned(), file)
        } else {
            (path.parent().unwrap_or(Path::new(".")).to_owned(), path.to_owned())
        };
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let config: AgentConfig = serde_yaml_ng::from_str(&expand_env(&text)?)
            .with_context(|| format!("invalid {}", file.display()))?;

        let instructions = match &config.instructions {
            Some(text) => text.clone(),
            None => std::fs::read_to_string(dir.join("instructions.md"))
                .unwrap_or_else(|_| "You are a helpful assistant.".to_owned()),
        };

        let mut names = std::collections::HashSet::new();
        for subagent in &config.subagents {
            if !names.insert(subagent.name.as_str()) {
                bail!("two subagents are named `{}`", subagent.name);
            }
        }

        let skills = crate::skills::load_all(&dir.join("skills"))?;
        let knowledge_dir = dir.join("knowledge");
        let knowledge = match knowledge_dir.is_dir() {
            true => Some(KnowledgeBase::load(&knowledge_dir)?).filter(|kb| !kb.documents.is_empty()),
            false => None,
        };

        let name = config.name.clone().unwrap_or_else(|| {
            let dir = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "agent".to_owned())
        });
        Ok(Self { dir, name, config, instructions, skills, knowledge })
    }

    /// Whether a built-in tool group is on. Knowledge and skills also need content.
    pub fn enabled(&self, builtin: Builtin) -> bool {
        let listed = self.config.tools.as_ref().is_none_or(|tools| tools.contains(&builtin));
        listed
            && match builtin {
                Builtin::Knowledge => self.knowledge.is_some(),
                Builtin::Skills => !self.skills.is_empty(),
                _ => true,
            }
    }

    /// The system prompt: the instructions, plus the skills the agent can load.
    pub fn system_prompt(&self) -> String {
        let mut prompt = self.instructions.trim_end().to_owned();
        if self.enabled(Builtin::Skills) {
            prompt.push_str(&crate::skills::prompt_section(&self.skills));
        }
        prompt
    }
}

/// Replaces `${VAR}` and `${VAR:-default}`. An unset `${VAR}` without a
/// default is an error, so a typo does not silently become an empty string.
pub fn expand_env(text: &str) -> anyhow::Result<String> {
    expand_with(text, |name| std::env::var(name).ok())
}

fn expand_with(text: &str, lookup: impl Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').context("unclosed `${` in agent.yaml")?;
        let (name, default) = match after[..end].split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (&after[..end], None),
        };
        match lookup(name).filter(|value| !value.is_empty()).or(default.map(str::to_owned)) {
            Some(value) => out.push_str(&value),
            None => bail!("agent.yaml uses ${{{name}}}, but {name} is not set"),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_variables_and_defaults() {
        let lookup = |name: &str| (name == "HOST").then(|| "mcp".to_owned());
        assert_eq!(expand_with("http://${HOST}:${PORT:-8081}/mcp", lookup).unwrap(), "http://mcp:8081/mcp");
        assert!(expand_with("${MISSING}", lookup).is_err());
        assert_eq!(expand_with("no variables", lookup).unwrap(), "no variables");
    }

    #[test]
    fn loads_the_sample_agents() {
        let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../agents"));
        for entry in std::fs::read_dir(root).unwrap() {
            let dir = entry.unwrap().path();
            if !dir.is_dir() {
                continue;
            }
            let bundle = Bundle::load(&dir).unwrap_or_else(|error| panic!("{}: {error:#}", dir.display()));
            assert!(!bundle.instructions.is_empty(), "{}", dir.display());
        }
    }

    #[test]
    fn minimal_agent_gets_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.yaml"), "instructions: Be brief.\n").unwrap();
        let bundle = Bundle::load(dir.path()).unwrap();
        assert_eq!(bundle.config.model, DEFAULT_MODEL);
        assert!(bundle.enabled(Builtin::Planning));
        assert!(!bundle.enabled(Builtin::Knowledge));
        assert!(!bundle.enabled(Builtin::Skills));
    }

    #[test]
    fn rejects_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.yaml"), "modle: openai/gpt-5.5\n").unwrap();
        assert!(Bundle::load(dir.path()).is_err());
    }
}
