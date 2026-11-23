//! The agent operator: turns `Agent` resources into running agents.
//!
//! ```text
//! agent-operator                      run the controller (in a pod, or locally with your kubeconfig)
//! agent-operator crd                  print the Agent CRD as YAML
//! agent-operator render <folder> [--name NAME] [--namespace NS]
//!                                     print an agent folder as an Agent and its ConfigMaps
//! ```
//!
//! Settings: `AGENT_IMAGE` (default `agent-runtime:dev`), `DEFAULT_BASE_URL`
//! (the model gateway for agents that set no `baseUrl`), `AGENT_ENV_<NAME>`
//! (passed to every agent pod as `<NAME>`), `SESSION_DATABASE_SECRET` (a
//! Secret whose `url` becomes the pods' `DATABASE_URL`, with a schema per
//! agent), `GATEWAY_MODELS_NAMESPACE` (check agents' models against the
//! gateway's AgentgatewayModels there), `RUST_LOG`.

mod crd;
mod reconcile;
mod render;

use kube::CustomResourceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("crd") => {
            print!("{}", serde_yaml_ng::to_string(&crd::Agent::crd())?);
            Ok(())
        }
        Some("render") => {
            let mut args = std::env::args().skip(2);
            let (mut dir, mut name, mut namespace) = (None, None, "agents".to_owned());
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--name" => name = Some(args.next().ok_or_else(|| anyhow::anyhow!("--name needs a value"))?),
                    "--namespace" | "-n" => {
                        namespace = args.next().ok_or_else(|| anyhow::anyhow!("--namespace needs a value"))?
                    }
                    flag if flag.starts_with('-') => anyhow::bail!("unknown option {flag}"),
                    path => dir = Some(std::path::PathBuf::from(path)),
                }
            }
            let dir = dir.ok_or_else(|| anyhow::anyhow!("usage: agent-operator render <folder> [--name NAME] [--namespace NS]"))?;
            print!("{}", render::render(&dir, name.as_deref(), &namespace)?.to_yaml()?);
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
        Some(other) => anyhow::bail!("unknown command `{other}`; use `crd`, `render` or no command"),
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
        let settings =
            reconcile::Settings {
                image: "x".into(),
                default_base_url: Some("http://gw/v1".into()),
                agent_env: Vec::new(),
                session_secret: None,
                models_namespace: None,
            };
        let yaml = reconcile::render(&agent, &settings).unwrap();
        assert!(yaml.contains("name: pirate"), "{yaml}");
        assert!(yaml.contains("baseUrl: http://gw/v1"), "{yaml}");
        assert!(!yaml.contains("skills"), "the runtime does not know Kubernetes fields: {yaml}");
    }
}
