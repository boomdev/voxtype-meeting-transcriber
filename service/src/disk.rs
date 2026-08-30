use std::path::Path;

use crate::error::{AppError, Result};

pub fn free_bytes(path: &Path) -> Result<u64> {
    let probe = if path.exists() {
        path
    } else {
        path.parent()
            .filter(|parent| parent.exists())
            .unwrap_or(path)
    };
    let stat = rustix::fs::statvfs(probe).map_err(|error| {
        AppError::other(format!(
            "could not read free disk space for {}: {error}",
            probe.display()
        ))
    })?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

pub fn capture_allowed(free_bytes: u64, minimum_free_space_mb: u64) -> bool {
    free_bytes >= minimum_free_space_mb.saturating_mul(1024 * 1024)
}

pub fn assert_enough_space(path: &Path, minimum_free_space_mb: u64) -> Result<()> {
    let free = free_bytes(path)?;
    if capture_allowed(free, minimum_free_space_mb) {
        return Ok(());
    }
    Err(disk_stop_error(free, path, minimum_free_space_mb))
}

pub fn disk_stop_error(free_bytes: u64, path: &Path, minimum_free_space_mb: u64) -> AppError {
    let free_mb = free_bytes / (1024 * 1024);
    AppError::other(format!(
        "Capture stopped: only {free_mb} MB free on {}, minimum_free_space_mb is {minimum_free_space_mb}",
        path.display()
    ))
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

pub fn stored_audio_bytes(sessions_dir: &Path) -> Result<u64> {
    if !sessions_dir.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut stack = vec![sessions_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "flac") {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::{capture_allowed, format_bytes};

    #[test]
    fn threshold_logic() {
        let mb = 1024u64 * 1024;
        assert!(capture_allowed(2000 * mb, 1024));
        assert!(capture_allowed(1024 * mb, 1024));
        assert!(!capture_allowed(500 * mb, 1024));
        assert!(!capture_allowed(0, 1024));
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(512), "512 B");
        assert!(format_bytes(2 * 1024 * 1024).contains("MB"));
    }
}
