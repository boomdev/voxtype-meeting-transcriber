use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::audio::{AudioBackend, PulseAudioBackend};
use crate::config::{openai_api_key, Config};
use crate::distro::{DistroInfo, LinuxDistribution};
use crate::error::Result;
use crate::paths::PathResolver;
use crate::storage::Db;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

struct Check {
    name: &'static str,
    status: CheckStatus,
    detail: String,
}

pub fn cmd_doctor() -> Result<()> {
    let paths = PathResolver::from_env()?;
    let distro = DistroInfo::detect().unwrap_or_else(|_| DistroInfo::from_os_release(""));
    let checks = collect_checks(&paths, &distro);

    println!("Distribution: {}", distro.pretty_name());
    let mut failed = false;
    for check in &checks {
        println!(
            "{:<28} {:>4}  {}",
            check.name,
            check.status.label(),
            check.detail
        );
        if check.status == CheckStatus::Fail {
            failed = true;
        }
    }
    if failed {
        return Err(crate::error::AppError::other(
            "doctor found one or more FAIL checks",
        ));
    }
    Ok(())
}

pub fn diagnostics_payload(paths: &PathResolver) -> Vec<serde_json::Value> {
    let distro = DistroInfo::detect().unwrap_or_else(|_| DistroInfo::from_os_release(""));
    collect_checks(paths, &distro)
        .into_iter()
        .map(|check| {
            serde_json::json!({
                "id": check_id(check.name),
                "name": check.name,
                "status": check.status.label(),
                "detail": check.detail,
            })
        })
        .collect()
}

fn collect_checks(paths: &PathResolver, distro: &DistroInfo) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(check_distro(distro));
    let (config, config_check) = check_config(paths);
    checks.push(config_check);

    match PulseAudioBackend::connect() {
        Ok(backend) => {
            checks.push(Check {
                name: "PulseAudio connectivity",
                status: CheckStatus::Pass,
                detail: "context ready".into(),
            });
            checks.push(check_server_identity(&backend));
            checks.push(check_device(
                "Default microphone",
                backend.current_microphone(),
            ));
            checks.push(check_device(
                "Default output sink",
                backend.current_output_sink(),
            ));
            checks.push(check_device(
                "Default output monitor",
                backend.current_output_monitor(),
            ));
        }
        Err(error) => {
            checks.push(Check {
                name: "PulseAudio connectivity",
                status: CheckStatus::Fail,
                detail: error.to_string(),
            });
            checks.push(Check {
                name: "Audio server identity",
                status: CheckStatus::Warn,
                detail: "undetectable because the audio server is not connected".into(),
            });
            checks.push(missing_device("Default microphone"));
            checks.push(missing_device("Default output sink"));
            checks.push(missing_device("Default output monitor"));
        }
    }
    checks.push(check_data_dir(paths));
    checks.push(check_database(paths));
    checks.push(check_disk(paths, config.as_ref()));
    checks.push(check_systemd());
    checks.push(check_openai(config.as_ref()));
    checks.push(check_whisper(config.as_ref()));
    checks.push(check_whisper_model(config.as_ref()));
    checks.push(check_ui_runtime());
    checks
}

fn check_id(name: &str) -> &'static str {
    match name {
        "PulseAudio connectivity" => "audio_server",
        "Default microphone" => "microphone",
        "Default output sink" => "system_output",
        "Default output monitor" => "system_monitor",
        "OpenAI API key" => "openai",
        "whisper-cli" => "whisper_cpp",
        "whisper model" => "local_model",
        "Database" => "database",
        "Data directory" => "data_directory",
        "Free disk space" => "free_disk",
        "Desktop UI runtime" => "ui_runtime",
        "systemd user session" => "systemd",
        "Linux distribution" => "distribution",
        "Configuration" => "configuration",
        "Audio server identity" => "audio_server_identity",
        _ => "other",
    }
}

