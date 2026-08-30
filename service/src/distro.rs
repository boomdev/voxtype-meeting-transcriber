use std::path::Path;

use crate::error::{AppError, Result};

const DEFAULT_OS_RELEASE: &str = "/etc/os-release";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxDistribution {
    LinuxMint,
    Ubuntu,
    Debian,
    Arch,
    Unknown,
}

impl LinuxDistribution {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LinuxMint => "linuxmint",
            Self::Ubuntu => "ubuntu",
            Self::Debian => "debian",
            Self::Arch => "arch",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DistroInfo {
    pub distribution: LinuxDistribution,
    pub pretty_name: Option<String>,
    pub id: Option<String>,
    pub id_like: Option<String>,
}

impl DistroInfo {
    pub fn detect() -> Result<Self> {
        Self::from_path(DEFAULT_OS_RELEASE)
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path).map_err(|error| {
            AppError::distro(format!("could not read {}: {error}", path.display()))
        })?;
        Ok(Self::from_os_release(&contents))
    }

    pub fn from_os_release(contents: &str) -> Self {
        let mut id = None;
        let mut id_like = None;
        let mut pretty_name = None;

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let value = unquote(value.trim());
                match key {
                    "ID" => id = Some(value),
                    "ID_LIKE" => id_like = Some(value),
                    "PRETTY_NAME" => pretty_name = Some(value),
                    _ => {}
                }
            }
        }

        let distribution = match id.as_deref() {
            Some("linuxmint") => LinuxDistribution::LinuxMint,
            Some("ubuntu") => LinuxDistribution::Ubuntu,
            Some("debian") => LinuxDistribution::Debian,
            Some("arch") => LinuxDistribution::Arch,
            _ => distribution_from_id_like(id_like.as_deref()),
        };

        Self {
            distribution,
            pretty_name,
            id,
            id_like,
        }
    }

    pub fn pretty_name(&self) -> &str {
        self.pretty_name
            .as_deref()
            .unwrap_or(self.distribution.as_str())
    }
}

fn distribution_from_id_like(id_like: Option<&str>) -> LinuxDistribution {
    let Some(id_like) = id_like else {
        return LinuxDistribution::Unknown;
    };
    for token in id_like.split_whitespace() {
        match token {
            "ubuntu" => return LinuxDistribution::Ubuntu,
            "debian" => return LinuxDistribution::Debian,
            "arch" => return LinuxDistribution::Arch,
            _ => {}
        }
    }
    LinuxDistribution::Unknown
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::{DistroInfo, LinuxDistribution};

    #[test]
    fn linuxmint() {
        let info = DistroInfo::from_os_release(
            r#"
NAME="Linux Mint"
ID=linuxmint
ID_LIKE="ubuntu debian"
PRETTY_NAME="Linux Mint 22.3"
"#,
        );
        assert_eq!(info.distribution, LinuxDistribution::LinuxMint);
        assert_eq!(info.pretty_name(), "Linux Mint 22.3");
    }

    #[test]
    fn id_like_arch() {
        let info = DistroInfo::from_os_release(
            r#"
ID=manjaro
ID_LIKE=arch
PRETTY_NAME="Manjaro Linux"
"#,
        );
        assert_eq!(info.distribution, LinuxDistribution::Arch);
    }

    #[test]
    fn unknown() {
        let info = DistroInfo::from_os_release("this is not os-release");
        assert_eq!(info.distribution, LinuxDistribution::Unknown);
        let empty = DistroInfo::from_os_release("");
        assert_eq!(empty.distribution, LinuxDistribution::Unknown);
    }

    #[test]
    fn ubuntu_id() {
        let info = DistroInfo::from_os_release("ID=ubuntu\nPRETTY_NAME=\"Ubuntu 24.04\"\n");
        assert_eq!(info.distribution, LinuxDistribution::Ubuntu);
    }
}
