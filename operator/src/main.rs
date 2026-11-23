//! The agent operator: turns `Agent` resources into running agents.
//!
//! ```text
//! agent-operator         run the controller (in a pod, or locally with your kubeconfig)
//! agent-operator crd     print the Agent CRD as YAML
//! ```
//!
//! Settings: `AGENT_IMAGE` (default `agent-runtime:dev`), `DEFAULT_BASE_URL`
//! (the model gateway for agents that set no `baseUrl`), `RUST_LOG`.

mod crd;
mod reconcile;

use kube::CustomResourceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("crd") => {
            print!("{}", serde_yaml_ng::to_string(&crd::Agent::crd())?);
            Ok(())
        }
        None | Some("run") => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,kube=warn".into()),
                )
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
                .init();
            let client = kube::Client::try_default().await?;
            reconcile::run(client, reconcile::Settings::from_env()).await
        }
        Some(other) => anyhow::bail!("unknown command `{other}`; use `crd` or no command"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// deploy/crd.yaml is generated; regenerate it after changing the types:
    /// `cargo run -q -p agent-operator -- crd > deploy/crd.yaml`
    #[test]
    fn crd_file_is_up_to_date() {
        let generated = serde_yaml_ng::to_string(&crd::Agent::crd()).unwrap();
        let committed = include_str!("../../deploy/crd.yaml");
        assert!(generated == committed, "deploy/crd.yaml is stale; regenerate it");
    }

    #[test]
    fn spec_round_trips_agent_yaml_fields() {
        let spec: crd::AgentSpec = serde_json::from_value(serde_json::json!({
            "model": "openai/azure-gpt-6-luna",
            "tools": ["planning"],
            "skills": [{ "configMap": "skill-packing-list" }],
        }))
        .unwrap();
        assert_eq!(spec.agent.model, "openai/azure-gpt-6-luna");
        assert_eq!(spec.replicas, 1);
        assert_eq!(spec.skills[0].folder(), "packing-list");
    }

    #[test]
    fn renders_agent_yaml_with_defaults() {
        let agent = crd::Agent::new(
            "pirate",
            serde_json::from_value(serde_json::json!({ "instructions": "Talk like a pirate." })).unwrap(),
        );
        let settings = reconcile::Settings { image: "x".into(), default_base_url: Some("http://gw/v1".into()) };
        let yaml = reconcile::render(&agent, &settings).unwrap();
        assert!(yaml.contains("name: pirate"), "{yaml}");
        assert!(yaml.contains("baseUrl: http://gw/v1"), "{yaml}");
        assert!(!yaml.contains("skills"), "the runtime does not know Kubernetes fields: {yaml}");
    }
}
