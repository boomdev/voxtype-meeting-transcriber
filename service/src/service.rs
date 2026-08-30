use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use crate::error::{AppError, Result};
use crate::paths::PathResolver;
use crate::runtime::STOP_WAIT_SECS;

const UNIT: &str = "voxtype-meeting-service.service";

pub fn systemctl_user(args: &[&str]) -> Result<Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|error| {
            AppError::other(format!(
                "could not run systemctl --user {}: {error}",
                args.join(" ")
            ))
        })
}

pub fn unit_is_active() -> Option<bool> {
    let output = systemctl_user(&["is-active", UNIT]).ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    Some(text.trim() == "active")
}

pub fn unit_exists() -> bool {
    systemctl_user(&["cat", UNIT])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub fn cmd_start() -> Result<()> {
    if unit_is_active() == Some(true) {
        println!("Service already running.");
        return Ok(());
    }
    if !unit_exists() {
        return Err(AppError::other(format!(
            "systemd user unit {UNIT} is not installed. Copy systemd/voxtype-meeting-service.service to ~/.config/systemd/user/{UNIT} and run: systemctl --user daemon-reload"
        )));
    }
    let output = systemctl_user(&["start", UNIT])?;
    if output.status.success() {
        println!("Started {UNIT}.");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(AppError::other(format!(
            "systemctl --user start {UNIT} failed: {}",
            stderr.trim()
        )))
    }
}

pub async fn cmd_stop(paths: &PathResolver) -> Result<()> {
    let socket = paths.control_socket().ok();
    let socket_live = match &socket {
        Some(path) if path.exists() => crate::control::send_request(path, "stop").await.ok(),
        _ => None,
    };
    if socket_live.is_some() {
        wait_socket_gone(socket.as_ref()).await;
        println!("Stopped voxtype-meeting-service.");
        return Ok(());
    }
    if socket.as_ref().is_some_and(|path| path.exists()) {
        eprintln!("Control socket was unavailable; falling back to systemctl --user stop.");
    }
    if unit_is_active() == Some(true) {
        let output = systemctl_user(&["stop", UNIT])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppError::other(format!(
                "systemctl --user stop {UNIT} failed: {}",
                stderr.trim()
            )));
        }
        wait_socket_gone(socket.as_ref()).await;
        println!("Stopped voxtype-meeting-service.");
        return Ok(());
    }
    println!("Service is not running.");
    Ok(())
}

async fn wait_socket_gone(path: Option<&PathBuf>) {
    let Some(path) = path else {
        return;
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(STOP_WAIT_SECS);
    while tokio::time::Instant::now() < deadline {
        if !path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn cmd_status(paths: &PathResolver) -> Result<()> {
    let systemd_active = unit_is_active();
    let socket = paths.control_socket().ok();
    let payload = match &socket {
        Some(path) if path.exists() => match crate::control::send_request(path, "status").await {
            Ok(crate::control::protocol::Response::OkStatus { status, .. }) => Some(*status),
            Ok(crate::control::protocol::Response::Err { error, .. }) => {
                return Err(AppError::control(error));
            }
            Ok(_) => None,
            Err(_) => None,
        },
        _ => None,
    };
    let service = if payload.is_some() || systemd_active == Some(true) {
        "running"
    } else {
        "stopped"
    };
    print!(
        "{}",
        crate::control::format_status_text(service, payload.as_ref())
    );
    Ok(())
}
