//! The `shell` tool: rig-tools' [`Shell`], run in a folder of its own per
//! session that holds the session's files.
//!
//! The session's files (the scratch filesystem the UI shows, kept in the
//! session store) stay the source of truth. Before each command they are
//! written into the folder; after it, text files the command made or changed
//! come back into the session, and files it deleted leave it. Binary and
//! large files stay on disk for the rest of the turn only. So `write_file` a
//! script, then run it, and its output files show up in the UI.
//!
//! Commands run as the runtime's user, so the pod (read-only root, no
//! capabilities, its memory limit) is what contains them. What keeps them
//! from the runtime's secrets and from other chats:
//! - a clean environment: no API keys, no `DATABASE_URL`, only `PATH`, the
//!   locale and `HOME`/`TMPDIR` pointing into the session folder;
//! - [`protect_runtime`]: the runtime's own `/proc/<pid>/environ`, memory
//!   and open connections are unreadable to them;
//! - a session's folder exists only while its turn runs, under a name that
//!   does not reveal the session id. Turns of other chats running on the
//!   same pod at the same moment can still see each other's folders: give
//!   an agent whose users must not share anything one pod per user.
//!
//! The tool is only offered when the image has `sh`: the distroless image
//! has none, and an agent that lists `shell` there runs without it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rig::tool::{Tool, ToolContext};
use rig_tools::process::Launcher;
use rig_tools::{Shell, ShellArgs};
use schemars::JsonSchema;
use serde::Deserialize;

use super::files::MAX_FILE_BYTES;
use super::{SharedWorkspace, ToolError, schema};

/// Variables a command inherits from the runtime; every other one is removed.
const KEPT_ENV: &[&str] = &["PATH", "LANG", "LANGUAGE", "TZ", "SSL_CERT_FILE", "SSL_CERT_DIR"];
/// Folders that are never read back into the session: caches and installs.
const SKIPPED_DIRS: &[&str] = &["__pycache__", "node_modules", "site-packages"];
/// Most files read back into the session after one command.
const MAX_FILES: usize = 200;
/// Most entries looked at when reading the folder back.
const MAX_ENTRIES: usize = 5_000;

/// Makes the runtime process non-dumpable: the kernel then makes its
/// `/proc/<pid>` files (environ, mem, fd) root's, so the commands it starts,
/// which run as the same user, cannot read its secrets or its database
/// connection. Children are dumpable again after `exec`, which is harmless.
pub fn protect_runtime() -> std::io::Result<()> {
    // SAFETY: PR_SET_DUMPABLE takes one integer argument and touches no memory.
    match unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// Where and how long commands run, from the environment.
#[derive(Clone, Debug)]
pub struct ShellSettings {
    /// Holds one folder per session (`SHELL_DIR`, default `$TMPDIR/agent-sessions`).
    pub root: PathBuf,
    /// The time limit of a command that sets none, and the most it may set
    /// (`SHELL_TIMEOUT_SECS`, default 300).
    pub timeout_secs: u64,
    /// Keys the folder names, so they do not reveal session ids.
    pub key: std::collections::hash_map::RandomState,
}

impl ShellSettings {
    /// The settings, or `None` when this image has no `sh` to run commands with.
    pub fn from_env() -> Option<Self> {
        if !has_sh() {
            return None;
        }
        let root = std::env::var_os("SHELL_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("agent-sessions"));
        let timeout_secs = std::env::var("SHELL_TIMEOUT_SECS").ok().and_then(|secs| secs.parse().ok()).unwrap_or(300);
        Some(Self { root, timeout_secs, key: Default::default() })
    }

    /// The folder of one session: a keyed hash of its id, so a command that
    /// lists the folders learns no session ids, and no id becomes a path.
    pub fn session_dir(&self, session: &str) -> PathBuf {
        use std::hash::BuildHasher;
        self.root.join(format!("s{:016x}", self.key.hash_one(session)))
    }

    /// Deletes a session's folder, at the end of its turn.
    pub fn remove_session(&self, session: &str) {
        let dir = self.session_dir(session);
        if let Err(error) = std::fs::remove_dir_all(&dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("could not delete {}: {error}", dir.display());
        }
    }

    /// Deletes the folders of sessions that ran no command for `idle`: those
    /// a turn left behind when it did not end normally.
    /// Returns how many it deleted.
    pub fn expire(&self, idle: Duration) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.root) else { return 0 };
        let now = SystemTime::now();
        let mut gone = 0;
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() >= idle);
            if old && std::fs::remove_dir_all(entry.path()).is_ok() {
                gone += 1;
            }
        }
        gone
    }
}

