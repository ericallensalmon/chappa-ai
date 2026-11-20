//! The folders the app keeps its state in, resolved once at startup.
//!
//! Every app-owned folder comes from here: the platform's per-user folders,
//! or, when `CHAPPA_PROFILE_DIR` names one, subfolders of that profile (see
//! [`chappa_ai_mcp::profile`], which `chappa-ai-mcp` resolves the same way).
//! A separate profile reads and writes nothing in the platform folders.

use std::path::PathBuf;

pub use chappa_ai_mcp::profile::{Profile, PROFILE_DIR_ENV};

/// Where the app's state lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    /// Settings, projects, records, sources, agent tools, the coordination
    /// database, the control token and endpoint.
    pub config: PathBuf,
    /// Logs, crash reports and dumps.
    pub logs: PathBuf,
    /// The webview's storage, when a profile moves it; `None` leaves the
    /// webview on its platform default.
    pub webview: Option<PathBuf>,
    /// The app's private data (the files launched agent CLIs are given
    /// their deny rules in, among others).
    pub data: PathBuf,
    /// Every folder the app keeps private state in: what a launched agent
    /// CLI is told to refuse. The whole profile under a profile; the
    /// platform's config, data, local data and log folders otherwise.
    pub private: Vec<PathBuf>,
}

/// The data folder's name under a profile.
pub const DATA_SUBDIR: &str = "data";

/// The platform folders, asked for only when no profile is set.
pub trait PlatformDirs {
    fn config(&self) -> Option<PathBuf>;
    fn logs(&self) -> Option<PathBuf>;
    fn data(&self) -> Option<PathBuf> {
        None
    }
    /// The per-machine data folder (the webview's default storage).
    fn local_data(&self) -> Option<PathBuf> {
        None
    }
}

impl<R: tauri::Runtime> PlatformDirs for tauri::AppHandle<R> {
    fn config(&self) -> Option<PathBuf> {
        tauri::Manager::path(self).app_config_dir().ok()
    }
    fn logs(&self) -> Option<PathBuf> {
        tauri::Manager::path(self).app_log_dir().ok()
    }
    fn data(&self) -> Option<PathBuf> {
        tauri::Manager::path(self).app_data_dir().ok()
    }
    fn local_data(&self) -> Option<PathBuf> {
        tauri::Manager::path(self).app_local_data_dir().ok()
    }
}

impl AppDirs {
    /// The folders for `profile`, or the platform ones without it. The
    /// platform is not consulted at all under a profile.
    pub fn resolve(profile: Option<&Profile>, platform: &dyn PlatformDirs) -> AppDirs {
        match profile {
            Some(profile) => AppDirs {
                config: profile.config_dir(),
                logs: profile.logs_dir(),
                webview: Some(profile.webview_dir()),
                data: profile.root().join(DATA_SUBDIR),
                private: vec![profile.root().to_path_buf()],
            },
            None => {
                let config = platform.config().unwrap_or_default();
                let logs = platform.logs().unwrap_or_else(|| config.join("logs"));
                let data = platform.data().unwrap_or_else(|| config.clone());
                let mut private: Vec<PathBuf> = Vec::new();
                for dir in [
                    Some(config.clone()),
                    Some(data.clone()),
                    platform.local_data(),
                    Some(logs.clone()),
                ]
                .into_iter()
                .flatten()
                {
                    if !dir.as_os_str().is_empty() && !private.contains(&dir) {
                        private.push(dir);
                    }
                }
                AppDirs {
                    config,
                    logs,
                    webview: None,
                    data,
                    private,
                }
            }
        }
    }
}

