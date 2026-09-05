//! One atomic-JSON-write helper shared by every config store (review
//! — previously duplicated line-for-line in `projects` and `settings`, so a
//! durability fix would have had to land twice).

use std::fs;
use std::path::Path;

use serde::Serialize;

/// Serialize `value` (pretty) and replace `path` atomically: write a temp
/// next to the target, then rename — a crash mid-write can never leave a
/// truncated file. Creates the parent directory if missing.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write_text_atomic(path, &json, "json.tmp")
}

/// Replace `path` with `text` atomically (temp next to the target, then
/// rename). `tmp_ext` names the temp file's extension so two stores in one
/// directory (projects.json / settings.json) or a `chappa.yml` never collide on
/// a temp name. The project-file write-back uses this directly.
pub fn write_text_atomic(path: &Path, text: &str, tmp_ext: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent dir", path.display()))?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(tmp_ext);
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    if let Err(e) = fs::rename(&tmp, path) {
        // A failed rename must not leave the temp file behind (
        // review): it would sit next to chappa.yml forever, and a glob like
        // `chappa.yml*` in a restart_when_changed list would keep matching it.
        let _ = fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}
