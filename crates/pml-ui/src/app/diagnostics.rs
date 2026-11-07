//! One-click diagnostics bundle for support requests.
//!
//! The report contains the portable paths, a redacted-enough config snapshot
//! (there is no token field in `config.json`), the resolved Java, the selected
//! manifest and the tails of the launcher/game logs. It is written next to the
//! other logs so a player can attach one file instead of hunting through
//! `launcher_data`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::util::open_path;
use super::LauncherApp;

impl LauncherApp {
    pub(crate) fn export_diagnostics(&mut self) {
        match self.write_diagnostics() {
            Ok(path) => {
                self.status = format!("Диагностика сохранена: {}", path.display());
                open_path(&self.paths.logs_dir());
            }
            Err(error) => {
                self.error = Some(format!("Не удалось сохранить диагностику: {error}"));
            }
        }
    }

    fn write_diagnostics(&self) -> Result<PathBuf, String> {
        let mut report = String::new();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);

        writeln!(report, "Odium Launcher diagnostics").map_err(|error| error.to_string())?;
        writeln!(report, "version: {}", env!("CARGO_PKG_VERSION"))
            .map_err(|error| error.to_string())?;
        writeln!(report, "generated_unix: {stamp}").map_err(|error| error.to_string())?;
        writeln!(
            report,
            "os: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
        .map_err(|error| error.to_string())?;
        writeln!(
            report,
            "exe: {}",
            std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|error| format!("<unavailable: {error}>"))
        )
        .map_err(|error| error.to_string())?;
        writeln!(report, "root: {}", self.paths.root().display())
            .map_err(|error| error.to_string())?;
        writeln!(report, "config: {}", self.store.path().display())
            .map_err(|error| error.to_string())?;
        writeln!(report, "theme: {:?}", self.theme).map_err(|error| error.to_string())?;
        writeln!(
            report,
            "busy: {}, game_running: {}",
            self.busy, self.game_running
        )
        .map_err(|error| error.to_string())?;
        writeln!(report, "status: {}", self.status).map_err(|error| error.to_string())?;

        writeln!(report, "\n[config.json]").map_err(|error| error.to_string())?;
        match serde_json::to_string_pretty(self.store.config()) {
            Ok(json) => writeln!(report, "{json}").map_err(|error| error.to_string())?,
            Err(error) => writeln!(report, "<serialization failed: {error}>")
                .map_err(|error| error.to_string())?,
        }

        writeln!(report, "\n[selected source]").map_err(|error| error.to_string())?;
        match self.selected_source() {
            Some(source) => {
                writeln!(
                    report,
                    "id: {}\nname: {}\nkind: {:?}\nurl: {}",
                    source.id,
                    source.name,
                    source.kind(),
                    source.manifest_url
                )
                .map_err(|error| error.to_string())?;
            }
            None => writeln!(report, "<none>").map_err(|error| error.to_string())?,
        }

        writeln!(report, "\n[manifest]").map_err(|error| error.to_string())?;
        match &self.manifest {
            Some(manifest) => {
                writeln!(
                    report,
                    "name: {}\nversion: {}\nminecraft: {}\nloader: {} {}\nfiles: {} (+{} optional)\nserver: {:?}",
                    manifest.name,
                    manifest.version,
                    manifest.mc_version,
                    manifest.loader.kind.label(),
                    manifest.loader.version,
                    manifest.files.len(),
                    manifest.optional_files.len(),
                    manifest.server
                )
                .map_err(|error| error.to_string())?;
            }
            None => {
                writeln!(
                    report,
                    "<not loaded>{}",
                    self.manifest_error
                        .as_ref()
                        .map(|error| format!("\nerror: {error}"))
                        .unwrap_or_default()
                )
                .map_err(|error| error.to_string())?;
            }
        }

        writeln!(report, "\n[java]").map_err(|error| error.to_string())?;
        match &self.java {
            Some(java) => {
                writeln!(
                    report,
                    "path: {}\nversion: {}\nmajor: {}\norigin: {}",
                    java.java_exe.display(),
                    java.version,
                    java.major,
                    java.origin.label()
                )
                .map_err(|error| error.to_string())?;
            }
            None => writeln!(report, "<not resolved yet>").map_err(|error| error.to_string())?,
        }

        writeln!(report, "\n[launcher log]").map_err(|error| error.to_string())?;
        match newest_launcher_log(&self.paths.logs_dir()) {
            Some(path) => {
                writeln!(report, "file: {}", path.display()).map_err(|error| error.to_string())?;
                append_tail(&mut report, &path, 200);
            }
            None => {
                writeln!(report, "<no launcher.log yet>").map_err(|error| error.to_string())?;
            }
        }

        writeln!(report, "\n[game log]").map_err(|error| error.to_string())?;
        match &self.log_path {
            Some(path) => {
                writeln!(report, "file: {}", path.display()).map_err(|error| error.to_string())?;
                append_tail(&mut report, path, 200);
            }
            None => {
                writeln!(report, "<game has not started yet>")
                    .map_err(|error| error.to_string())?;
            }
        }

        let logs = self.paths.logs_dir();
        std::fs::create_dir_all(&logs).map_err(|error| error.to_string())?;
        let path = logs.join(format!("diagnostics-{stamp}.txt"));
        std::fs::write(&path, report).map_err(|error| error.to_string())?;
        Ok(path)
    }
}

fn newest_launcher_log(directory: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(directory).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("launcher.log") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if newest.as_ref().is_none_or(|(best, _)| modified > *best) {
            newest = Some((modified, entry.path()));
        }
    }
    newest.map(|(_, path)| path)
}

fn append_tail(report: &mut String, path: &Path, lines: usize) {
    // `tail_log` already caps the read window to 96 KiB; a multi-megabyte log
    // is never loaded into the report.
    let tail = pml_launcher::tail_log(path, lines);
    if tail.is_empty() {
        let _ = writeln!(report, "<empty>");
        return;
    }
    for line in tail {
        let _ = writeln!(report, "{line}");
    }
}