/// The profile this run uses, made absolute and written back into the
/// environment, so every process the app starts (`chappa-ai-mcp` above all,
/// which finds the app through the profile's endpoint file) resolves the same
/// folder whatever its working folder. Call before any thread is started.
pub fn adopt_from_env() -> Option<Profile> {
    let profile = Profile::from_env()?;
    std::env::set_var(PROFILE_DIR_ENV, profile.root());
    Some(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    struct Platform {
        config: Option<PathBuf>,
        logs: Option<PathBuf>,
    }

    impl PlatformDirs for Platform {
        fn config(&self) -> Option<PathBuf> {
            self.config.clone()
        }
        fn logs(&self) -> Option<PathBuf> {
            self.logs.clone()
        }
    }

    /// A platform that fails the test if it is asked anything.
    struct Untouchable;

    impl PlatformDirs for Untouchable {
        fn config(&self) -> Option<PathBuf> {
            panic!("platform config folder resolved under a profile")
        }
        fn logs(&self) -> Option<PathBuf> {
            panic!("platform log folder resolved under a profile")
        }
    }

    fn profile() -> Profile {
        let root = std::env::temp_dir().join("demo-profile");
        Profile::from_value(Some(root.as_os_str()), Path::new("/")).unwrap()
    }

    #[test]
    fn a_profile_holds_every_folder_and_never_asks_the_platform() {
        let profile = profile();
        let dirs = AppDirs::resolve(Some(&profile), &Untouchable);
        assert_eq!(dirs.config, profile.root().join("config"));
        assert_eq!(dirs.logs, profile.root().join("logs"));
        assert_eq!(dirs.webview, Some(profile.root().join("webview")));
        assert_eq!(dirs.private, vec![profile.root().to_path_buf()]);
        for dir in [
            &dirs.config,
            &dirs.logs,
            dirs.webview.as_ref().unwrap(),
            &dirs.data,
        ] {
            assert!(dir.starts_with(profile.root()), "{}", dir.display());
        }
    }

    #[test]
    fn without_a_profile_the_platform_folders_are_used_unchanged() {
        let platform = Platform {
            config: Some(PathBuf::from("platform-config")),
            logs: Some(PathBuf::from("platform-logs")),
        };
        let dirs = AppDirs::resolve(None, &platform);
        assert_eq!(
            dirs,
            AppDirs {
                config: PathBuf::from("platform-config"),
                logs: PathBuf::from("platform-logs"),
                webview: None,
                data: PathBuf::from("platform-config"),
                private: vec![
                    PathBuf::from("platform-config"),
                    PathBuf::from("platform-logs")
                ],
            }
        );
    }

    #[test]
    fn without_a_platform_log_folder_logs_go_beside_the_config() {
        let platform = Platform {
            config: Some(PathBuf::from("platform-config")),
            logs: None,
        };
        let dirs = AppDirs::resolve(None, &platform);
        assert_eq!(dirs.logs, PathBuf::from("platform-config").join("logs"));
    }

    #[test]
    fn every_platform_folder_with_app_state_is_private() {
        struct Full;
        impl PlatformDirs for Full {
            fn config(&self) -> Option<PathBuf> {
                Some(PathBuf::from("cfg"))
            }
            fn logs(&self) -> Option<PathBuf> {
                Some(PathBuf::from("cfg/logs"))
            }
            fn data(&self) -> Option<PathBuf> {
                Some(PathBuf::from("data"))
            }
            fn local_data(&self) -> Option<PathBuf> {
                Some(PathBuf::from("local"))
            }
        }
        let dirs = AppDirs::resolve(None, &Full);
        assert_eq!(dirs.data, PathBuf::from("data"));
        assert_eq!(
            dirs.private,
            ["cfg", "data", "local", "cfg/logs"]
                .map(PathBuf::from)
                .to_vec()
        );
    }

    #[test]
    fn the_app_and_the_mcp_server_agree_on_the_config_folder() {
        let profile = profile();
        let app = AppDirs::resolve(Some(&profile), &Untouchable).config;
        let mcp = chappa_ai_mcp::profile::config_dir_for(Some(&profile), || unreachable!());
        assert_eq!(app, mcp);
    }

    /// Every app-owned folder must come from [`AppDirs`]: a direct call to
    /// one of Tauri's per-app folder lookups anywhere else would put state
    /// outside a profile.
    #[test]
    fn no_other_source_file_asks_tauri_for_an_app_folder() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let lookups = [
            "app_config_dir",
            "app_data_dir",
            "app_local_data_dir",
            "app_cache_dir",
            "app_log_dir",
            "BaseDirectory::App",
        ];
        let mut offenders = Vec::new();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") || path.ends_with("profile.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                for lookup in lookups {
                    if text.contains(lookup) {
                        offenders.push(format!("{} uses {lookup}", path.display()));
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "{offenders:#?}");
    }
}
