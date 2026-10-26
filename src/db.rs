//! Where the app keeps its local SQLite files.

use std::path::PathBuf;

use rusqlite::Connection;

/// Opens `file` in `~/Library/Application Support/gh-pr-status`, creating the
/// folder the first time.
pub fn open(file: &str) -> Result<Connection, String> {
    let home = std::env::var_os("HOME").ok_or("No se encontró HOME")?;
    let dir = PathBuf::from(home).join("Library/Application Support/gh-pr-status");
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("No se pudo crear {}: {error}", dir.display()))?;
    let path = dir.join(file);
    Connection::open(&path).map_err(|error| format!("No se pudo abrir {}: {error}", path.display()))
}
