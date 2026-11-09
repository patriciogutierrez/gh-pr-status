//! The app's settings: who to ask for reviews and where, which bot comments
//! the pipelines, and the like. They live in a local SQLite file, except the
//! secrets (the review webhook and the Slack token), which go to the macOS
//! Keychain through the `security` tool. The Keychain trusts that tool, not
//! the app, so a rebuilt app reads them without asking.
//!
//! Each setting is named after the `GH_PR_STATUS_*` variable that used to set
//! it. One still set in the environment and not yet saved is saved on the
//! first load, so moving off `.env` takes nothing by hand. Everything is read
//! once at startup and kept in memory, since the menus ask on every render.

use std::collections::HashMap;
use std::env;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{OnceLock, RwLock};

use rusqlite::{Connection, OptionalExtension, params};

use crate::db;

/// Where the Keychain items are filed, one per secret setting.
const KEYCHAIN_SERVICE: &str = "com.patriciogutierrez.pr-manager";
/// What `security` exits with when the item isn't there.
const NOT_FOUND: i32 = 44;

/// One setting as the settings window shows it.
pub struct Setting {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub secret: bool,
}

const fn setting(key: &'static str, label: &'static str, help: &'static str) -> Setting {
    Setting {
        key,
        label,
        help,
        secret: false,
    }
}

const fn secret(key: &'static str, label: &'static str, help: &'static str) -> Setting {
    Setting {
        key,
        label,
        help,
        secret: true,
    }
}

pub const SLACK_USER: &str = "GH_PR_STATUS_SLACK_USER";
pub const REVIEW_WEBHOOK: &str = "GH_PR_STATUS_REVIEW_WEBHOOK";
pub const REVIEWERS: &str = "GH_PR_STATUS_REVIEWERS";
pub const REVIEW_CHANNELS: &str = "GH_PR_STATUS_REVIEW_CHANNELS";
pub const SLACK_TOKEN: &str = "GH_PR_STATUS_SLACK_TOKEN";
pub const SLACK_CHANNEL: &str = "GH_PR_STATUS_SLACK_CHANNEL";
pub const SLACK_TEAM: &str = "GH_PR_STATUS_SLACK_TEAM";
pub const BOT: &str = "GH_PR_STATUS_BOT";
pub const GCP_PREPROD: &str = "GH_PR_STATUS_GCP_PREPROD";
pub const GCP_PROD: &str = "GH_PR_STATUS_GCP_PROD";
pub const GCP_DEVELOPMENT: &str = "GH_PR_STATUS_GCP_DEVELOPMENT";
pub const CODE_DIR: &str = "GH_PR_STATUS_CODE_DIR";

/// Every setting, in the order the window lists them.
pub const ALL: [Setting; 12] = [
    setting(SLACK_USER, "Tu ID de Slack", "Va como quien pide la review"),
    secret(
        REVIEW_WEBHOOK,
        "Webhook del workflow",
        "hooks.slack.com/triggers/…",
    ),
    setting(
        REVIEWERS,
        "Reviewers",
        "Apodo:IDdeSlack:loginDeGitHub, separados por coma",
    ),
    setting(
        REVIEW_CHANNELS,
        "Canales de review",
        "nombre:IDdeCanal, separados por coma; el primero va preseleccionado",
    ),
    secret(
        SLACK_TOKEN,
        "Token de Slack",
        "Token de usuario (xoxp-…) para seguir los hilos de review",
    ),
    setting(
        SLACK_CHANNEL,
        "Canal de deploys",
        "ID del canal que abre el menú",
    ),
    setting(SLACK_TEAM, "Workspace de Slack", "ID del workspace (T…)"),
    setting(
        BOT,
        "Bot de pipelines",
        "Login de GitHub del bot que comenta los runs",
    ),
    setting(
        GCP_PREPROD,
        "Proyecto GCP preprod",
        "Para los links a los logs",
    ),
    setting(GCP_PROD, "Proyecto GCP prod", "Para los links a los logs"),
    setting(
        GCP_DEVELOPMENT,
        "Proyecto GCP development",
        "Para los links a los logs",
    ),
    setting(
        CODE_DIR,
        "Carpeta de repos",
        "Donde están clonados los repos del agente; por defecto ~/code",
    ),
];

fn values() -> &'static RwLock<Option<HashMap<&'static str, String>>> {
    static VALUES: OnceLock<RwLock<Option<HashMap<&'static str, String>>>> = OnceLock::new();
    VALUES.get_or_init(|| RwLock::new(None))
}

/// The value of setting `key`, or `None` when it is empty or unset. Before
/// [`load`] runs, the environment answers.
pub fn get(key: &str) -> Option<String> {
    let loaded = values()
        .read()
        .ok()
        .and_then(|values| values.as_ref().map(|values| values.get(key).cloned()));
    match loaded {
        Some(value) => value,
        None => from_env(key),
    }
}

