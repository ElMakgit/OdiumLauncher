//! Complete removal of everything the launcher created.
//!
//! The launcher writes only inside its data root (`%LOCALAPPDATA%\OdiumLauncher`
//! by default, `launcher_data` next to the executable in portable mode), so a
//! full uninstall is a recursive delete of that one directory. Because the
//! operation is destructive and irreversible, the root is validated first: a
//! drive root, the user profile, or a directory that contains the running
//! executable are refused.
//!
//! The executable itself is deleted by a detached shell command a couple of
//! seconds after the process exits, since Windows locks a running image.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use thiserror::Error;

/// Result of a purge attempt.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    /// Files that are gone, including an estimate of the ones that were inside
    /// already removed directories.
    pub removed_files: u64,
    /// Total size of the removed files in bytes.
    pub removed_bytes: u64,
    /// Paths that survived the delete, usually because a process still holds
    /// them open. The caller should tell the user to close the game first.
    pub leftovers: Vec<PathBuf>,
    /// False when the data root did not exist to begin with.
    pub root_existed: bool,
}

#[derive(Debug, Error)]
pub enum UninstallError {
    #[error("отказано: небезопасный путь для удаления: {0}")]
    UnsafePath(PathBuf),
    #[error("не удалось удалить данные: {0}")]
    Failed(String),
}

/// Deletes the whole data root. Locked files are retried a few times; whatever
/// remains is reported in [`PurgeReport::leftovers`] instead of failing the
/// whole operation.
pub fn purge(root: &Path) -> Result<PurgeReport, UninstallError> {
    let canonical = match fs::canonicalize(root) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(PurgeReport::default()),
        Err(error) => {
            return Err(UninstallError::Failed(format!(
                "{}: {error}",
                root.display()
            )))
        }
    };
    if !is_safe_root(&canonical) {
        return Err(UninstallError::UnsafePath(canonical));
    }

    let mut files = 0u64;
    let mut bytes = 0u64;
    walk_stats(&canonical, &mut files, &mut bytes);
    let mut report = PurgeReport {
        root_existed: true,
        removed_files: files,
        removed_bytes: bytes,
        leftovers: Vec::new(),
    };

    // Antivirus scanners and the just-closed game often keep a handle for a
    // fraction of a second; three spaced attempts cover that without hiding a
    // real permission problem.
    let mut last_error = None;
    for attempt in 0..3 {
        match fs::remove_dir_all(&canonical) {
            Ok(()) => {
                report.leftovers.clear();
                return Ok(report);
            }
            Err(error) => {
                last_error = Some(error);
                if attempt < 2 {
                    sleep(Duration::from_millis(250));
                }
            }
        }
    }

    if !canonical.exists() {
        report.leftovers.clear();
        return Ok(report);
    }
    collect_leftovers(&canonical, &mut report.leftovers);
    if report.leftovers.is_empty() {
        if let Some(error) = last_error {
            return Err(UninstallError::Failed(error.to_string()));
        }
    }
    // Counts stay as the pre-delete estimate; the caller only shows "removed N".
    Ok(report)
}

/// Starts a detached process that removes the executable once this process has
/// exited. Only the executable itself is removed, plus its folder if that
/// folder is empty afterwards.
pub fn schedule_exe_removal(exe: &Path) -> io::Result<()> {
    // `current_exe` is already absolute; keep the plain form instead of
    // canonicalizing because cmd handles `C:\...` better than `\\?\C:\...`.
    let exe = if exe.is_absolute() {
        exe.to_path_buf()
    } else {
        std::env::current_dir()?.join(exe)
    };
    let parent = exe.parent().map(Path::to_path_buf);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // Windows keeps the image locked until the process is gone, so wait a
        // moment and then retry the delete a few times. `del` is repeated
        // instead of using a loop with `exit`, because `exit /b` would also
        // skip the final empty-folder cleanup.
        // Each retry is wrapped in `if exist ... (...)` so the whole chain
        // ends as soon as the image is gone; a bare `cmd1 & cmd2` after an
        // `if` would bind cmd2 to that condition and never finish.
        let mut script = String::from("ping 127.0.0.1 -n 3 >NUL");
        for _ in 0..10 {
            script.push_str(&format!(
                " & (if exist \"{exe}\" (ping 127.0.0.1 -n 2 >NUL & del /f /q \"{exe}\" >NUL 2>&1))",
                exe = exe.display()
            ));
        }
        if let Some(parent) = &parent {
            script.push_str(&format!(" & rmdir \"{}\" 2>NUL", parent.display()));
        }
        // `raw_arg` keeps the command line intact: the default argument
        // escaping would turn the embedded quotes into `\"`, which cmd.exe
        // does not understand, and the delete would silently target a
        // non-existent path.
        let mut command = std::process::Command::new("cmd");
        command
            .raw_arg("/C")
            .raw_arg(&script)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
    }

    #[cfg(not(windows))]
    {
        let mut script = format!(
            "sleep 2; for i in 1 2 3 4 5 6 7 8 9 10; do \
             rm -f \"$1\" 2>/dev/null; [ ! -e \"$1\" ] && break; sleep 1; done"
        );
        if parent.is_some() {
            script.push_str("; rmdir \"$2\" 2>/dev/null");
        }
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(script).arg("sh").arg(&exe);
        if let Some(parent) = &parent {
            command.arg(parent);
        }
        command.spawn().map(|_| ())
    }
}

