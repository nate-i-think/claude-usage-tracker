//! Platform-aware locations for app data, Claude Code, and Cowork sessions.
//!
//! Port of paths.py.

use std::fs;
use std::path::{Path, PathBuf};

const APP_DIR_NAME: &str = "claude_usage_tui";

/// The app's data directory and the files inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    data_dir: PathBuf,
}

impl Paths {
    /// The per-user default: `<user data root>/claude_usage_tui`.
    ///
    /// Windows `%APPDATA%`, macOS `~/Library/Application Support`, Linux
    /// `$XDG_DATA_HOME` else `~/.local/share`.
    pub fn platform_default() -> Self {
        Self::with_data_dir(user_data_root().join(APP_DIR_NAME))
    }

    /// Use an explicit data directory (tests, overrides).
    pub fn with_data_dir(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Append-only history, one JSON object per line.
    pub fn history_file(&self) -> PathBuf {
        self.data_dir.join("history.jsonl")
    }

    /// Full most-recent snapshot, for out-of-process readers.
    pub fn latest_file(&self) -> PathBuf {
        self.data_dir.join("latest.json")
    }

    pub fn anonymous_id_file(&self) -> PathBuf {
        self.data_dir.join("anonymous_id")
    }

    /// Per-install anonymous id (`claudeai.v1.<uuid4>`), created on first use.
    ///
    /// An unreadable or empty file is replaced. A failed write is ignored so
    /// the caller still gets a usable id for this run.
    pub fn get_or_create_anonymous_id(&self) -> String {
        let file = self.anonymous_id_file();
        if let Ok(existing) = fs::read_to_string(&file) {
            let existing = existing.trim();
            if !existing.is_empty() {
                return existing.to_owned();
            }
        }
        let value = format!("claudeai.v1.{}", uuid::Uuid::new_v4());
        let _ = fs::create_dir_all(&self.data_dir).and_then(|()| fs::write(&file, &value));
        value
    }
}

/// Claude Code project session JSONLs: `~/.claude/projects`.
pub fn claude_code_dir() -> PathBuf {
    home_dir().join(".claude").join("projects")
}

/// Where the Cowork desktop app stores local agent session JSONLs.
pub fn cowork_dir() -> PathBuf {
    let base = if cfg!(windows) {
        PathBuf::from(env_or_empty("APPDATA"))
    } else if cfg!(target_os = "macos") {
        home_dir().join("Library").join("Application Support")
    } else {
        home_dir().join(".config")
    };
    base.join("Claude").join("local-agent-mode-sessions")
}

fn user_data_root() -> PathBuf {
    if cfg!(windows) {
        if let Some(appdata) = non_empty_env("APPDATA") {
            return PathBuf::from(appdata);
        }
    }
    if cfg!(target_os = "macos") {
        return home_dir().join("Library").join("Application Support");
    }
    if let Some(xdg) = non_empty_env("XDG_DATA_HOME") {
        return PathBuf::from(xdg);
    }
    home_dir().join(".local").join("share")
}

pub(crate) fn home_dir() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

pub(crate) fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_or_empty(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_live_under_data_dir() {
        let p = Paths::with_data_dir(PathBuf::from("base"));
        assert_eq!(p.data_dir(), Path::new("base"));
        assert_eq!(p.history_file(), Path::new("base").join("history.jsonl"));
        assert_eq!(p.latest_file(), Path::new("base").join("latest.json"));
        assert_eq!(
            p.anonymous_id_file(),
            Path::new("base").join("anonymous_id")
        );
    }

    #[test]
    fn platform_default_ends_with_app_dir() {
        assert!(Paths::platform_default().data_dir().ends_with(APP_DIR_NAME));
    }

    #[test]
    fn session_dirs_have_expected_suffixes() {
        assert!(claude_code_dir().ends_with(Path::new(".claude").join("projects")));
        assert!(cowork_dir().ends_with(Path::new("Claude").join("local-agent-mode-sessions")));
    }

    #[test]
    fn anonymous_id_created_in_missing_dir_then_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::with_data_dir(tmp.path().join("nested").join("dir"));

        let id = p.get_or_create_anonymous_id();
        let uuid_part = id.strip_prefix("claudeai.v1.").expect("prefix");
        let parsed = uuid::Uuid::parse_str(uuid_part).expect("uuid");
        assert_eq!(parsed.get_version_num(), 4);
        assert_eq!(fs::read_to_string(p.anonymous_id_file()).unwrap(), id);

        assert_eq!(p.get_or_create_anonymous_id(), id);
    }

    #[test]
    fn anonymous_id_existing_value_is_trimmed() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::with_data_dir(tmp.path().to_path_buf());
        fs::write(p.anonymous_id_file(), "  claudeai.v1.existing\n").unwrap();
        assert_eq!(p.get_or_create_anonymous_id(), "claudeai.v1.existing");
    }

    #[test]
    fn anonymous_id_empty_file_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::with_data_dir(tmp.path().to_path_buf());
        fs::write(p.anonymous_id_file(), "  \n").unwrap();
        let id = p.get_or_create_anonymous_id();
        assert!(id.starts_with("claudeai.v1."));
        assert_eq!(fs::read_to_string(p.anonymous_id_file()).unwrap(), id);
    }
}
