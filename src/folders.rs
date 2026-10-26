//! The groups the user sorts PRs into, kept in a local SQLite file.
//!
//! Called "grupos" in the window; `Folder` here so it isn't mistaken for
//! `status::Group`, the automatic sections of "Mis PRs". A PR belongs to at
//! most one folder, by URL, so it stays in it after it merges. Like `merged`,
//! everything here blocks and runs on the background executor.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::db;

/// Where the sidebar's width is remembered.
const SIDEBAR_WIDTH: &str = "sidebar_width";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: i64,
    pub name: String,
}

/// Every folder in creation order, and which folder each PR URL is in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Folders {
    pub list: Vec<Folder>,
    pub by_url: HashMap<String, i64>,
}

pub fn load() -> Result<Folders, String> {
    read(&open()?)
}

/// Adds a folder at the end of the list and returns it.
pub fn create(name: &str) -> Result<Folder, String> {
    let db = open()?;
    db.execute(
        "INSERT INTO folders (name, position)
         VALUES (?1, (SELECT COALESCE(MAX(position), 0) + 1 FROM folders))",
        params![name],
    )
    .map_err(db_error)?;
    Ok(Folder {
        id: db.last_insert_rowid(),
        name: name.to_string(),
    })
}

pub fn rename(id: i64, name: &str) -> Result<(), String> {
    open()?
        .execute(
            "UPDATE folders SET name = ?2 WHERE id = ?1",
            params![id, name],
        )
        .map(drop)
        .map_err(db_error)
}

/// Removes the folder; its PRs go back to having none.
pub fn delete(id: i64) -> Result<(), String> {
    let db = open()?;
    let tx = db.unchecked_transaction().map_err(db_error)?;
    tx.execute("DELETE FROM members WHERE folder_id = ?1", params![id])
        .map_err(db_error)?;
    tx.execute("DELETE FROM folders WHERE id = ?1", params![id])
        .map_err(db_error)?;
    tx.commit().map_err(db_error)
}

/// Moves the PR at `url` into `folder`, or out of any with `None`.
pub fn assign(url: &str, folder: Option<i64>) -> Result<(), String> {
    write_assignment(&open()?, url, folder)
}

/// How wide the user last left the sidebar, in pixels.
pub fn sidebar_width() -> Result<Option<f32>, String> {
    read_width(&open()?)
}

pub fn save_sidebar_width(width: f32) -> Result<(), String> {
    write_width(&open()?, width)
}

fn read_width(db: &Connection) -> Result<Option<f32>, String> {
    db.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![SIDEBAR_WIDTH],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map(|value| value.and_then(|width| width.parse().ok()))
    .map_err(db_error)
}

fn write_width(db: &Connection, width: f32) -> Result<(), String> {
    db.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SIDEBAR_WIDTH, width.to_string()],
    )
    .map(drop)
    .map_err(db_error)
}

fn write_assignment(db: &Connection, url: &str, folder: Option<i64>) -> Result<(), String> {
    match folder {
        Some(id) => db.execute(
            "INSERT INTO members (url, folder_id) VALUES (?1, ?2)
             ON CONFLICT(url) DO UPDATE SET folder_id = excluded.folder_id",
            params![url, id],
        ),
        None => db.execute("DELETE FROM members WHERE url = ?1", params![url]),
    }
    .map(drop)
    .map_err(db_error)
}

fn read(db: &Connection) -> Result<Folders, String> {
    let mut statement = db
        .prepare("SELECT id, name FROM folders ORDER BY position, id")
        .map_err(db_error)?;
    let list = statement
        .query_map([], |row| {
            Ok(Folder {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .map_err(db_error)?;

    let mut statement = db
        .prepare("SELECT url, folder_id FROM members")
        .map_err(db_error)?;
    let by_url = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .and_then(|rows| rows.collect::<Result<HashMap<_, _>, _>>())
        .map_err(db_error)?;

    Ok(Folders { list, by_url })
}

fn open() -> Result<Connection, String> {
    let db = db::open("groups.sqlite")?;
    migrate(&db)?;
    Ok(db)
}

fn migrate(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS folders (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             position INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS members (
             url TEXT PRIMARY KEY,
             folder_id INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS members_by_folder ON members (folder_id);
         CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .map_err(db_error)
}

fn db_error(error: rusqlite::Error) -> String {
    format!("Error de la base local de grupos: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://github.com/acme/payments/pull/7";

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        migrate(&db).unwrap();
        db
    }

    fn add(db: &Connection, name: &str) -> i64 {
        db.execute(
            "INSERT INTO folders (name, position) VALUES (?1, 0)",
            params![name],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    #[test]
    fn a_pull_request_moves_between_folders_and_out_of_them() {
        let db = db();
        let first = add(&db, "Pagos");
        let second = add(&db, "Webhooks");

        write_assignment(&db, URL, Some(first)).unwrap();
        write_assignment(&db, URL, Some(second)).unwrap();
        let moved = read(&db).unwrap().by_url.get(URL).copied();
        write_assignment(&db, URL, None).unwrap();

        assert_eq!(moved, Some(second));
        assert!(read(&db).unwrap().by_url.is_empty());
    }

    #[test]
    fn the_sidebar_width_is_remembered_once_saved() {
        let db = db();

        let before = read_width(&db).unwrap();
        write_width(&db, 240.).unwrap();

        assert_eq!(before, None);
        assert_eq!(read_width(&db).unwrap(), Some(240.));
    }

    #[test]
    fn folders_read_back_in_creation_order() {
        let db = db();
        add(&db, "Pagos");
        add(&db, "Webhooks");

        let names: Vec<_> = read(&db)
            .unwrap()
            .list
            .into_iter()
            .map(|f| f.name)
            .collect();

        assert_eq!(names, ["Pagos", "Webhooks"]);
    }
}
