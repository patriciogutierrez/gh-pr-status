//! Reading Slack as the user, with the user token in `GH_PR_STATUS_SLACK_TOKEN`:
//! finding the message the review workflow posted for a PR, and counting the
//! replies under it. Everything here blocks and runs on the background
//! executor.

use std::env;
use std::process::Command;

use serde_json::Value;

use crate::tool;

/// How many of the channel's latest messages are searched for the request.
const HISTORY_LIMIT: &str = "50";

/// The message the workflow posted: where it is and its link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub ts: String,
    pub permalink: String,
}

/// The user token, when configured.
pub fn token() -> Option<String> {
    env::var("GH_PR_STATUS_SLACK_TOKEN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The latest message in `channel` that mentions `needle` (the PR's URL),
/// with its permalink; `None` when the workflow hasn't posted yet.
pub fn find_message(token: &str, channel: &str, needle: &str) -> Result<Option<Message>, String> {
    let history = call(
        token,
        "conversations.history",
        &[("channel", channel), ("limit", HISTORY_LIMIT)],
    )?;
    let Some(ts) = message_with(&history, needle) else {
        return Ok(None);
    };
    let link = call(
        token,
        "chat.getPermalink",
        &[("channel", channel), ("message_ts", &ts)],
    )?;
    let permalink = link["permalink"]
        .as_str()
        .ok_or("Slack no devolvió el permalink del mensaje")?
        .to_string();
    Ok(Some(Message { ts, permalink }))
}

/// How many replies the thread under `ts` has.
pub fn replies(token: &str, channel: &str, ts: &str) -> Result<u64, String> {
    let thread = call(
        token,
        "conversations.replies",
        &[("channel", channel), ("ts", ts), ("limit", "200")],
    )?;
    Ok(reply_count(&thread))
}

/// The `ts` of the newest message whose text, anywhere in it, has `needle`.
fn message_with(history: &Value, needle: &str) -> Option<String> {
    history["messages"]
        .as_array()?
        .iter()
        .find(|message| message.to_string().contains(needle))
        .and_then(|message| message["ts"].as_str())
        .map(str::to_string)
}

/// The messages of a thread minus its root.
fn reply_count(thread: &Value) -> u64 {
    thread["messages"]
        .as_array()
        .map_or(0, |messages| messages.len().saturating_sub(1) as u64)
}

/// Calls a Slack Web API method with the user token.
fn call(token: &str, method: &str, query: &[(&str, &str)]) -> Result<Value, String> {
    let url = format!("https://slack.com/api/{method}");
    let mut command = Command::new(tool::path("curl"));
    command
        .args(["-sS", "--max-time", "15", "-G", &url])
        .args(["-H", &format!("Authorization: Bearer {token}")]);
    for (key, value) in query {
        command.args(["--data-urlencode", &format!("{key}={value}")]);
    }
    let output = command
        .output()
        .map_err(|error| format!("No se pudo ejecutar curl: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("No se pudo llegar a Slack: {}", stderr.trim()));
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Slack respondió algo que no es JSON".to_string())?;
    if value["ok"].as_bool() == Some(true) {
        Ok(value)
    } else {
        Err(format!(
            "Slack rechazó {method}: {}",
            value["error"].as_str().unwrap_or("sin detalle")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn the_newest_message_mentioning_the_pull_request_is_found() {
        let history = json!({ "messages": [
            { "ts": "3.0", "text": "otra cosa" },
            { "ts": "2.0", "text": "Review: <https://github.com/acme/demo/pull/7|PR>", "blocks": [] },
            { "ts": "1.0", "blocks": [{ "text": { "text": "https://github.com/acme/demo/pull/7" } }] },
        ] });

        assert_eq!(
            message_with(&history, "https://github.com/acme/demo/pull/7").as_deref(),
            Some("2.0")
        );
        assert_eq!(message_with(&history, "pull/8"), None);
    }

    #[test]
    fn replies_are_the_thread_minus_its_root() {
        let thread = json!({ "messages": [{ "ts": "1.0" }, { "ts": "1.1" }, { "ts": "1.2" }] });

        assert_eq!(reply_count(&thread), 2);
        assert_eq!(reply_count(&json!({ "messages": [{ "ts": "1.0" }] })), 0);
    }
}
