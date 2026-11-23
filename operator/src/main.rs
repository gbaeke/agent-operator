//! The agent operator.
//!
//! ```text
//! agent-operator crd    print the Agent CRD as YAML
//! ```

mod crd;

use kube::CustomResourceExt;

fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("crd") => {
            print!("{}", serde_yaml_ng::to_string(&crd::Agent::crd())?);
            Ok(())
        }
        _ => anyhow::bail!("usage: agent-operator crd"),
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
}
