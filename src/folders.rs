//! The groups the user sorts PRs into, kept in a local SQLite file.
//!
//! Called "grupos" in the window; `Folder` here so it isn't mistaken for
//! `status::Group`, the automatic sections of "Mis PRs". A PR belongs to at
//! most one folder, by URL, so it stays in it after it merges. A folder can
//! hold subfolders one level deep, which share its agent. Like `merged`,
//! everything here blocks and runs on the background executor.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};

use crate::db;

/// Where the sidebar's width is remembered.
const SIDEBAR_WIDTH: &str = "sidebar_width";
/// Where the agent panel's height is remembered.
const AGENT_HEIGHT: &str = "agent_height";
/// Where the folded folders are remembered, as comma-separated ids.
const COLLAPSED: &str = "collapsed";
/// Where each folder's agent folder is remembered, followed by the folder id.
const AGENT_DIR: &str = "agent_dir:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: i64,
    pub name: String,
    /// The folder this one is a subfolder of.
    pub parent: Option<i64>,
}

/// Every folder in creation order, and which folder each PR URL is in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Folders {
    pub list: Vec<Folder>,
    pub by_url: HashMap<String, i64>,
    /// PRs once put in a folder on their own, from their Orca worktree. They
    /// are never put back, so moving one out by hand sticks.
    pub claimed: HashSet<String>,
    /// Folders whose subfolders the sidebar hides.
    pub collapsed: HashSet<i64>,
}

impl Folders {
    pub fn get(&self, id: i64) -> Option<&Folder> {
        self.list.iter().find(|folder| folder.id == id)
    }

    /// The top-level folder `id` is in: its parent, or itself.
    pub fn root(&self, id: i64) -> Option<&Folder> {
        let folder = self.get(id)?;
        match folder.parent {
            Some(parent) => self.get(parent),
            None => Some(folder),
        }
    }

    pub fn children(&self, id: i64) -> impl Iterator<Item = &Folder> {
        self.list
            .iter()
            .filter(move |folder| folder.parent == Some(id))
    }

    /// Whether a PR in folder `member` shows under folder `id`: it is in it,
    /// or in one of its subfolders.
    pub fn includes(&self, id: i64, member: i64) -> bool {
        member == id || self.get(member).and_then(|folder| folder.parent) == Some(id)
    }
}

pub fn load() -> Result<Folders, String> {
    read(&open()?)
}