/// Whether `sh` is on `PATH`.
fn has_sh() -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(dir.join("sh")).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

#[derive(Deserialize, JsonSchema)]
pub struct RunArgs {
    /// The command line, run with `sh -c`, e.g. `python3 analyze.py data.csv`.
    pub command: String,
    /// Seconds before the command is stopped. Defaults to the most allowed.
    pub timeout_secs: Option<u64>,
    /// A folder to run it in, relative to the workspace; default the workspace itself.
    pub cwd: Option<String>,
}

pub struct RunShell {
    pub workspace: SharedWorkspace,
    pub settings: Arc<ShellSettings>,
    /// The session's folder.
    pub dir: PathBuf,
    /// One command at a time: two running together would each sync the folder.
    pub busy: Arc<tokio::sync::Mutex<()>>,
}

impl Tool for RunShell {
    const NAME: &'static str = "shell";
    type Error = ToolError;
    type Args = RunArgs;
    type Output = String;

    fn description(&self) -> String {
        format!(
            "Run a shell command with `sh -c` in a folder that holds your workspace files, and wait for it \
             to end (at most {} seconds). Use it to run code: write a script with write_file, then run it, \
             e.g. `python3 script.py`. Text files the command creates or changes come back into the \
             workspace, and files it deletes leave it; binary and large files stay on disk for later \
             commands in this turn only. Returns the exit code and stdout and stderr interleaved. Each call starts a \
             fresh `sh` with an empty stdin and no network credentials. Do not run servers or anything \
             that never ends.",
            self.settings.timeout_secs
        )
    }

    fn parameters(&self) -> serde_json::Value {
        schema::<RunArgs>()
    }

    async fn call(&self, _: &mut ToolContext, args: RunArgs) -> Result<String, ToolError> {
        let _busy = self.busy.lock().await;
        let files = self.workspace.snapshot().files;
        let dir = self.dir.clone();
        let written = files.clone();
        tokio::task::spawn_blocking(move || write_out(&dir, &written))
            .await
            .map_err(|error| ToolError(format!("could not prepare the folder: {error}")))?
            .map_err(|error| ToolError(format!("could not prepare the folder: {error}")))?;

        let max = self.settings.timeout_secs.max(1);
        let timeout = args.timeout_secs.unwrap_or(max).clamp(1, max);
        let mut shell_args = ShellArgs::new(args.command).timeout_secs(timeout);
        if let Some(cwd) = args.cwd.filter(|cwd| !cwd.trim().is_empty()) {
            shell_args = shell_args.in_dir(cwd);
        }
        let shell = Shell { launcher: launcher(&self.dir), confine: true, ..Shell::default() };
        let ran = shell.run(shell_args).await.map_err(|error| ToolError(error.to_string()))?;

        let dir = self.dir.clone();
        let back = tokio::task::spawn_blocking(move || read_back(&dir))
            .await
            .map_err(|error| ToolError(format!("could not read the folder back: {error}")))?;
        let summary = self.merge(&files, back);
        Ok(match summary {
            Some(summary) => format!("{}\n{summary}", ran.output),
            None => ran.output,
        })
    }
}