/// Reads every setting into memory, saving the ones the environment sets
/// and nothing stored does yet. A setting that fails to read is left out
/// and named in the error; the rest still load.
pub fn load() -> Result<(), String> {
    let db = open()?;
    let mut loaded = HashMap::new();
    let mut errors = Vec::new();
    for setting in &ALL {
        let stored = if setting.secret {
            keychain_get(setting.key)
        } else {
            read(&db, setting.key)
        };
        let value = match stored {
            Ok(Some(value)) => Some(value),
            Ok(None) => from_env(setting.key).inspect(|value| {
                if let Err(error) = store(&db, setting, value) {
                    errors.push(error);
                }
            }),
            Err(error) => {
                errors.push(error);
                None
            }
        };
        if let Some(value) = value {
            loaded.insert(setting.key, value);
        }
    }
    if let Ok(mut values) = values().write() {
        *values = Some(loaded);
    }
    match errors.is_empty() {
        true => Ok(()),
        false => Err(errors.join("; ")),
    }
}

/// Saves the settings changed in the window. An empty value clears a plain
/// setting; a secret left empty keeps what it had, since the window never
/// shows it.
pub fn save(changes: Vec<(&'static str, String)>) -> Result<(), String> {
    let db = open()?;
    for (key, value) in changes {
        let Some(setting) = ALL.iter().find(|setting| setting.key == key) else {
            continue;
        };
        let value = value.trim().to_string();
        if value.is_empty() && setting.secret {
            continue;
        }
        store(&db, setting, &value)?;
        if let Ok(mut values) = values().write() {
            let values = values.get_or_insert_with(HashMap::new);
            if value.is_empty() {
                values.remove(key);
            } else {
                values.insert(key, value);
            }
        }
    }
    Ok(())
}

fn from_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn store(db: &Connection, setting: &Setting, value: &str) -> Result<(), String> {
    if setting.secret {
        keychain_set(setting.key, value)
    } else {
        write(db, setting.key, value)
    }
}

fn open() -> Result<Connection, String> {
    let db = db::open("settings.sqlite")?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .map_err(db_error)?;
    Ok(db)
}

fn read(db: &Connection, key: &str) -> Result<Option<String>, String> {
    db.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map(|value| value.filter(|value| !value.is_empty()))
    .map_err(db_error)
}

fn write(db: &Connection, key: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        db.execute("DELETE FROM settings WHERE key = ?1", params![key])
    } else {
        db.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
    }
    .map(drop)
    .map_err(db_error)
}

fn keychain_get(key: &str) -> Result<Option<String>, String> {
    let output = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            key,
            "-w",
        ])
        .output()
        .map_err(|error| format!("No se pudo leer el Llavero: {error}"))?;
    match output.status.code() {
        Some(0) => Ok(
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
                .filter(|value| !value.is_empty()),
        ),
        Some(NOT_FOUND) => Ok(None),
        _ => Err(format!(
            "No se pudo leer {key} del Llavero: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Writes through `security`'s own prompt on stdin, so the secret never
/// shows in the process list as an argument would.
fn keychain_set(key: &str, value: &str) -> Result<(), String> {
    if value.contains(['"', '\\', '\n', '\r']) {
        return Err(format!(
            "{key} no puede llevar comillas, \\ ni saltos de línea"
        ));
    }
    let mut child = Command::new("/usr/bin/security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("No se pudo abrir el Llavero: {error}"))?;
    let command =
        format!("add-generic-password -U -s {KEYCHAIN_SERVICE} -a {key} -w \"{value}\"\n");
    child
        .stdin
        .take()
        .ok_or("No se pudo escribir al Llavero")?
        .write_all(command.as_bytes())
        .map_err(|error| format!("No se pudo escribir al Llavero: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("No se pudo escribir al Llavero: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() && stderr.trim().is_empty() {
        Ok(())
    } else {
        Err(format!(
            "No se pudo guardar {key} en el Llavero: {}",
            stderr.trim()
        ))
    }
}

fn db_error(error: rusqlite::Error) -> String {
    format!("Error en settings.sqlite: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        db
    }

    #[test]
    fn a_saved_setting_reads_back_and_clearing_it_forgets_it() {
        let db = db();
        write(&db, BOT, "toku-bot").unwrap();

        let saved = read(&db, BOT).unwrap();
        write(&db, BOT, "").unwrap();

        assert_eq!(saved.as_deref(), Some("toku-bot"));
        assert_eq!(read(&db, BOT).unwrap(), None);
    }

    #[test]
    fn a_secret_with_a_quote_is_refused_before_reaching_the_keychain() {
        assert!(keychain_set(SLACK_TOKEN, "xoxp-\"; delete").is_err());
    }

    #[test]
    fn every_setting_has_its_own_key() {
        let keys: std::collections::HashSet<_> = ALL.iter().map(|setting| setting.key).collect();

        assert_eq!(keys.len(), ALL.len());
    }
}
