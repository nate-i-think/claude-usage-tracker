//! Platform-aware locations for app data and Cowork sessions.
//!
//! Ported from the Python paths.py (removed; see git history).

use std::path::{Path, PathBuf};

use crate::profiles::ProfileId;

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

    /// One profile's own files: `<data>/profiles/<id>/`.
    pub fn for_profile(&self, id: &ProfileId) -> Paths {
        Self::with_data_dir(self.data_dir.join("profiles").join(id.as_str()))
    }

    /// Index of known profiles for out-of-process readers (CLI, MCP).
    pub fn profiles_index_file(&self) -> PathBuf {
        self.data_dir.join("profiles.json")
    }

    /// Present once the pre-profiles root files were copied to `default`.
    pub fn legacy_marker_file(&self) -> PathBuf {
        self.data_dir.join("profiles").join(".legacy-migrated")
    }

    /// Append-only history, one JSON object per line.
    pub fn history_file(&self) -> PathBuf {
        self.data_dir.join("history.jsonl")
    }

    /// Full most-recent snapshot, for out-of-process readers.
    pub fn latest_file(&self) -> PathBuf {
        self.data_dir.join("latest.json")
    }

    /// GUI crash log (panics and startup failures), appended to.
    pub fn crash_log_file(&self) -> PathBuf {
        self.data_dir.join("crash.log")
    }
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

/// The user's home directory; empty when unknown.
pub fn home_dir() -> PathBuf {
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
    }

    #[test]
    fn profile_files_live_under_profiles_dir() {
        let p = Paths::with_data_dir(PathBuf::from("base"));
        let id = ProfileId::new("claude-secondary");
        let profile = p.for_profile(&id);
        let dir = Path::new("base").join("profiles").join("claude-secondary");
        assert_eq!(profile.data_dir(), dir);
        assert_eq!(profile.history_file(), dir.join("history.jsonl"));
        assert_eq!(profile.latest_file(), dir.join("latest.json"));
        assert_eq!(
            p.profiles_index_file(),
            Path::new("base").join("profiles.json")
        );
        assert_eq!(
            p.legacy_marker_file(),
            Path::new("base").join("profiles").join(".legacy-migrated")
        );
    }

    #[test]
    fn platform_default_ends_with_app_dir() {
        assert!(Paths::platform_default().data_dir().ends_with(APP_DIR_NAME));
    }

    #[test]
    fn cowork_dir_has_expected_suffix() {
        assert!(cowork_dir().ends_with(Path::new("Claude").join("local-agent-mode-sessions")));
    }
}
