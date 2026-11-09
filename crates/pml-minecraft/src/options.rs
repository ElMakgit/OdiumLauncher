//! Minimal `options.txt` maintenance.
//!
//! The launcher only touches the `lang` key: it makes Russian the default for
//! a fresh instance and migrates the English default left by a vanilla launch.
//! Any other language chosen by the player is preserved, as is every other
//! option in the file.

use std::fs;
use std::path::Path;

const RUSSIAN: &str = "ru_ru";
const ENGLISH: &str = "en_us";

/// Ensures `<instance>/options.txt` selects Russian.
pub fn ensure_russian_language(instance_dir: &Path) -> Result<(), String> {
    let path = instance_dir.join("options.txt");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return write_options(&path, &format!("lang:{RUSSIAN}\n"));
        }
        Err(error) => return Err(format!("не удалось прочитать options.txt: {error}")),
    };

    let mut found = false;
    let mut lines = Vec::new();
    for line in contents.lines() {
        let (key, value) = line.split_once(':').unwrap_or((line, ""));
        if key == "lang" {
            found = true;
            if value.is_empty() || value == ENGLISH {
                lines.push(format!("lang:{RUSSIAN}"));
            } else {
                lines.push(line.to_owned());
            }
        } else {
            lines.push(line.to_owned());
        }
    }
    if !found {
        lines.push(format!("lang:{RUSSIAN}"));
    }

    let mut updated = lines.join("\n");
    updated.push('\n');
    if updated == contents {
        return Ok(());
    }
    write_options(&path, &updated)
}

fn write_options(path: &Path, contents: &str) -> Result<(), String> {
    let temporary = path.with_extension("txt.tmp");
    fs::write(&temporary, contents)
        .map_err(|error| format!("не удалось записать {}: {error}", temporary.display()))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("не удалось заменить {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_russian_options_for_new_instance() {
        let directory = tempfile::tempdir().expect("tempdir");
        ensure_russian_language(directory.path()).expect("create");
        let contents = fs::read_to_string(directory.path().join("options.txt")).expect("read");
        assert!(contents.contains("lang:ru_ru"));
    }

    #[test]
    fn migrates_english_and_keeps_other_settings() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("options.txt");
        fs::write(&path, "fov:0.5\nlang:en_us\nrenderDistance:12\n").expect("write");
        ensure_russian_language(directory.path()).expect("migrate");
        let contents = fs::read_to_string(&path).expect("read");
        assert!(contents.contains("lang:ru_ru"));
        assert!(!contents.contains("lang:en_us"));
        assert!(contents.contains("fov:0.5"));
        assert!(contents.contains("renderDistance:12"));
    }

    #[test]
    fn preserves_player_chosen_language() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("options.txt");
        fs::write(&path, "lang:de_de\n").expect("write");
        ensure_russian_language(directory.path()).expect("keep");
        let contents = fs::read_to_string(&path).expect("read");
        assert!(contents.contains("lang:de_de"));
    }
}
