//! The user's merged pull requests, kept in a local SQLite file.
//!
//! The first sync backfills from the Monday of last week; every later one only
//! asks GitHub for what merged since the day of the previous sync, usually just
//! today, so past days are read from disk instead of searched again. Like
//! `github`, everything here blocks and runs on the background executor.
//!
//! GitHub's search sometimes returns fewer results than it counts; such a
//! sync stores what came back but keeps its start date, so the next one asks
//! for the same days again.

use chrono::{Datelike, Days, Local, NaiveDate, TimeZone};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::db;
use crate::github::{self, parse_timestamp};

const QUERY: &str = r#"
query($search: String!, $after: String) {
  search(query: $search, type: ISSUE, first: 100, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number title url mergedAt baseRefName headRefName
        repository { nameWithOwner }
      }
    }
  }
}
"#;

/// A week of merges fits in one page; this only bounds a runaway backfill.
const MAX_PAGES: usize = 10;
/// How far back the tab lists; older rows stay in the file.
const DAYS_SHOWN: u64 = 60;
/// Where the last successful sync is remembered, as `YYYY-MM-DD` local time.
const SYNCED_ON: &str = "synced_on";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merged {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `owner/name`.
    pub repo: String,
    pub base: String,
    pub head: String,
    /// Seconds since the Unix epoch.
    pub merged_at: i64,
}

impl Merged {
    pub fn repo_name(&self) -> &str {
        self.repo.rsplit('/').next().unwrap_or(&self.repo)
    }
}

/// Fetches what merged since the last sync and stores it.
pub fn sync() -> Result<(), String> {
    let db = open()?;
    let today = Local::now().date_naive();
    let since = synced_on(&db)?.unwrap_or_else(|| start_of_last_week(today));
    // GitHub reads search dates in UTC, so a day of margin keeps the merges
    // of a local evening that is already tomorrow in UTC.
    let (pulls, complete) = fetch_since(since - Days::new(1))?;

    let tx = db.unchecked_transaction().map_err(db_error)?;
    for pull in &pulls {
        tx.execute(
            "INSERT INTO merged (url, repo, number, title, base, head, merged_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(url) DO UPDATE SET title = excluded.title, merged_at = excluded.merged_at",
            params![
                pull.url,
                pull.repo,
                pull.number as i64,
                pull.title,
                pull.base,
                pull.head,
                pull.merged_at
            ],
        )
        .map_err(db_error)?;
    }
    if complete {
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SYNCED_ON, today.to_string()],
        )
        .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)
}

/// The stored merges of the last `DAYS_SHOWN` days, newest first.
pub fn load() -> Result<Vec<Merged>, String> {
    let db = open()?;
    let since = Local::now().timestamp() - DAYS_SHOWN as i64 * 86_400;
    let mut statement = db
        .prepare(
            "SELECT number, title, url, repo, base, head, merged_at FROM merged
             WHERE merged_at >= ?1 ORDER BY merged_at DESC",
        )
        .map_err(db_error)?;
    statement
        .query_map(params![since], |row| {
            Ok(Merged {
                number: row.get::<_, i64>(0)? as u64,
                title: row.get(1)?,
                url: row.get(2)?,
                repo: row.get(3)?,
                base: row.get(4)?,
                head: row.get(5)?,
                merged_at: row.get(6)?,
            })
        })
        .and_then(|rows| rows.collect())
        .map_err(db_error)
}

/// The local day a merge happened on.
pub fn day_of(timestamp: i64) -> NaiveDate {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.date_naive())
        .unwrap_or_default()
}

