//! `load_skill`: reads a skill's instructions, or one of its files.

use std::sync::Arc;

use rig::tool::{Tool, ToolContext};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{ToolError, schema};
use crate::skills::Skill;

#[derive(Deserialize, JsonSchema)]
pub struct LoadSkillArgs {
    /// The skill's name, as listed in the system prompt.
    pub name: String,
    /// A file of the skill, e.g. `templates/report.md`. Leave it out for the skill's instructions.
    pub file: Option<String>,
}

pub struct LoadSkill {
    pub skills: Arc<Vec<Skill>>,
}

impl Tool for LoadSkill {
    const NAME: &'static str = "load_skill";
    type Error = ToolError;
    type Args = LoadSkillArgs;
    type Output = String;

    fn description(&self) -> String {
        "Load a skill's instructions before doing a task it covers. The result lists the skill's \
         other files; call again with `file` to read one."
            .to_owned()
    }

    fn parameters(&self) -> serde_json::Value {
        schema::<LoadSkillArgs>()
    }

    async fn call(&self, _: &mut ToolContext, args: LoadSkillArgs) -> Result<String, ToolError> {
        let skill = self.skills.iter().find(|skill| skill.name == args.name.trim()).ok_or_else(|| {
            let names: Vec<&str> = self.skills.iter().map(|skill| skill.name.as_str()).collect();
            ToolError(format!("no skill `{}`; the skills are: {}", args.name, names.join(", ")))
        })?;
        match args.file.as_deref().map(str::trim).filter(|file| !file.is_empty()) {
            Some(file) => skill
                .files
                .get(file.trim_start_matches("./"))
                .cloned()
                .ok_or_else(|| ToolError(format!("skill `{}` has no file `{file}`", skill.name))),
            None if skill.files.is_empty() => Ok(skill.instructions.clone()),
            None => {
                let files: Vec<&str> = skill.files.keys().map(String::as_str).collect();
                Ok(format!("{}\n\n---\nFiles in this skill: {}", skill.instructions, files.join(", ")))
            }
        }
    }
}
