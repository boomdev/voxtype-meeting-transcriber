use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{AppError, Result};

const APP_NAME: &str = "voxtype-meeting-service";

/// Resolves XDG config, data, and runtime paths.
///
/// Environment roots can be injected so tests do not mutate process-global env.
#[derive(Clone, Debug)]
pub struct PathResolver {
    home: PathBuf,
    xdg_config_home: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
    uid: u32,
}

impl PathResolver {
    pub fn from_env() -> Result<Self> {
        let home = match env::var_os("HOME") {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => {
                return Err(AppError::path(
                    "HOME is not set; cannot resolve XDG directories",
                ));
            }
        };

        Ok(Self {
            home,
            xdg_config_home: env_path("XDG_CONFIG_HOME"),
            xdg_data_home: env_path("XDG_DATA_HOME"),
            xdg_runtime_dir: env_path("XDG_RUNTIME_DIR"),
            uid: current_uid()?,
        })
    }

    pub fn from_parts(
        home: PathBuf,
        xdg_config_home: Option<PathBuf>,
        xdg_data_home: Option<PathBuf>,
        xdg_runtime_dir: Option<PathBuf>,
        uid: u32,
    ) -> Self {
        Self {
            home,
            xdg_config_home,
            xdg_data_home,
            xdg_runtime_dir,
            uid,
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn config_dir(&self) -> PathBuf {
        match &self.xdg_config_home {
            Some(root) => root.join(APP_NAME),
            None => self.home.join(".config").join(APP_NAME),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }

    pub fn data_dir(&self) -> PathBuf {
        match &self.xdg_data_home {
            Some(root) => root.join(APP_NAME),
            None => self.home.join(".local").join("share").join(APP_NAME),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join("voxtype-meeting-service.db")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir().join("sessions")
    }

    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(session_id)
    }

    pub fn models_dir(&self) -> PathBuf {
        self.data_dir().join("models")
    }

    pub fn runtime_dir(&self) -> Result<PathBuf> {
        if let Some(root) = &self.xdg_runtime_dir {
            return Ok(root.join(APP_NAME));
        }

        let user_runtime = PathBuf::from("/run/user").join(self.uid.to_string());
        if user_runtime.is_dir() {
            return Ok(user_runtime.join(APP_NAME));
        }

        Err(AppError::path(format!(
            "XDG_RUNTIME_DIR is unset and {} does not exist; cannot create the control socket directory",
            user_runtime.display()
        )))
    }

    pub fn control_socket(&self) -> Result<PathBuf> {
        Ok(self.runtime_dir()?.join("voxtype-meeting-service.sock"))
    }
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    ensure_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key).and_then(|value| {
        if value.is_empty() {
            None
        } else {
            Some(PathBuf::from(value))
        }
    })
}

fn current_uid() -> Result<u32> {
    let status = fs::read_to_string("/proc/self/status").map_err(|error| {
        AppError::path(format!(
            "could not read /proc/self/status to determine uid: {error}"
        ))
    })?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            if let Some(uid) = rest.split_whitespace().next() {
                return uid.parse().map_err(|_| {
                    AppError::path(format!("could not parse uid from /proc/self/status: {uid}"))
                });
            }
        }
    }
    Err(AppError::path(
        "could not determine current uid from /proc/self/status",
    ))
}

#[cfg(test)]
mod tests {
    use super::PathResolver;
    use std::path::PathBuf;

    fn resolver(
        home: &str,
        config: Option<&str>,
        data: Option<&str>,
        runtime: Option<&str>,
    ) -> PathResolver {
        PathResolver::from_parts(
            PathBuf::from(home),
            config.map(PathBuf::from),
            data.map(PathBuf::from),
            runtime.map(PathBuf::from),
            1000,
        )
    }

    #[test]
    fn xdg_data_home() {
        let paths = resolver("/home/xa", None, Some("/custom/data"), None);
        assert_eq!(
            paths.data_dir(),
            PathBuf::from("/custom/data/voxtype-meeting-service")
        );
        assert_eq!(
            paths.db_path(),
            PathBuf::from("/custom/data/voxtype-meeting-service/voxtype-meeting-service.db")
        );
    }

    #[test]
    fn xdg_data_default() {
        let paths = resolver("/home/xa", None, None, None);
        assert_eq!(
            paths.data_dir(),
            PathBuf::from("/home/xa/.local/share/voxtype-meeting-service")
        );
        assert_eq!(
            paths.sessions_dir(),
            PathBuf::from("/home/xa/.local/share/voxtype-meeting-service/sessions")
        );
        assert_eq!(
            paths.session_dir("90e351d4-24cc-4ca1-bfd7-87d36aa9b021"),
            PathBuf::from(
                "/home/xa/.local/share/voxtype-meeting-service/sessions/90e351d4-24cc-4ca1-bfd7-87d36aa9b021"
            )
        );
        assert_eq!(
            paths.models_dir(),
            PathBuf::from("/home/xa/.local/share/voxtype-meeting-service/models")
        );
    }

    #[test]
    fn xdg_config_default() {
        let paths = resolver("/home/xa", None, None, None);
        assert_eq!(
            paths.config_dir(),
            PathBuf::from("/home/xa/.config/voxtype-meeting-service")
        );
        assert_eq!(
            paths.config_file(),
            PathBuf::from("/home/xa/.config/voxtype-meeting-service/config.toml")
        );
    }

    #[test]
    fn xdg_config_home() {
        let paths = resolver("/home/xa", Some("/custom/config"), None, None);
        assert_eq!(
            paths.config_file(),
            PathBuf::from("/custom/config/voxtype-meeting-service/config.toml")
        );
    }

    #[test]
    fn runtime_dir_from_xdg() {
        let paths = resolver("/home/xa", None, None, Some("/run/user/1000"));
        assert_eq!(
            paths.runtime_dir().expect("runtime"),
            PathBuf::from("/run/user/1000/voxtype-meeting-service")
        );
        assert_eq!(
            paths.control_socket().expect("socket"),
            PathBuf::from("/run/user/1000/voxtype-meeting-service/voxtype-meeting-service.sock")
        );
    }
}