impl RunShell {
    /// Brings what the command did to the files into the session, and says
    /// what changed.
    fn merge(&self, before: &BTreeMap<String, String>, back: Folder) -> Option<String> {
        let mut workspace = self.workspace.lock();
        let mut changed = Vec::new();
        let mut added = Vec::new();
        for (path, content) in back.text {
            match before.get(&path) {
                Some(old) if *old == content => {}
                Some(_) => changed.push(path.clone()),
                None => added.push(path.clone()),
            }
            workspace.files.insert(path, content);
        }
        let mut removed = Vec::new();
        for path in before.keys() {
            // Gone from disk, and not turned into a file that cannot come back.
            if !back.seen.contains(path) {
                workspace.files.remove(path);
                removed.push(path.clone());
            }
        }
        let mut parts = Vec::new();
        let list = |label: &str, paths: &[String]| format!("{label} {}", paths.join(", "));
        if !added.is_empty() {
            parts.push(list("added", &added));
        }
        if !changed.is_empty() {
            parts.push(list("updated", &changed));
        }
        if !removed.is_empty() {
            parts.push(list("removed", &removed));
        }
        if !back.disk_only.is_empty() {
            parts.push(list("on disk only (binary or over 200 KB)", &back.disk_only));
        }
        if back.truncated {
            parts.push(format!("only the first {MAX_FILES} files were read back"));
        }
        (!parts.is_empty()).then(|| format!("[workspace: {}]", parts.join("; ")))
    }
}

/// How commands start: in the session folder, with a clean environment.
fn launcher(dir: &Path) -> Launcher {
    let removed: Vec<OsString> = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| {
            let name = name.to_string_lossy();
            !KEPT_ENV.contains(&name.as_ref()) && !name.starts_with("LC_")
        })
        .collect();
    Launcher::default()
        .in_dir(dir)
        .without_env(removed)
        .with_env("HOME", dir)
        .with_env("TMPDIR", dir.join(".tmp"))
        .with_env("PYTHONUNBUFFERED", "1")
        .with_env("PYTHONDONTWRITEBYTECODE", "1")
}

/// Writes the session's files into its folder, leaving files that are
/// already up to date alone, and marks the folder as used now.
fn write_out(dir: &Path, files: &BTreeMap<String, String>) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join(".tmp"))?;
    for (path, content) in files {
        let target = dir.join(path);
        if std::fs::read(&target).is_ok_and(|bytes| bytes == content.as_bytes()) {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content)?;
    }
    // Expiry goes by the folder's time, which nested writes do not change.
    std::fs::File::open(dir)?.set_modified(SystemTime::now())
}

/// What a command left in the folder.
#[derive(Default)]
struct Folder {
    /// Text files small enough for the session, by relative path.
    text: BTreeMap<String, String>,
    /// Every file path seen, text or not.
    seen: std::collections::HashSet<String>,
    /// Files that stay on disk: binary, or too large.
    disk_only: Vec<String>,
    truncated: bool,
}

