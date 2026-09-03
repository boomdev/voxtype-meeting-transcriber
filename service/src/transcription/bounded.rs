use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::time::timeout;

use crate::error::{AppError, Result};

pub const AUDIO_MAX_BYTES: u64 = 32 * 1024 * 1024;
pub const PROCESS_OUTPUT_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const JSON_MAX_BYTES: usize = 4 * 1024 * 1024;
pub const TRANSCRIPT_MAX_BYTES: usize = 1024 * 1024;
pub const CONFIG_MAX_BYTES: usize = 1024 * 1024;
pub const MEETING_LANGUAGE_MAX_BYTES: usize = 64;

enum ReadOutcome {
    Bytes(Vec<u8>),
    Overflow,
}

#[derive(Debug)]
pub struct CappedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub fn check_audio_file_size(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path).map_err(|error| {
        AppError::transcription(format!(
            "could not inspect audio chunk {}: {error}",
            path.display()
        ))
    })?;
    if !meta.is_file() {
        return Err(AppError::transcription(format!(
            "audio chunk {} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > AUDIO_MAX_BYTES {
        return Err(AppError::transcription(format!(
            "audio chunk {} exceeds the {} byte limit",
            path.display(),
            AUDIO_MAX_BYTES
        )));
    }
    Ok(())
}

pub fn limit_transcript(text: String) -> Result<String> {
    if text.len() > TRANSCRIPT_MAX_BYTES {
        return Err(AppError::transcription(
            "transcript exceeds the configured byte limit",
        ));
    }
    Ok(text)
}

pub fn read_capped_bytes_sync(path: &Path, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            return Ok(buf);
        }
        if buf.len().saturating_add(n) > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("file exceeds the {max_bytes} byte limit"),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

pub async fn read_capped_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    let meta = tokio::fs::metadata(path).await.map_err(|error| {
        AppError::transcription(format!("could not inspect {}: {error}", path.display()))
    })?;
    if meta.len() > max_bytes as u64 {
        return Err(AppError::transcription(format!(
            "{} exceeds the {} byte limit",
            path.display(),
            max_bytes
        )));
    }
    let mut file = tokio::fs::File::open(path).await.map_err(|error| {
        AppError::transcription(format!("could not read {}: {error}", path.display()))
    })?;
    match read_capped(&mut file, max_bytes).await {
        Ok(ReadOutcome::Bytes(buf)) => Ok(buf),
        Ok(ReadOutcome::Overflow) => Err(AppError::transcription(format!(
            "{} exceeds the {} byte limit",
            path.display(),
            max_bytes
        ))),
        Err(error) => Err(AppError::transcription(format!(
            "could not read {}: {error}",
            path.display()
        ))),
    }
}

pub async fn run_capped_command(mut command: Command, limit: Duration) -> Result<CappedOutput> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn().map_err(|error| {
        AppError::transcription(format!("failed to start transcription process: {error}"))
    })?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(async move {
        match stdout {
            Some(reader) => read_capped(reader, PROCESS_OUTPUT_MAX_BYTES).await,
            None => Ok(ReadOutcome::Bytes(Vec::new())),
        }
    });
    let stderr_task = tokio::spawn(async move {
        match stderr {
            Some(reader) => read_capped(reader, PROCESS_OUTPUT_MAX_BYTES).await,
            None => Ok(ReadOutcome::Bytes(Vec::new())),
        }
    });
    match timeout(limit, child.wait()).await {
        Err(_) => {
            reap_group(&mut child, pid).await;
            stdout_task.abort();
            stderr_task.abort();
            Err(AppError::transcription(
                "transcription process timed out".to_string(),
            ))
        }
        Ok(Err(error)) => {
            reap_group(&mut child, pid).await;
            stdout_task.abort();
            stderr_task.abort();
            Err(AppError::transcription(format!(
                "transcription process failed: {error}"
            )))
        }
        Ok(Ok(status)) => {
            let stdout = join_output(stdout_task, &mut child, pid).await?;
            let stderr = join_output(stderr_task, &mut child, pid).await?;
            Ok(CappedOutput {
                status,
                stdout,
                stderr,
            })
        }
    }
}

async fn join_output(
    task: tokio::task::JoinHandle<std::io::Result<ReadOutcome>>,
    child: &mut Child,
    pid: Option<u32>,
) -> Result<Vec<u8>> {
    match task.await {
        Ok(Ok(ReadOutcome::Bytes(buf))) => Ok(buf),
        Ok(Ok(ReadOutcome::Overflow)) => {
            reap_group(child, pid).await;
            Err(AppError::transcription(
                "transcription process output exceeds the byte limit",
            ))
        }
        Ok(Err(error)) => Err(AppError::transcription(format!(
            "could not read transcription process output: {error}"
        ))),
        Err(_) => Err(AppError::transcription(
            "transcription process output reader failed",
        )),
    }
}

async fn read_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    max_bytes: usize,
) -> std::io::Result<ReadOutcome> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(ReadOutcome::Bytes(buf));
        }
        if buf.len().saturating_add(n) > max_bytes {
            return Ok(ReadOutcome::Overflow);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn reap_group(child: &mut Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        kill_group(pid);
    }
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(2), child.wait()).await;
}

fn kill_group(pid: u32) {
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    if raw <= 0 {
        return;
    }
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return;
    };
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::TERM);
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
}

#[cfg(test)]
mod tests {
    use super::{
        check_audio_file_size, limit_transcript, read_capped_bytes_sync, read_capped_file,
        run_capped_command, AUDIO_MAX_BYTES, TRANSCRIPT_MAX_BYTES,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::process::Command;

    #[test]
    fn audio_size_cap_rejects_oversize_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("chunk.flac");
        std::fs::write(&path, vec![0u8; 64]).unwrap();
        check_audio_file_size(&path).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(AUDIO_MAX_BYTES + 1).unwrap();
        let error = check_audio_file_size(&path).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[test]
    fn transcript_cap_rejects_oversize_text() {
        let error = limit_transcript("x".repeat(TRANSCRIPT_MAX_BYTES + 1)).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[tokio::test]
    async fn capped_command_kills_descendants_on_timeout() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("hang.sh");
        std::fs::write(&script, "#!/bin/sh\n/bin/sleep 30 &\nwait\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        let command = Command::new(&script);
        let error = run_capped_command(command, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn capped_command_rejects_runaway_stdout() {
        let mut command = Command::new("sh");
        command.args(["-c", "dd if=/dev/zero bs=1024 count=8192 2>/dev/null"]);
        let error = run_capped_command(command, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[tokio::test]
    async fn capped_file_read_rejects_oversize() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.json");
        std::fs::write(&path, vec![b'x'; 64]).unwrap();
        let error = read_capped_file(&path, 16).await.unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
        let error = read_capped_bytes_sync(&path, 16).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