/// The local `HH:MM` a merge happened at.
pub fn time_of(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|time| time.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// "Hoy", "Ayer", or the weekday and date, such as "lunes 13 oct".
pub fn day_label(day: NaiveDate, today: NaiveDate) -> String {
    const WEEKDAYS: [&str; 7] = [
        "lunes",
        "martes",
        "miércoles",
        "jueves",
        "viernes",
        "sábado",
        "domingo",
    ];
    const MONTHS: [&str; 12] = [
        "ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sep", "oct", "nov", "dic",
    ];

    if day == today {
        "Hoy".to_string()
    } else if Some(day) == today.pred_opt() {
        "Ayer".to_string()
    } else {
        format!(
            "{} {} {}",
            WEEKDAYS[day.weekday().num_days_from_monday() as usize],
            day.day(),
            MONTHS[day.month0() as usize]
        )
    }
}

/// The Monday of the week before `today`'s, where the record starts.
fn start_of_last_week(today: NaiveDate) -> NaiveDate {
    let monday = today - Days::new(u64::from(today.weekday().num_days_from_monday()));
    monday - Days::new(7)
}

/// Every merge since `since`, and whether GitHub returned as many as it counts.
fn fetch_since(since: NaiveDate) -> Result<(Vec<Merged>, bool), String> {
    let search = format!("is:pr is:merged author:@me merged:>={since}");
    let mut pulls = Vec::new();
    let mut after: Option<String> = None;
    let mut count = 0;

    for _ in 0..MAX_PAGES {
        let mut args = vec![
            "api".to_string(),
            "graphql".to_string(),
            "-f".to_string(),
            format!("query={QUERY}"),
            "-f".to_string(),
            format!("search={search}"),
        ];
        if let Some(cursor) = &after {
            args.extend(["-f".to_string(), format!("after={cursor}")]);
        }
        let stdout = github::gh(&args)?;
        let value: Value = serde_json::from_slice(&stdout)
            .map_err(|error| format!("gh devolvió algo que no es JSON: {error}"))?;
        if let Some(message) = value["errors"][0]["message"].as_str() {
            return Err(format!("GitHub rechazó la consulta: {message}"));
        }

        let page = &value["data"]["search"];
        count = page["issueCount"].as_u64().unwrap_or(0) as usize;
        pulls.extend(
            page["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(parse_merged),
        );
        after = page["pageInfo"]["endCursor"].as_str().map(str::to_string);
        if page["pageInfo"]["hasNextPage"].as_bool() != Some(true) || after.is_none() {
            break;
        }
    }
    let complete = pulls.len() >= count;
    Ok((pulls, complete))
}

fn parse_merged(node: &Value) -> Option<Merged> {
    let text = |key: &str| node[key].as_str().unwrap_or_default().to_string();
    Some(Merged {
        number: node["number"].as_u64()?,
        title: text("title"),
        url: text("url"),
        repo: node["repository"]["nameWithOwner"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        base: text("baseRefName"),
        head: text("headRefName"),
        merged_at: node["mergedAt"].as_str().and_then(parse_timestamp)?,
    })
}

fn open() -> Result<Connection, String> {
    let db = db::open("merged.sqlite")?;
    migrate(&db)?;
    Ok(db)
}

fn migrate(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS merged (
             url TEXT PRIMARY KEY,
             repo TEXT NOT NULL,
             number INTEGER NOT NULL,
             title TEXT NOT NULL,
             base TEXT NOT NULL,
             head TEXT NOT NULL,
             merged_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS merged_by_time ON merged (merged_at);
         CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .map_err(db_error)
}

fn synced_on(db: &Connection) -> Result<Option<NaiveDate>, String> {
    let value: Option<String> = db
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![SYNCED_ON],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)?;
    Ok(value.and_then(|date| date.parse().ok()))
}

fn db_error(error: rusqlite::Error) -> String {
    format!("Error de la base local de mergeadas: {error}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn date(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    #[test]
    fn the_record_starts_on_the_monday_of_last_week() {
        assert_eq!(start_of_last_week(date("2026-10-21")), date("2026-10-12"));
        assert_eq!(start_of_last_week(date("2026-10-19")), date("2026-10-12"));
        assert_eq!(start_of_last_week(date("2026-10-25")), date("2026-10-12"));
    }

    #[test]
    fn days_read_as_today_yesterday_or_a_date() {
        let today = date("2026-10-21");

        assert_eq!(day_label(today, today), "Hoy");
        assert_eq!(day_label(date("2026-10-20"), today), "Ayer");
        assert_eq!(day_label(date("2026-10-13"), today), "martes 13 oct");
    }

    #[test]
    fn parses_a_merged_pull_request_and_skips_one_without_a_merge_time() {
        let merged = json!({
            "number": 7, "title": "Fix", "url": "https://github.com/acme/payments/pull/7",
            "mergedAt": "2026-10-20T17:46:59Z", "baseRefName": "master", "headRefName": "fix/x",
            "repository": { "nameWithOwner": "acme/payments" }
        });
        let unmerged = json!({ "number": 8, "mergedAt": null });

        let pull = parse_merged(&merged).unwrap();

        assert_eq!(pull.repo_name(), "payments");
        assert_eq!(
            pull.merged_at,
            parse_timestamp("2026-10-20T17:46:59Z").unwrap()
        );
        assert_eq!(parse_merged(&unmerged), None);
    }

    #[test]
    fn the_schema_stores_and_reads_back_a_merge() {
        let db = Connection::open_in_memory().unwrap();
        migrate(&db).unwrap();

        db.execute(
            "INSERT INTO meta (key, value) VALUES (?1, '2026-10-20')",
            params![SYNCED_ON],
        )
        .unwrap();

        assert_eq!(synced_on(&db).unwrap(), Some(date("2026-10-20")));
    }
}
