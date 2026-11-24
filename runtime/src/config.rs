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

use crate::kb::KnowledgeBase;
use crate::skills::Skill;

pub use agent_spec::{AgentConfig, Builtin, McpServer};
#[cfg(test)]
use agent_spec::DEFAULT_MODEL;

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
        let config = parse(&expand_env(&text)?).with_context(|| format!("invalid {}", file.display()))?;

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

    /// Whether a built-in tool group is on. Knowledge and skills also need
    /// content. The shell is never on by default: it must be listed.
    pub fn enabled(&self, builtin: Builtin) -> bool {
        let listed = match &self.config.tools {
            Some(tools) => tools.contains(&builtin),
            None => builtin != Builtin::Shell,
        };
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

/// Parses agent.yaml and rejects unknown keys, so a typo like `modle:`
/// fails loudly instead of being ignored.
fn parse(text: &str) -> anyhow::Result<AgentConfig> {
    let mut unknown = Vec::new();
    let config = serde_ignored::deserialize(serde_yaml_ng::Deserializer::from_str(text), |path| {
        unknown.push(path.to_string());
    })?;
    if !unknown.is_empty() {
        bail!("unknown keys: {}", unknown.join(", "));
    }
    Ok(config)
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
        assert!(!bundle.enabled(Builtin::Shell), "the shell must be listed to be on");
    }

    #[test]
    fn shell_is_on_when_listed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.yaml"), "tools: [files, shell]\n").unwrap();
        let bundle = Bundle::load(dir.path()).unwrap();
        assert!(bundle.enabled(Builtin::Shell));
        assert!(bundle.enabled(Builtin::Files));
        assert!(!bundle.enabled(Builtin::Planning));
    }

    #[test]
    fn rejects_unknown_fields() {
        let error = parse("modle: openai/gpt-5.5\n").unwrap_err().to_string();
        assert!(error.contains("modle"), "{error}");
        let nested = parse("mcpServers:\n  - url: http://x\n    tool: [a]\n").unwrap_err().to_string();
        assert!(nested.contains("tool"), "{nested}");
    }
}