/// Reads the folder back, skipping hidden entries (`.tmp`, `.cache`, a
/// `.venv`), caches and symlinks.
fn read_back(dir: &Path) -> Folder {
    let mut folder = Folder::default();
    let mut pending = vec![dir.to_path_buf()];
    let mut entries_seen = 0;
    while let Some(current) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            entries_seen += 1;
            if entries_seen > MAX_ENTRIES {
                folder.truncated = true;
                return folder;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_dir() {
                if !SKIPPED_DIRS.contains(&name.as_str()) {
                    pending.push(path);
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let Some(relative) = path.strip_prefix(dir).ok().and_then(Path::to_str).map(str::to_owned) else {
                continue;
            };
            folder.seen.insert(relative.clone());
            let small = entry.metadata().is_ok_and(|meta| meta.len() <= MAX_FILE_BYTES as u64);
            // Text is valid UTF-8 without NUL bytes, which UTF-8 allows but text files do not have.
            let text = small
                .then(|| std::fs::read(&path).ok().filter(|bytes| !bytes.contains(&0)))
                .flatten()
                .and_then(|bytes| String::from_utf8(bytes).ok());
            match text {
                Some(_) if folder.text.len() >= MAX_FILES => folder.truncated = true,
                Some(text) => {
                    folder.text.insert(relative, text);
                }
                None => folder.disk_only.push(relative),
            }
        }
    }
    folder
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Workspace;

    fn tool(root: &Path, files: &[(&str, &str)]) -> RunShell {
        let settings = Arc::new(ShellSettings { root: root.to_owned(), timeout_secs: 10, key: Default::default() });
        let workspace = Workspace {
            files: files.iter().map(|(path, text)| (path.to_string(), text.to_string())).collect(),
            ..Default::default()
        };
        RunShell {
            workspace: SharedWorkspace::new(workspace),
            dir: settings.session_dir("s1"),
            settings,
            busy: Default::default(),
        }
    }

    async fn run(tool: &RunShell, command: &str) -> String {
        let args = RunArgs { command: command.to_owned(), timeout_secs: None, cwd: None };
        tool.call(&mut ToolContext::default(), args).await.unwrap()
    }

    #[test]
    fn folder_names_hide_session_ids() {
        let settings = ShellSettings { root: "/r".into(), timeout_secs: 1, key: Default::default() };
        let dir = settings.session_dir("abc-123");
        assert_eq!(dir, settings.session_dir("abc-123"), "stable within a process");
        assert_ne!(dir, settings.session_dir("abc-124"));
        assert!(!dir.to_string_lossy().contains("abc"));
        assert_eq!(settings.session_dir("../../etc").parent(), Some(Path::new("/r")));
    }

    #[tokio::test]
    async fn runs_on_the_session_files_and_brings_back_changes() {
        let root = tempfile::tempdir().unwrap();
        let tool = tool(root.path(), &[("in.txt", "3\n4\n"), ("gone.txt", "x"), ("notes/keep.md", "k")]);
        let output = run(&tool, "cat in.txt; sort -r in.txt > out.txt; rm gone.txt; printf '\\0' > blob.bin").await;
        assert!(output.contains("3\n4"), "{output}");
        assert!(output.contains("[exit code 0]"), "{output}");
        assert!(output.contains("added out.txt"), "{output}");
        assert!(output.contains("removed gone.txt"), "{output}");
        assert!(output.contains("on disk only (binary or over 200 KB) blob.bin"), "{output}");
        let files = tool.workspace.snapshot().files;
        assert_eq!(files.get("out.txt").map(String::as_str), Some("4\n3\n"));
        assert_eq!(files.get("notes/keep.md").map(String::as_str), Some("k"));
        assert!(!files.contains_key("gone.txt") && !files.contains_key("blob.bin"));
    }

    #[tokio::test]
    async fn commands_get_a_clean_environment() {
        let root = tempfile::tempdir().unwrap();
        // SAFETY: tests in this module do not read this variable concurrently.
        unsafe { std::env::set_var("OPENAI_API_KEY", "secret") };
        let tool = tool(root.path(), &[]);
        let output = run(&tool, "echo key=${OPENAI_API_KEY:-none} home=$HOME").await;
        assert!(output.contains("key=none"), "{output}");
        assert!(output.contains(&format!("home={}", tool.dir.display())), "{output}");
    }

    #[tokio::test]
    async fn stops_commands_at_the_limit() {
        let root = tempfile::tempdir().unwrap();
        let tool = tool(root.path(), &[]);
        let args = RunArgs { command: "sleep 5".to_owned(), timeout_secs: Some(1), cwd: None };
        let output = tool.call(&mut ToolContext::default(), args).await.unwrap();
        assert!(output.contains("timed out after 1s"), "{output}");
    }

    #[test]
    fn expires_idle_session_folders() {
        let root = tempfile::tempdir().unwrap();
        let settings = ShellSettings { root: root.path().to_owned(), timeout_secs: 1, key: Default::default() };
        write_out(&settings.session_dir("old"), &BTreeMap::new()).unwrap();
        assert_eq!(settings.expire(Duration::from_secs(3600)), 0);
        assert_eq!(settings.expire(Duration::ZERO), 1);
        assert!(!settings.session_dir("old").exists());
    }
}
