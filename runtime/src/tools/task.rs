//! The `task` tool hands a self-contained job to a subagent with a fresh
//! context window. It does the digging and returns only its report, so the
//! main agent's context stays small. Several `task` calls in one turn run
//! in parallel.
//!
//! Subagents are defined in agent.yaml. Each gets the agent's knowledge,
//! skills and MCP tools, but no plan, files or subagents of its own.

use std::sync::Arc;

use futures::StreamExt;
use rig::DynModel;
use rig::agent::MultiTurnStreamItem;
use rig::operation::Completion;
use rig::prelude::*;
use rig::tool::{DynamicTool, Tool, ToolContext};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{ListDocuments, LoadSkill, ReadDocument, SearchKnowledgeBase, ToolError, schema};
use crate::events::{EventSender, UiEvent};
use crate::kb::KnowledgeBase;
use crate::skills::Skill;

pub const GENERAL_PURPOSE: &str = "You are a subagent. You get one self-contained task. Use your tools to \
do it thoroughly: search with several phrasings, read sources in full when details matter, and use a \
calculator tool for arithmetic if you have one. Do not guess: every fact in your answer must come from a \
document or a tool result. Finish with a concise report: the answer, the key facts with their sources, \
and anything you could not find.";

/// One kind of subagent, with its model already connected.
pub struct Subagent {
    pub name: String,
    pub description: String,
    pub preamble: String,
    pub model: DynModel<Completion>,
    pub max_turns: usize,
}

/// What every subagent can use.
#[derive(Clone)]
pub struct SubagentTools {
    pub knowledge: Option<Arc<KnowledgeBase>>,
    pub skills: Option<Arc<Vec<Skill>>>,
    pub mcp: Vec<DynamicTool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskArgs {
    /// Which subagent to use. Defaults to the first one listed.
    pub subagent: Option<String>,
    /// A complete, self-contained description of the task. The subagent
    /// cannot see the conversation, so include every detail it needs.
    pub description: String,
}

pub struct Task {
    pub subagents: Arc<Vec<Subagent>>,
    pub tools: SubagentTools,
    pub events: EventSender,
}

impl Tool for Task {
    const NAME: &'static str = "task";
    type Error = ToolError;
    type Args = TaskArgs;
    type Output = String;

    fn description(&self) -> String {
        let list: Vec<String> =
            self.subagents.iter().map(|agent| format!("- {}: {}", agent.name, agent.description)).collect();
        format!(
            "Delegate a self-contained task to a subagent with its own context window. It returns a short \
             report. Use it for independent sub-questions; call it several times in the same turn to work \
             in parallel. Subagents:\n{}",
            list.join("\n")
        )
    }

    fn parameters(&self) -> serde_json::Value {
        schema::<TaskArgs>()
    }

    async fn call(&self, _: &mut ToolContext, args: TaskArgs) -> Result<String, ToolError> {
        let subagent = match args.subagent.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
            None => &self.subagents[0],
            Some(name) => self.subagents.iter().find(|agent| agent.name == name).ok_or_else(|| {
                let names: Vec<&str> = self.subagents.iter().map(|agent| agent.name.as_str()).collect();
                ToolError(format!("no subagent `{name}`; use one of: {}", names.join(", ")))
            })?,
        };
        let label: String = format!("{}: {}", subagent.name, args.description.chars().take(60).collect::<String>());
        let notify = |kind: &str, detail: String| {
            let _ = self.events.send(UiEvent::Subagent { task: label.clone(), kind: kind.to_owned(), detail });
        };
        notify("started", args.description.clone());

        let mut builder = AgentBuilder::new(subagent.model.clone()).preamble(&subagent.preamble).max_tokens(4096);
        if let Some(kb) = &self.tools.knowledge {
            builder = builder
                .tool(SearchKnowledgeBase { kb: kb.clone() })
                .tool(ReadDocument { kb: kb.clone() })
                .tool(ListDocuments { kb: kb.clone() });
        }
        if let Some(skills) = &self.tools.skills {
            builder = builder.tool(LoadSkill { skills: skills.clone() });
        }
        let worker = builder.dynamic_tools(self.tools.mcp.clone()).build();

        let mut stream = worker.prompt(args.description.as_str()).max_turns(subagent.max_turns).stream();
        let mut report = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(MultiTurnStreamItem::ToolCall { tool_call }) => notify(
                    "tool_call",
                    format!(
                        "{}({})",
                        tool_call.function.name,
                        serde_json::Value::Object(tool_call.function.arguments.clone())
                    ),
                ),
                Ok(MultiTurnStreamItem::FinalResponse(response)) => report = Some(response.output()),
                Ok(_) => {}
                Err(error) => {
                    notify("error", error.to_string());
                    return Err(ToolError(format!("the subagent failed: {error}")));
                }
            }
        }
        let report = report.ok_or_else(|| ToolError("the subagent finished without a report".to_owned()))?;
        notify("done", report.clone());
        Ok(report)
    }
}