/// Directory names the launcher itself creates. The uninstaller only accepts a
/// root with one of these names: a hand-edited `ODIUM_DATA_DIR` pointing at
/// `Documents` or a drive root can never be wiped by mistake.
const SAFE_ROOT_NAMES: [&str; 2] = ["OdiumLauncher", "launcher_data"];

/// Rejects paths whose recursive removal would wipe something the launcher does
/// not own. The name allowlist is the primary guard; the remaining checks catch
/// a symlinked or relocated folder that happens to carry the right name.
fn is_safe_root(root: &Path) -> bool {
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name.is_empty() || root.parent().is_none() {
        return false;
    }
    if !SAFE_ROOT_NAMES
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
    {
        return false;
    }
    if canonical_eq(root, &std::env::temp_dir()) {
        return false;
    }
    if let Some(home) = home_dir() {
        if canonical_eq(root, &home) {
            return false;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(exe) = fs::canonicalize(exe) {
            if exe.starts_with(root) {
                return false;
            }
        }
    }
    true
}

fn canonical_eq(left: &Path, right: &Path) -> bool {
    match fs::canonicalize(right) {
        Ok(right) => left == right,
        Err(_) => false,
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn walk_stats(path: &Path, files: &mut u64, bytes: &mut u64) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            *files += 1;
            continue;
        }
        if file_type.is_dir() {
            walk_stats(&entry.path(), files, bytes);
        } else if let Ok(meta) = entry.metadata() {
            *files += 1;
            *bytes += meta.len();
        }
    }
}

fn collect_leftovers(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(path) else {
        out.push(path.to_path_buf());
        return;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if is_dir {
            collect_leftovers(&entry_path, out);
        } else {
            out.push(entry_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_removes_nested_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("OdiumLauncher");
        let nested = root.join("instances").join("odium").join("mods");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join("logs")).unwrap();
        fs::write(root.join("config.json"), b"{}").unwrap();
        fs::write(nested.join("mod.jar"), vec![0u8; 1024]).unwrap();
        fs::write(root.join("logs").join("latest.log"), b"log").unwrap();

        let report = purge(&root).unwrap();
        assert!(!root.exists());
        assert!(report.root_existed);
        assert_eq!(report.removed_files, 3);
        assert!(report.removed_bytes >= 1027);
        assert!(report.leftovers.is_empty());
    }

    #[test]
    fn purge_missing_root_is_a_no_op() {
        let temp = tempfile::tempdir().unwrap();
        let report = purge(&temp.path().join("never-created")).unwrap();
        assert!(!report.root_existed);
        assert_eq!(report.removed_files, 0);
    }

    #[test]
    fn refuses_drive_root() {
        let root = Path::new("C:\\");
        assert!(!is_safe_root(root));
    }

    #[test]
    fn refuses_temp_dir_itself() {
        let temp = std::env::temp_dir();
        let canonical = fs::canonicalize(&temp).unwrap();
        assert!(!is_safe_root(&canonical));
    }

    #[test]
    fn refuses_directory_containing_the_executable() {
        let exe = std::env::current_exe().unwrap();
        let exe = fs::canonicalize(exe).unwrap();
        let parent = exe.parent().unwrap();
        assert!(!is_safe_root(parent));
    }

    #[test]
    fn accepts_a_normal_app_folder() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("OdiumLauncher");
        fs::create_dir_all(&root).unwrap();
        let canonical = fs::canonicalize(&root).unwrap();
        assert!(is_safe_root(&canonical));
    }

    #[test]
    fn refuses_a_folder_with_a_foreign_name() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Documents");
        fs::create_dir_all(&root).unwrap();
        assert!(!is_safe_root(&root));

        let portable = temp.path().join("launcher_data");
        fs::create_dir_all(&portable).unwrap();
        let canonical = fs::canonicalize(&portable).unwrap();
        assert!(is_safe_root(&canonical));
    }
}
