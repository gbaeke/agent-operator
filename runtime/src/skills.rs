//! Skills in the Agent Skills format: a folder with a `SKILL.md` whose
//! frontmatter has a `name` and a `description`, plus any reference files.
//!
//! Only the names and descriptions go into the system prompt. The agent
//! calls `load_skill` to read a skill's instructions when a request matches,
//! and to read its other files, so many skills cost few tokens.
//!
//! Skills are read into memory at startup. The agent cannot run a skill's
//! scripts: the image has no shell. Scripts are readable like other files.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;

const MAX_FILE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// SKILL.md without its frontmatter.
    pub instructions: String,
    /// The skill's other text files, by path relative to the skill folder.
    pub files: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: String,
}

/// Every skill in `dir`, sorted by name. A missing folder means no skills.
pub fn load_all(dir: &Path) -> anyhow::Result<Vec<Skill>> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(Vec::new()) };
    let mut skills = Vec::new();
    for entry in entries {
        let path = entry?.path();
        // Kubernetes mounts ConfigMaps with hidden `..data` folders; skip them.
        if hidden(&path) || !path.join("SKILL.md").is_file() {
            continue;
        }
        skills.push(load(&path).with_context(|| format!("skill {}", path.display()))?);
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    for pair in skills.windows(2) {
        if pair[0].name == pair[1].name {
            bail!("two skills are named `{}`", pair[0].name);
        }
    }
    Ok(skills)
}

fn load(dir: &Path) -> anyhow::Result<Skill> {
    let text = std::fs::read_to_string(dir.join("SKILL.md"))?;
    let (frontmatter, instructions) = split_frontmatter(&text).context("SKILL.md needs a --- frontmatter --- block")?;
    let meta: Frontmatter = serde_yaml_ng::from_str(frontmatter).context("SKILL.md frontmatter needs a description")?;
    let folder = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut files = BTreeMap::new();
    collect_files(dir, dir, &mut files)?;
    files.remove("SKILL.md");
    Ok(Skill {
        name: meta.name.unwrap_or(folder),
        description: meta.description.trim().to_owned(),
        instructions: instructions.trim().to_owned(),
        files,
    })
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start_matches('\u{feff}').strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let body = &rest[end + 4..];
    Some((&rest[..end], body.split_once('\n').map_or("", |(_, body)| body)))
}

fn hidden(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

fn collect_files(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if hidden(&path) {
            continue;
        }
        // `metadata` follows symlinks, as ConfigMap volumes use them.
        let meta = std::fs::metadata(&path)?;
        if meta.is_dir() {
            collect_files(root, &path, files)?;
        } else if meta.len() <= MAX_FILE_BYTES
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            let relative = path.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
            files.insert(relative, content);
        }
    }
    Ok(())
}

/// The part of the system prompt that lists the skills.
pub fn prompt_section(skills: &[Skill]) -> String {
    let list: Vec<String> = skills.iter().map(|skill| format!("- {}: {}", skill.name, skill.description)).collect();
    format!(
        "\n\n## Skills\n\nSkills hold the instructions for specific kinds of tasks. When a request matches a \
         skill's description, call load_skill with its name before you start, and follow what it says.\n\n{}",
        list.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_skill_folder() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("summarize");
        std::fs::create_dir_all(skill.join("templates")).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: summarize\ndescription: Summarize a document.\n---\n\n# Steps\n\nBe short.\n",
        )
        .unwrap();
        std::fs::write(skill.join("templates/short.md"), "TL;DR").unwrap();
        std::fs::create_dir(dir.path().join("..data")).unwrap();

        let skills = load_all(dir.path()).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "Summarize a document.");
        assert_eq!(skills[0].instructions, "# Steps\n\nBe short.");
        assert_eq!(skills[0].files.keys().collect::<Vec<_>>(), ["templates/short.md"]);
    }

    #[test]
    fn missing_folder_means_no_skills() {
        assert!(load_all(Path::new("/does/not/exist")).unwrap().is_empty());
    }
}
