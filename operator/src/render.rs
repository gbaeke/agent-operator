//! `agent-operator render <folder>`: an agent folder as Kubernetes objects,
//! ready for `kubectl apply -f -`:
//!
//! - one ConfigMap `<agent>-skill-<skill>` per folder in `skills/`;
//! - ConfigMap `<agent>-knowledge` with the `.md` files of `knowledge/`;
//! - the `Agent`, with agent.yaml as its spec and instructions.md as
//!   `instructions` if agent.yaml has none.
//!
//! `${VAR}` placeholders are kept: the pod resolves them from its
//! environment, which the operator fills from its `AGENT_ENV_*` settings,
//! as compose does for containers.

use std::collections::BTreeMap;
use std::path::Path;

use agent_spec::AgentConfig;
use anyhow::{Context, bail};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::ObjectMeta;

use crate::crd::{Agent, AgentSpec, ConfigMapRef, SkillRef};

/// The most a ConfigMap may hold, with some room for its metadata.
const MAX_CONFIG_MAP_BYTES: usize = 1_000_000;

pub struct Rendered {
    pub config_maps: Vec<ConfigMap>,
    pub agent: Agent,
}

impl Rendered {
    /// All objects as one multi-document YAML stream, ConfigMaps first.
    pub fn to_yaml(&self) -> anyhow::Result<String> {
        let mut documents = Vec::new();
        for config_map in &self.config_maps {
            documents.push(serde_yaml_ng::to_string(config_map)?);
        }
        documents.push(serde_yaml_ng::to_string(&self.agent)?);
        Ok(documents.join("---\n"))
    }
}

pub fn render(dir: &Path, name: Option<&str>, namespace: &str) -> anyhow::Result<Rendered> {
    let file = ["agent.yaml", "agent.yml"]
        .iter()
        .map(|file| dir.join(file))
        .find(|file| file.is_file())
        .with_context(|| format!("no agent.yaml in {}", dir.display()))?;
    let text = std::fs::read_to_string(&file)?;
    let mut unknown = Vec::new();
    let mut config: AgentConfig =
        serde_ignored::deserialize(serde_yaml_ng::Deserializer::from_str(&text), |path| unknown.push(path.to_string()))
            .with_context(|| format!("invalid {}", file.display()))?;
    if !unknown.is_empty() {
        bail!("{}: unknown keys: {}", file.display(), unknown.join(", "));
    }
    if config.instructions.is_none()
        && let Ok(instructions) = std::fs::read_to_string(dir.join("instructions.md"))
    {
        config.instructions = Some(instructions);
    }

    let folder = std::fs::canonicalize(dir)?.file_name().map(|name| name.to_string_lossy().into_owned());
    let name = dns_name(name.map(str::to_owned).or(folder).as_deref().unwrap_or("agent"))?;
    let labels = BTreeMap::from([("app.kubernetes.io/part-of".to_owned(), name.clone())]);
    let config_map = |object: &str, data: BTreeMap<String, String>| ConfigMap {
        metadata: ObjectMeta {
            name: Some(object.to_owned()),
            namespace: Some(namespace.to_owned()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let mut config_maps = Vec::new();
    let mut skills = Vec::new();
    for skill_dir in subfolders(&dir.join("skills"))? {
        if !skill_dir.join("SKILL.md").is_file() {
            continue;
        }
        let skill = skill_dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let object = dns_name(&format!("{name}-skill-{skill}"))?;
        let data = files(&skill_dir, |_| true).with_context(|| format!("skill {skill}"))?;
        config_maps.push(config_map(&object, data));
        skills.push(SkillRef { config_map: object, name: Some(skill) });
    }

    let knowledge_dir = dir.join("knowledge");
    let mut knowledge = None;
    if knowledge_dir.is_dir() {
        let data = files(&knowledge_dir, |file| file.ends_with(".md")).context("knowledge")?;
        if !data.is_empty() {
            let object = format!("{name}-knowledge");
            config_maps.push(config_map(&object, data));
            knowledge = Some(ConfigMapRef { config_map: object });
        }
    }

    for config_map in &config_maps {
        let size: usize = config_map.data.iter().flatten().map(|(key, value)| key.len() + value.len()).sum();
        if size > MAX_CONFIG_MAP_BYTES {
            bail!("{} would hold {size} bytes; a ConfigMap holds at most 1 MiB", config_map.metadata.name.as_deref().unwrap_or(""));
        }
    }

    let spec = AgentSpec {
        agent: config,
        skills,
        knowledge,
        replicas: 1,
        image: None,
        env: Vec::new(),
        resources: None,
    };
    let mut agent = Agent::new(&name, spec);
    agent.metadata.namespace = Some(namespace.to_owned());
    Ok(Rendered { config_maps, agent })
}

fn subfolders(dir: &Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(Vec::new()) };
    let mut folders: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir() && !hidden(path))
        .collect();
    folders.sort();
    Ok(folders)
}

/// The files directly in `dir` that `keep` accepts, by file name. ConfigMap
/// keys cannot hold a `/`, so a subfolder is an error rather than dropped.
fn files(dir: &Path, keep: impl Fn(&str) -> bool) -> anyhow::Result<BTreeMap<String, String>> {
    let mut data = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if hidden(&path) {
            continue;
        }
        let file = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if path.is_dir() {
            bail!("{} is a folder; a ConfigMap cannot hold folders, so keep its files next to SKILL.md", path.display());
        }
        if keep(&file) {
            let content = std::fs::read_to_string(&path).with_context(|| format!("{} is not text", path.display()))?;
            data.insert(file, content);
        }
    }
    Ok(data)
}