fn check_ui_runtime() -> Check {
    match std::process::Command::new("python3")
        .args([
            "-c",
            "import gi; gi.require_version('Gtk', '3.0'); gi.require_version('XApp', '1.0'); from gi.repository import Gtk, XApp, Gio, GLib",
        ])
        .output()
    {
        Ok(output) if output.status.success() => Check {
            name: "Desktop UI runtime",
            status: CheckStatus::Pass,
            detail: "python3, GTK 3, and XApp are available".into(),
        },
        Ok(output) => Check {
            name: "Desktop UI runtime",
            status: CheckStatus::Warn,
            detail: format!(
                "GTK 3 / XApp import failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        },
        Err(error) => Check {
            name: "Desktop UI runtime",
            status: CheckStatus::Warn,
            detail: format!("python3 is not available: {error}"),
        },
    }
}

fn check_distro(distro: &DistroInfo) -> Check {
    match distro.distribution {
        LinuxDistribution::LinuxMint => Check {
            name: "Linux distribution",
            status: CheckStatus::Pass,
            detail: distro.pretty_name().to_string(),
        },
        LinuxDistribution::Ubuntu | LinuxDistribution::Debian => Check {
            name: "Linux distribution",
            status: CheckStatus::Warn,
            detail: format!(
                "{} is not the V1 supported distro (Linux Mint)",
                distro.pretty_name()
            ),
        },
        LinuxDistribution::Arch => Check {
            name: "Linux distribution",
            status: CheckStatus::Warn,
            detail: format!(
                "{} is a future target; V1 is Linux Mint only",
                distro.pretty_name()
            ),
        },
        LinuxDistribution::Unknown => Check {
            name: "Linux distribution",
            status: CheckStatus::Warn,
            detail: format!("unknown distribution ({})", distro.pretty_name()),
        },
    }
}

fn check_config(paths: &PathResolver) -> (Option<Config>, Check) {
    let file = paths.config_file();
    if !file.exists() {
        return (
            Some(Config::default()),
            Check {
                name: "Configuration",
                status: CheckStatus::Warn,
                detail: "config.toml missing; using built-in defaults".into(),
            },
        );
    }
    match Config::load(paths) {
        Ok(config) => (
            Some(config),
            Check {
                name: "Configuration",
                status: CheckStatus::Pass,
                detail: file.display().to_string(),
            },
        ),
        Err(error) => (
            None,
            Check {
                name: "Configuration",
                status: CheckStatus::Fail,
                detail: error.to_string(),
            },
        ),
    }
}

fn check_server_identity(backend: &dyn AudioBackend) -> Check {
    match backend.server_identity() {
        Ok(identity) => Check {
            name: "Audio server identity",
            status: CheckStatus::Pass,
            detail: identity,
        },
        Err(error) => Check {
            name: "Audio server identity",
            status: CheckStatus::Warn,
            detail: format!("undetectable identity ({error})"),
        },
    }
}

fn check_device(
    name: &'static str,
    result: crate::error::Result<crate::audio::AudioDevice>,
) -> Check {
    match result {
        Ok(device) => Check {
            name,
            status: CheckStatus::Pass,
            detail: device.summary(),
        },
        Err(error) => Check {
            name,
            status: CheckStatus::Fail,
            detail: error.to_string(),
        },
    }
}

fn missing_device(name: &'static str) -> Check {
    Check {
        name,
        status: CheckStatus::Fail,
        detail: "missing".into(),
    }
}

fn check_data_dir(paths: &PathResolver) -> Check {
    let dir = paths.data_dir();
    match probe_writable(&dir) {
        Ok(()) => Check {
            name: "Data directory",
            status: CheckStatus::Pass,
            detail: format!("writable {}", dir.display()),
        },
        Err(error) => Check {
            name: "Data directory",
            status: CheckStatus::Fail,
            detail: error,
        },
    }
}

fn probe_writable(dir: &Path) -> std::result::Result<(), String> {
    crate::paths::ensure_dir(dir).map_err(|error| error.to_string())?;
    let probe = dir.join(".doctor-write-probe");
    match std::fs::File::create(&probe).and_then(|mut file| file.write_all(b"ok")) {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(error) => Err(format!("{} is not writable: {error}", dir.display())),
    }
}

fn check_database(paths: &PathResolver) -> Check {
    match Db::open(paths.db_path()) {
        Ok(db) => match db.with_conn(|conn| {
            conn.execute("CREATE TEMP TABLE doctor_probe (id INTEGER)", [])?;
            conn.execute("DROP TABLE doctor_probe", [])?;
            Ok(())
        }) {
            Ok(()) => Check {
                name: "Database",
                status: CheckStatus::Pass,
                detail: format!("writable {}", paths.db_path().display()),
            },
            Err(error) => Check {
                name: "Database",
                status: CheckStatus::Fail,
                detail: error.to_string(),
            },
        },
        Err(error) => Check {
            name: "Database",
            status: CheckStatus::Fail,
            detail: error.to_string(),
        },
    }
}

fn check_disk(paths: &PathResolver, config: Option<&Config>) -> Check {
    let minimum = config
        .map(|config| config.general.minimum_free_space_mb)
        .unwrap_or(1024);
    match crate::disk::free_bytes(&paths.data_dir()) {
        Ok(free) => {
            let free_mb = free / (1024 * 1024);
            if free_mb < minimum {
                Check {
                    name: "Free disk space",
                    status: CheckStatus::Fail,
                    detail: format!("{free_mb} MB free; minimum_free_space_mb is {minimum}"),
                }
            } else if free_mb < minimum.saturating_mul(2) {
                Check {
                    name: "Free disk space",
                    status: CheckStatus::Warn,
                    detail: format!("{free_mb} MB free; below 2× threshold {minimum} MB"),
                }
            } else {
                Check {
                    name: "Free disk space",
                    status: CheckStatus::Pass,
                    detail: format!("{free_mb} MB free (threshold {minimum} MB)"),
                }
            }
        }
        Err(error) => Check {
            name: "Free disk space",
            status: CheckStatus::Fail,
            detail: error.to_string(),
        },
    }
}

fn check_systemd() -> Check {
    match crate::service::systemctl_user(&["status", "voxtype-meeting-service.service"]) {
        Ok(_) => Check {
            name: "systemd user session",
            status: CheckStatus::Pass,
            detail: "systemctl --user is available".into(),
        },
        Err(error) => Check {
            name: "systemd user session",
            status: CheckStatus::Warn,
            detail: error.to_string(),
        },
    }
}

fn check_openai(config: Option<&Config>) -> Check {
    let provider = config.map(|config| config.transcription.provider);
    match provider {
        Some(crate::config::ProviderKind::Openai) | None => {
            if openai_api_key().is_some() {
                Check {
                    name: "OpenAI API key",
                    status: CheckStatus::Pass,
                    detail: "OPENAI_API_KEY is set".into(),
                }
            } else {
                Check {
                    name: "OpenAI API key",
                    status: CheckStatus::Warn,
                    detail:
                        "OPENAI_API_KEY is missing; capture still works, OpenAI jobs stay pending"
                            .into(),
                }
            }
        }
        Some(_) => Check {
            name: "OpenAI API key",
            status: CheckStatus::Pass,
            detail: "not required for whisper_cpp".into(),
        },
    }
}

fn check_whisper(config: Option<&Config>) -> Check {
    let Some(config) = config else {
        return Check {
            name: "whisper-cli",
            status: CheckStatus::Warn,
            detail: "configuration unavailable".into(),
        };
    };
    if config.transcription.provider != crate::config::ProviderKind::WhisperCpp {
        return Check {
            name: "whisper-cli",
            status: CheckStatus::Pass,
            detail: "N/A (provider is openai)".into(),
        };
    }
    let path = &config.transcription.whisper_cpp.executable;
    if !path.exists() {
        return Check {
            name: "whisper-cli",
            status: CheckStatus::Warn,
            detail: format!("not found at {}", path.display()),
        };
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.permissions().mode() & 0o111 != 0 => Check {
            name: "whisper-cli",
            status: CheckStatus::Pass,
            detail: path.display().to_string(),
        },
        Ok(_) => Check {
            name: "whisper-cli",
            status: CheckStatus::Fail,
            detail: format!("{} exists but is not executable", path.display()),
        },
        Err(error) => Check {
            name: "whisper-cli",
            status: CheckStatus::Fail,
            detail: error.to_string(),
        },
    }
}

fn check_whisper_model(config: Option<&Config>) -> Check {
    let Some(config) = config else {
        return Check {
            name: "whisper model",
            status: CheckStatus::Warn,
            detail: "configuration unavailable".into(),
        };
    };
    if config.transcription.provider != crate::config::ProviderKind::WhisperCpp {
        return Check {
            name: "whisper model",
            status: CheckStatus::Pass,
            detail: "N/A (provider is openai)".into(),
        };
    }
    let path = &config.transcription.whisper_cpp.model;
    if path.is_file() {
        Check {
            name: "whisper model",
            status: CheckStatus::Pass,
            detail: path.display().to_string(),
        }
    } else {
        Check {
            name: "whisper model",
            status: CheckStatus::Warn,
            detail: format!("missing at {}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{check_config, check_distro, CheckStatus};
    use crate::distro::DistroInfo;
    use crate::paths::PathResolver;
    use tempfile::tempdir;

    fn test_paths(root: &std::path::Path) -> PathResolver {
        PathResolver::from_parts(
            root.to_path_buf(),
            Some(root.join("config")),
            Some(root.join("data")),
            Some(root.join("runtime")),
            1000,
        )
    }

    #[test]
    fn distro_mint_passes() {
        let info = DistroInfo::from_os_release("ID=linuxmint\nPRETTY_NAME=\"Linux Mint 22.3\"\n");
        assert_eq!(check_distro(&info).status, CheckStatus::Pass);
    }

    #[test]
    fn invalid_config_fails() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        crate::paths::ensure_dir(&paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), "[audio]\nsource = \"invalid\"\n").unwrap();
        let (_config, check) = check_config(&paths);
        assert_eq!(check.status, CheckStatus::Fail);
    }

    #[test]
    fn missing_config_warns() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        let (_config, check) = check_config(&paths);
        assert_eq!(check.status, CheckStatus::Warn);
    }
}