/// Adds a folder at the end of the list, or of `parent`'s subfolders, and
/// returns it.
pub fn create(name: &str, parent: Option<i64>) -> Result<Folder, String> {
    write_folder(&open()?, name, parent)
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

/// Removes the folder and its subfolders; their PRs go back to having none.
pub fn delete(id: i64) -> Result<(), String> {
    write_deletion(&open()?, id)
}

/// Moves the PR at `url` into `folder`, or out of any with `None`.
pub fn assign(url: &str, folder: Option<i64>) -> Result<(), String> {
    write_assignment(&open()?, url, folder)
}

/// Puts the PR at `url` in `folder` because its worktree belongs to it, and
/// remembers that so it is never put there again.
pub fn claim(url: &str, folder: i64) -> Result<(), String> {
    write_claim(&open()?, url, folder)
}

pub fn save_collapsed(ids: &HashSet<i64>) -> Result<(), String> {
    let mut ids: Vec<_> = ids.iter().map(i64::to_string).collect();
    ids.sort();
    write_setting(&open()?, COLLAPSED, &ids.join(","))
}

/// How wide the user last left the sidebar, in pixels.
pub fn sidebar_width() -> Result<Option<f32>, String> {
    read_width(&open()?)
}

pub fn save_sidebar_width(width: f32) -> Result<(), String> {
    write_setting(&open()?, SIDEBAR_WIDTH, &width.to_string())
}

/// How tall the user last left the agent's panel, in pixels.
pub fn agent_height() -> Result<Option<f32>, String> {
    read_setting(&open()?, AGENT_HEIGHT).map(|value| value.and_then(|height| height.parse().ok()))
}

pub fn save_agent_height(height: f32) -> Result<(), String> {
    write_setting(&open()?, AGENT_HEIGHT, &height.to_string())
}

/// The folder on disk where `folder`'s agent works, once it has one.
pub fn agent_dir(folder: i64) -> Result<Option<String>, String> {
    read_setting(&open()?, &format!("{AGENT_DIR}{folder}"))
}

pub fn save_agent_dir(folder: i64, dir: &str) -> Result<(), String> {
    write_setting(&open()?, &format!("{AGENT_DIR}{folder}"), dir)
}

fn read_width(db: &Connection) -> Result<Option<f32>, String> {
    read_setting(db, SIDEBAR_WIDTH).map(|value| value.and_then(|width| width.parse().ok()))
}

fn read_setting(db: &Connection, key: &str) -> Result<Option<String>, String> {
    db.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(db_error)
}

fn write_setting(db: &Connection, key: &str, value: &str) -> Result<(), String> {
    db.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map(drop)
    .map_err(db_error)
}

fn write_folder(db: &Connection, name: &str, parent: Option<i64>) -> Result<Folder, String> {
    db.execute(
        "INSERT INTO folders (name, position, parent_id)
         VALUES (?1, (SELECT COALESCE(MAX(position), 0) + 1 FROM folders), ?2)",
        params![name, parent],
    )
    .map_err(db_error)?;
    Ok(Folder {
        id: db.last_insert_rowid(),
        name: name.to_string(),
        parent,
    })
}

fn write_deletion(db: &Connection, id: i64) -> Result<(), String> {
    let tx = db.unchecked_transaction().map_err(db_error)?;
    tx.execute(
        "DELETE FROM members WHERE folder_id IN
             (SELECT id FROM folders WHERE id = ?1 OR parent_id = ?1)",
        params![id],
    )
    .map_err(db_error)?;
    tx.execute(
        "DELETE FROM settings WHERE key = ?1",
        params![format!("{AGENT_DIR}{id}")],
    )
    .map_err(db_error)?;
    tx.execute(
        "DELETE FROM folders WHERE id = ?1 OR parent_id = ?1",
        params![id],
    )
    .map_err(db_error)?;
    tx.commit().map_err(db_error)
}

fn write_claim(db: &Connection, url: &str, folder: i64) -> Result<(), String> {
    let tx = db.unchecked_transaction().map_err(db_error)?;
    write_assignment(&tx, url, Some(folder))?;
    tx.execute(
        "INSERT OR IGNORE INTO claimed (url) VALUES (?1)",
        params![url],
    )
    .map_err(db_error)?;
    tx.commit().map_err(db_error)
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
        .prepare("SELECT id, name, parent_id FROM folders ORDER BY position, id")
        .map_err(db_error)?;
    let list = statement
        .query_map([], |row| {
            Ok(Folder {
                id: row.get(0)?,
                name: row.get(1)?,
                parent: row.get(2)?,
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

    let mut statement = db.prepare("SELECT url FROM claimed").map_err(db_error)?;
    let claimed = statement
        .query_map([], |row| row.get(0))
        .and_then(|rows| rows.collect::<Result<HashSet<_>, _>>())
        .map_err(db_error)?;

    let collapsed = read_setting(db, COLLAPSED)?
        .unwrap_or_default()
        .split(',')
        .filter_map(|id| id.parse().ok())
        .collect();

    Ok(Folders {
        list,
        by_url,
        claimed,
        collapsed,
    })
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
         CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS claimed (url TEXT PRIMARY KEY);",
    )
    .map_err(db_error)?;

    // Files from before subfolders lack the column. The window opens several
    // connections at once, so the check and the change hold the write lock.
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let has_parent = tx
        .prepare("SELECT 1 FROM pragma_table_info('folders') WHERE name = 'parent_id'")
        .and_then(|mut statement| statement.exists([]))
        .map_err(db_error)?;
    if !has_parent {
        tx.execute_batch("ALTER TABLE folders ADD COLUMN parent_id INTEGER")
            .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)
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
        write_setting(&db, SIDEBAR_WIDTH, "240").unwrap();

        assert_eq!(before, None);
        assert_eq!(read_width(&db).unwrap(), Some(240.));
    }

    #[test]
    fn a_claimed_pull_request_stays_claimed_after_it_is_moved_out() {
        let db = db();
        let folder = add(&db, "Pagos");

        write_claim(&db, URL, folder).unwrap();
        write_assignment(&db, URL, None).unwrap();
        let folders = read(&db).unwrap();

        assert!(folders.by_url.is_empty());
        assert!(folders.claimed.contains(URL));
    }

    #[test]
    fn deleting_a_folder_takes_its_subfolders_and_their_pull_requests() {
        let db = db();
        let parent = write_folder(&db, "Activaciones", None).unwrap();
        let child = write_folder(&db, "Front", Some(parent.id)).unwrap();
        let other = write_folder(&db, "Pagos", None).unwrap();
        write_assignment(&db, URL, Some(child.id)).unwrap();

        write_deletion(&db, parent.id).unwrap();
        let folders = read(&db).unwrap();

        assert_eq!(folders.list, [other]);
        assert!(folders.by_url.is_empty());
    }

    #[test]
    fn a_folder_includes_its_subfolders_but_not_its_parent() {
        let db = db();
        let parent = write_folder(&db, "Activaciones", None).unwrap();
        let child = write_folder(&db, "Front", Some(parent.id)).unwrap();
        let folders = read(&db).unwrap();

        assert!(folders.includes(parent.id, child.id));
        assert!(!folders.includes(child.id, parent.id));
        assert_eq!(folders.root(child.id), Some(&parent));
    }

    #[test]
    fn a_file_from_before_subfolders_gains_the_column() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE folders (id INTEGER PRIMARY KEY, name TEXT NOT NULL, position INTEGER NOT NULL);
             INSERT INTO folders (name, position) VALUES ('Pagos', 1);",
        )
        .unwrap();

        migrate(&db).unwrap();
        migrate(&db).unwrap();

        assert_eq!(read(&db).unwrap().list[0].parent, None);
    }

    #[test]
    fn folded_folders_read_back() {
        let db = db();

        write_setting(&db, COLLAPSED, "4,9").unwrap();

        assert_eq!(read(&db).unwrap().collapsed, HashSet::from([4, 9]));
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
