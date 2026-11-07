//! JVM process construction and supervision.
//!
//! Minecraft's stdout/stderr are redirected straight into a per-launch log file
//! instead of a pipe. This keeps the game alive when the launcher window is
//! closed (the configured "close on start" behaviour) and avoids a broken pipe
//! killing the JVM. The UI tails the same file to display recent lines.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use pml_minecraft::LaunchPlan;
use thiserror::Error;
use tokio::process::{Child, Command};

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub java_exe: PathBuf,
    pub plan: LaunchPlan,
    pub logs_dir: PathBuf,
}

#[derive(Debug)]
pub struct RunningGame {
    pub child: Child,
    pub pid: Option<u32>,
    pub log_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("ошибка ввода-вывода {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("не удалось запустить JVM {path}: {source}")]
    Spawn {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl RunningGame {
    pub async fn wait(mut self) -> Result<ExitStatus, LaunchError> {
        self.child.wait().await.map_err(|source| LaunchError::Io {
            path: self.log_path.clone(),
            source,
        })
    }

    pub async fn kill(&mut self) -> Result<(), LaunchError> {
        self.child.kill().await.map_err(|source| LaunchError::Io {
            path: self.log_path.clone(),
            source,
        })
    }
}

/// Pure command construction, separated so argument ordering is unit-testable.
pub fn command_line(spec: &LaunchSpec) -> Vec<String> {
    let mut args = Vec::with_capacity(spec.plan.jvm_args.len() + spec.plan.game_args.len() + 2);
    args.extend(spec.plan.jvm_args.iter().cloned());
    args.push(spec.plan.main_class.clone());
    args.extend(spec.plan.game_args.iter().cloned());
    args
}

pub fn spawn_game(spec: &LaunchSpec) -> Result<RunningGame, LaunchError> {
    std::fs::create_dir_all(&spec.logs_dir).map_err(|source| LaunchError::Io {
        path: spec.logs_dir.clone(),
        source,
    })?;
    let log_path = spec.logs_dir.join(log_file_name());
    let log = File::create(&log_path).map_err(|source| LaunchError::Io {
        path: log_path.clone(),
        source,
    })?;
    let stderr = log.try_clone().map_err(|source| LaunchError::Io {
        path: log_path.clone(),
        source,
    })?;

    let mut command = Command::new(&spec.java_exe);
    command
        .args(command_line(spec))
        .current_dir(&spec.plan.working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    hide_console(&mut command);

    let child = command.spawn().map_err(|source| LaunchError::Spawn {
        path: spec.java_exe.clone(),
        source,
    })?;
    let pid = child.id();
    tracing::info!(
        pid = pid.unwrap_or(0),
        java = %spec.java_exe.display(),
        main = %spec.plan.main_class,
        log = %log_path.display(),
        "Minecraft process started"
    );
    Ok(RunningGame {
        child,
        pid,
        log_path,
    })
}

fn log_file_name() -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("game-{stamp}.log")
}

/// Returns the last `max_lines` lines without loading a multi-megabyte log.
pub fn tail_log(path: &Path, max_lines: usize) -> Vec<String> {
    const WINDOW: u64 = 96 * 1024;
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return Vec::new();
    };
    let start = length.saturating_sub(WINDOW);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buffer = Vec::new();
    if file.read_to_end(&mut buffer).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut lines = text
        .lines()
        .map(|line| line.trim_end_matches('\r').to_owned())
        .collect::<Vec<_>>();
    if start > 0 && !lines.is_empty() {
        // The first line is probably cut in the middle of a UTF-8 sequence or
        // an ANSI escape; drop it rather than showing garbage.
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(max_lines);
    lines.into_iter().skip(skip).collect()
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> LaunchPlan {
        LaunchPlan {
            version_id: "1.21.1".to_owned(),
            main_class: "net.minecraft.client.main.Main".to_owned(),
            classpath: vec![PathBuf::from("client.jar")],
            jvm_args: vec![
                "-Xmx4096M".to_owned(),
                "-cp".to_owned(),
                "client.jar".to_owned(),
            ],
            game_args: vec!["--server".to_owned(), "45.85.60.39".to_owned()],
            working_dir: PathBuf::from("."),
            natives_dir: PathBuf::from("natives"),
            assets_dir: PathBuf::from("assets"),
            instance_dir: PathBuf::from("."),
        }
    }

    #[test]
    fn command_line_puts_main_class_between_jvm_and_game_args() {
        let spec = LaunchSpec {
            java_exe: PathBuf::from("java.exe"),
            plan: plan(),
            logs_dir: PathBuf::from("logs"),
        };
        assert_eq!(
            command_line(&spec),
            vec![
                "-Xmx4096M",
                "-cp",
                "client.jar",
                "net.minecraft.client.main.Main",
                "--server",
                "45.85.60.39"
            ]
        );
    }

    #[test]
    fn tail_reads_last_lines() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("game.log");
        let content = (0..100)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, content).expect("write");
        let lines = tail_log(&path, 5);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines.first().map(String::as_str), Some("line 95"));
        assert_eq!(lines.last().map(String::as_str), Some("line 99"));
    }

    #[test]
    fn tail_of_missing_file_is_empty() {
        assert!(tail_log(Path::new("does-not-exist.log"), 10).is_empty());
    }
}
