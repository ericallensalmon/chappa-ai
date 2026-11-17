//! Where the app keeps its state: the platform's per-user folders, or one
//! folder named by [`PROFILE_DIR_ENV`].
//!
//! The variable gives a run a profile of its own (a demo, a test, a second
//! setup beside the everyday one): its settings, projects, records, logs,
//! crash dumps, control token and endpoint and the webview's storage all live
//! under that folder, and nothing outside it is read or written. The app and
//! this crate resolve the folders here so they cannot disagree about where
//! the control endpoint file is.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Names the folder that holds a separate profile. Unset or empty: the
/// platform folders.
pub const PROFILE_DIR_ENV: &str = "CHAPPA_PROFILE_DIR";

/// Inside a profile folder: settings, projects, records, the coordination
/// database, the control token and endpoint.
pub const CONFIG_SUBDIR: &str = "config";
/// Inside a profile folder: logs, crash reports and dumps.
pub const LOGS_SUBDIR: &str = "logs";
/// Inside a profile folder: the webview's own storage.
pub const WEBVIEW_SUBDIR: &str = "webview";

/// A profile folder's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    root: PathBuf,
}

impl Profile {
    /// The profile a [`PROFILE_DIR_ENV`] value names, or `None` for an unset
    /// or empty one. A relative value is taken from `cwd`, so the app and
    /// every process it starts (which may run elsewhere) agree on the folder.
    pub fn from_value(value: Option<&OsStr>, cwd: &Path) -> Option<Profile> {
        let value = value.filter(|v| !v.is_empty())?;
        let path = Path::new(value);
        let root = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        Some(Profile { root })
    }

    /// The profile this process's environment names, if any.
    pub fn from_env() -> Option<Profile> {
        let value = std::env::var_os(PROFILE_DIR_ENV)?;
        let cwd = std::env::current_dir().unwrap_or_default();
        Profile::from_value(Some(&value), &cwd)
    }

    /// The folder itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stands in for the platform app-config folder.
    pub fn config_dir(&self) -> PathBuf {
        self.root.join(CONFIG_SUBDIR)
    }

    /// Stands in for the platform app-log folder.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join(LOGS_SUBDIR)
    }

    /// Stands in for the webview's platform data folder.
    pub fn webview_dir(&self) -> PathBuf {
        self.root.join(WEBVIEW_SUBDIR)
    }
}

/// The platform app-config folder for [`crate::APP_IDENTIFIER`], the one
/// Tauri v2's `app_config_dir` names: `%APPDATA%\<id>` on Windows,
/// `~/Library/Application Support/<id>` on macOS,
/// `$XDG_CONFIG_HOME/<id>` (or `~/.config/<id>`) elsewhere. Ignores
/// [`PROFILE_DIR_ENV`]; see [`config_dir`].
pub fn platform_config_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_default().join(crate::APP_IDENTIFIER)
}

/// The app-config folder this process should use: the profile's when
/// [`PROFILE_DIR_ENV`] is set, else the platform one.
pub fn config_dir() -> PathBuf {
    config_dir_for(Profile::from_env().as_ref(), platform_config_dir)
}

/// [`config_dir`] with its inputs passed in.
pub fn config_dir_for(profile: Option<&Profile>, platform: impl FnOnce() -> PathBuf) -> PathBuf {
    match profile {
        Some(profile) => profile.config_dir(),
        None => platform(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn cwd() -> PathBuf {
        std::env::temp_dir().join("profile-cwd")
    }

    #[test]
    fn unset_or_empty_value_means_no_profile() {
        assert_eq!(Profile::from_value(None, &cwd()), None);
        assert_eq!(Profile::from_value(Some(OsStr::new("")), &cwd()), None);
    }

    #[test]
    fn absolute_value_is_the_root() {
        let root = std::env::temp_dir().join("demo-profile");
        let profile = Profile::from_value(Some(root.as_os_str()), &cwd()).unwrap();
        assert_eq!(profile.root(), root);
        assert_eq!(profile.config_dir(), root.join("config"));
        assert_eq!(profile.logs_dir(), root.join("logs"));
        assert_eq!(profile.webview_dir(), root.join("webview"));
    }

    #[test]
    fn relative_value_is_taken_from_the_working_folder() {
        let value = OsString::from("demo/profile");
        let profile = Profile::from_value(Some(&value), &cwd()).unwrap();
        assert_eq!(profile.root(), cwd().join("demo").join("profile"));
        assert!(profile.root().is_absolute());
    }

    #[test]
    fn config_dir_uses_the_profile_and_never_asks_the_platform() {
        let root = std::env::temp_dir().join("demo-profile");
        let profile = Profile::from_value(Some(root.as_os_str()), &cwd()).unwrap();
        let dir = config_dir_for(Some(&profile), || panic!("platform folder resolved"));
        assert_eq!(dir, root.join("config"));
    }

    #[test]
    fn config_dir_without_a_profile_is_the_platform_folder() {
        let platform = PathBuf::from("platform-config");
        assert_eq!(config_dir_for(None, || platform.clone()), platform);
    }

    #[test]
    fn platform_folder_is_named_after_the_app() {
        assert!(platform_config_dir().ends_with(crate::APP_IDENTIFIER));
    }
}