fn hidden(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

/// A valid Kubernetes object name (DNS-1123 subdomain) made from `text`.
fn dns_name(text: &str) -> anyhow::Result<String> {
    let name: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' })
        .collect();
    let name = name.trim_matches(|c| c == '-' || c == '.').to_owned();
    if name.is_empty() || name.len() > 253 {
        bail!("`{text}` cannot be made into a Kubernetes name");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../agents")
    }

    #[test]
    fn renders_quokka() {
        let rendered = render(&samples().join("quokka"), None, "agents").unwrap();
        let names: Vec<_> = rendered.config_maps.iter().map(|cm| cm.metadata.name.clone().unwrap()).collect();
        assert_eq!(names, ["quokka-skill-customer-email", "quokka-skill-sla-credit", "quokka-knowledge"]);
        assert_eq!(rendered.config_maps[1].data.as_ref().unwrap().len(), 2, "SKILL.md and worked-example.md");
        let spec = &rendered.agent.spec;
        assert!(spec.agent.instructions.as_deref().unwrap().contains("Quokka Assistant"), "from instructions.md");
        assert_eq!(spec.skills[1].folder(), "sla-credit");
        // Placeholders are left for the pod to resolve.
        assert!(spec.agent.mcp_servers[0].url.starts_with("${MCP_URL"));
    }

    #[test]
    fn makes_kubernetes_names() {
        assert_eq!(dns_name("Trip_Planner!").unwrap(), "trip-planner");
        assert!(dns_name("___").is_err());
    }

    /// deploy/samples/ is generated from agents/; regenerate it with
    /// `deploy/samples.sh` after changing a sample agent.
    #[test]
    fn samples_are_up_to_date() {
        for entry in std::fs::read_dir(samples()).unwrap() {
            let dir = entry.unwrap().path();
            if !dir.is_dir() {
                continue;
            }
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            let generated = render(&dir, None, "agents").unwrap().to_yaml().unwrap();
            let file = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../deploy/samples/{name}.yaml"));
            let committed = std::fs::read_to_string(&file).unwrap_or_default();
            assert!(generated == committed, "{} is stale; run deploy/samples.sh", file.display());
        }
    }
}
