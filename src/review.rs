//! Asking for a review through the team's Slack workflow.
//!
//! The workflow starts from a webhook: one `POST` with the reviewer, the
//! requester, the channel, the urgency, a comment and the PR's link, and it
//! posts the request with its "aprobar" button in the channel. Everything the
//! form offers comes from `GH_PR_STATUS_*` variables, so the app holds no
//! names or IDs of its own. When each request was sent is kept in a local
//! SQLite file, so the rows still say so after a restart. The send and the
//! file block and run on the background executor.

use std::collections::HashMap;
use std::env;
use std::process::Command;

use rusqlite::{Connection, params};
use serde_json::json;

use crate::{db, tool};

/// The urgencies the workflow knows, as its stickers: the word shown, and
/// what is sent.
pub const URGENCIES: [(&str, &str); 4] = [
    ("highest", ":priority-highest:"),
    ("high", ":priority-high:"),
    ("medium", ":priority-medium:"),
    ("low", ":priority-low:"),
];

/// Someone or somewhere in Slack: the name shown, and the ID sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub name: String,
    pub id: String,
}

/// What the form needs, read once from the environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub webhook: String,
    /// The user's own Slack ID, sent as the requester.
    pub requester: String,
    pub reviewers: Vec<Choice>,
    pub channels: Vec<Choice>,
}

/// What one request says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub reviewer: String,
    pub channel: String,
    pub urgency: String,
    pub comment: String,
    pub pr_url: String,
}

/// `None` unless the webhook, the requester, a reviewer and a channel are all
/// configured, since the form needs every one of them.
pub fn config() -> Option<Config> {
    let var = |name: &str| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let config = Config {
        webhook: var("GH_PR_STATUS_REVIEW_WEBHOOK")?,
        requester: var("GH_PR_STATUS_SLACK_USER")?,
        reviewers: parse_choices(&var("GH_PR_STATUS_REVIEWERS")?),
        channels: parse_choices(&var("GH_PR_STATUS_REVIEW_CHANNELS")?),
    };
    (!config.reviewers.is_empty() && !config.channels.is_empty()).then_some(config)
}

/// `Name:ID,Name:ID`, in alphabetical order and without accents; entries
/// without a colon are skipped.
fn parse_choices(list: &str) -> Vec<Choice> {
    let mut choices: Vec<Choice> = list
        .split(',')
        .filter_map(|entry| {
            let (name, id) = entry.split_once(':')?;
            let (name, id) = (plain(name.trim()), id.trim());
            (!name.is_empty() && !id.is_empty()).then(|| Choice {
                name,
                id: id.to_string(),
            })
        })
        .collect();
    choices.sort_by_key(|choice| choice.name.to_lowercase());
    choices
}

/// The name with its accents dropped: `Bastián` reads `Bastian`.
fn plain(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'Á' | 'À' | 'Ä' => 'A',
            'É' | 'È' | 'Ë' => 'E',
            'Í' | 'Ì' | 'Ï' => 'I',
            'Ó' | 'Ò' | 'Ö' => 'O',
            'Ú' | 'Ù' | 'Ü' => 'U',
            c => c,
        })
        .collect()
}

/// Posts the request to the workflow. Slack answers `{"ok":true}` when the
/// workflow started; anything else is its error.
pub fn send(config: &Config, request: &Request) -> Result<(), String> {
    let body = payload(config, request).to_string();
    let output = Command::new(tool::path("curl"))
        .args(["-sS", "--max-time", "15", "-X", "POST"])
        .args(["-H", "Content-Type: application/json"])
        .args(["-d", &body, &config.webhook])
        .output()
        .map_err(|error| format!("No se pudo ejecutar curl: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("No se pudo llegar a Slack: {}", stderr.trim()));
    }
    let answer = String::from_utf8_lossy(&output.stdout);
    match serde_json::from_str::<serde_json::Value>(&answer) {
        Ok(value) if value["ok"].as_bool() == Some(true) => Ok(()),
        Ok(value) => Err(format!(
            "Slack rechazó el pedido: {}",
            value["error"].as_str().unwrap_or(answer.trim())
        )),
        Err(_) => {
            Err("Slack respondió algo que no es JSON; ¿la URL es la del webhook?".to_string())
        }
    }
}

/// When a review was last asked for, by PR URL, as seconds since the epoch.
pub fn asked() -> Result<HashMap<String, i64>, String> {
    read_asked(&open()?)
}

pub fn remember(url: &str, at: i64) -> Result<(), String> {
    write_asked(&open()?, url, at)
}

fn read_asked(db: &Connection) -> Result<HashMap<String, i64>, String> {
    let mut statement = db
        .prepare("SELECT url, asked_at FROM requests")
        .map_err(db_error)?;
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .and_then(|rows| rows.collect())
        .map_err(db_error)
}

fn write_asked(db: &Connection, url: &str, at: i64) -> Result<(), String> {
    db.execute(
        "INSERT INTO requests (url, asked_at) VALUES (?1, ?2)
         ON CONFLICT(url) DO UPDATE SET asked_at = excluded.asked_at",
        params![url, at],
    )
    .map(drop)
    .map_err(db_error)
}

fn open() -> Result<Connection, String> {
    let db = db::open("reviews.sqlite")?;
    migrate(&db)?;
    Ok(db)
}

fn migrate(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS requests (url TEXT PRIMARY KEY, asked_at INTEGER NOT NULL);",
    )
    .map_err(db_error)
}

fn db_error(error: rusqlite::Error) -> String {
    format!("Error de la base local de reviews: {error}")
}

fn payload(config: &Config, request: &Request) -> serde_json::Value {
    json!({
        "reviewer": request.reviewer,
        "requester": config.requester,
        "channel": request.channel,
        "urgency": request.urgency,
        "comment": request.comment,
        "pr_url": request.pr_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_come_as_name_colon_id_pairs_sorted_and_without_accents() {
        let choices = parse_choices("Pato:U1, Matute : U2,sin-id,:U3,Bastián:U4");

        assert_eq!(
            choices,
            [
                Choice {
                    name: "Bastian".to_string(),
                    id: "U4".to_string()
                },
                Choice {
                    name: "Matute".to_string(),
                    id: "U2".to_string()
                },
                Choice {
                    name: "Pato".to_string(),
                    id: "U1".to_string()
                },
            ]
        );
    }

    #[test]
    fn a_request_is_remembered_and_asking_again_updates_it() {
        let db = Connection::open_in_memory().unwrap();
        migrate(&db).unwrap();

        write_asked(&db, "https://github.com/acme/demo/pull/1", 100).unwrap();
        write_asked(&db, "https://github.com/acme/demo/pull/1", 200).unwrap();

        assert_eq!(
            read_asked(&db).unwrap(),
            HashMap::from([("https://github.com/acme/demo/pull/1".to_string(), 200)])
        );
    }

    #[test]
    fn the_payload_carries_every_field_the_workflow_expects() {
        let config = Config {
            webhook: String::new(),
            requester: "U0".to_string(),
            reviewers: Vec::new(),
            channels: Vec::new(),
        };
        let request = Request {
            reviewer: "U1".to_string(),
            channel: "C1".to_string(),
            urgency: ":priority-high:".to_string(),
            comment: "por favor".to_string(),
            pr_url: "https://github.com/acme/demo/pull/1".to_string(),
        };

        let payload = payload(&config, &request);

        assert_eq!(payload["requester"], "U0");
        assert_eq!(payload["reviewer"], "U1");
        assert_eq!(payload["urgency"], ":priority-high:");
        assert_eq!(payload["pr_url"], "https://github.com/acme/demo/pull/1");
    }
}
