//! Pull requests read through the `gh` CLI.
//!
//! Going through `gh` means the app reuses the login the user already has and
//! never stores or sends a token itself. Everything here blocks on purpose:
//! the view hands these calls to the background executor, which is why nothing
//! in this module touches GPUI.

use std::collections::HashSet;
use std::process::Command;

use serde_json::Value;

use crate::pipeline::{Pipeline, Settings};
use crate::tool;

/// One search, one page. Fifty is more open PRs than anyone should have.
///
/// The latest comments carry the pipeline bot's report, but only the user's own
/// PRs need them: on other people's PRs those comments run to hundreds of KB and
/// turn a two-second search into a ten-second one.
const QUERY: &str = r#"
query($search: String!, $withComments: Boolean!) {
  viewer { login }
  search(query: $search, type: ISSUE, first: 50) {
    nodes {
      ... on PullRequest {
        number title url isDraft updatedAt baseRefName headRefName
        mergeable reviewDecision
        author { login }
        repository { nameWithOwner }
        comments { totalCount }
        recentComments: comments(last: 10) @include(if: $withComments) {
          nodes { url updatedAt author { login } body }
        }
        latestReviews(first: 20) { nodes { state author { login } } }
        reviewRequests(first: 10) {
          nodes { requestedReviewer { ... on User { login } ... on Team { name } } }
        }
        commits(last: 1) {
          nodes {
            commit {
              statusCheckRollup {
                state
                contexts(first: 50) {
                  nodes {
                    ... on CheckRun {
                      name conclusion detailsUrl title
                      annotations(first: 10) @include(if: $withComments) { nodes { message annotationLevel } }
                    }
                    ... on StatusContext { context state targetUrl description }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
"#;

/// Which list a tab shows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Search {
    /// Open PRs the user authored.
    Mine,
    /// Open PRs where someone asked the user for a review.
    ReviewRequested,
}

impl Search {
    fn query(self) -> &'static str {
        match self {
            Search::Mine => "is:pr is:open archived:false author:@me sort:updated-desc",
            Search::ReviewRequested => {
                "is:pr is:open archived:false review-requested:@me sort:updated-desc"
            }
        }
    }

    /// Whether the search also brings the pipeline bot's comments.
    fn with_comments(self) -> bool {
        self == Search::Mine
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Checks {
    Passing,
    Failing,
    Pending,
    /// The repo runs no checks on this commit.
    Missing,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Review {
    Approved,
    ChangesRequested,
    /// Branch protection asks for a review and there is none yet.
    Required,
    /// No protection rule and no reviews.
    Missing,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mergeable {
    Clean,
    Conflicting,
    /// GitHub computes this lazily; right after a push it is not known yet.
    Unknown,
}

#[derive(Clone, Debug)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `owner/name`.
    pub repo: String,
    pub author: String,
    pub base: String,
    pub head: String,
    pub draft: bool,
    pub checks: Checks,
    pub failing_checks: Vec<FailedCheck>,
    pub review: Review,
    /// Logins whose latest review approves the PR.
    pub approvers: Vec<String>,
    /// Users and teams asked for a review who have not given it yet.
    pub requested_reviewers: Vec<String>,
    pub mergeable: Mergeable,
    pub comments: u64,
    /// Seconds since the Unix epoch.
    pub updated_at: i64,
    /// The latest CI/CD run reported by the bot; only fetched for the user's PRs.
    pub pipeline: Option<Pipeline>,
}

impl PullRequest {
    /// The repo name without the owner, which is the same for almost every row.
    pub fn repo_name(&self) -> &str {
        self.repo.rsplit('/').next().unwrap_or(&self.repo)
    }
}

/// A check that failed on the PR's last commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedCheck {
    pub name: String,
    /// Why it failed, when the check says more than its exit code.
    pub reason: Option<String>,
    /// Where its logs are.
    pub url: Option<String>,
}

pub struct Snapshot {
    pub login: String,
    pub pulls: Vec<PullRequest>,
}

/// Runs the search through `gh api graphql`.
pub fn fetch(search: Search) -> Result<Snapshot, String> {
    let stdout = gh(&[
        "api".to_string(),
        "graphql".to_string(),
        "-f".to_string(),
        format!("query={QUERY}"),
        "-f".to_string(),
        format!("search={}", search.query()),
        "-F".to_string(),
        format!("withComments={}", search.with_comments()),
    ])?;

    let value: Value = serde_json::from_slice(&stdout)
        .map_err(|error| format!("gh devolvió algo que no es JSON: {error}"))?;
    parse(&value)
}

/// Posts `body` as a comment on the PR at `url`, as the user `gh` is logged in as.
pub fn comment(url: &str, body: &str) -> Result<(), String> {
    gh(&[
        "pr".to_string(),
        "comment".to_string(),
        url.to_string(),
        "--body".to_string(),
        body.to_string(),
    ])
    .map(|_| ())
}

/// Runs `gh` and returns its stdout, or an error worth showing as is.
fn gh(args: &[String]) -> Result<Vec<u8>, String> {
    let output = Command::new(tool::path("gh"))
        .args(args)
        .output()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                "No se encontró el comando gh. Instálalo con `brew install gh` y corre `gh auth login`."
                    .to_string()
            }
            _ => format!("No se pudo ejecutar gh: {error}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh terminó con error: {}", stderr.trim()));
    }
    Ok(output.stdout)
}

fn parse(value: &Value) -> Result<Snapshot, String> {
    if let Some(message) = value["errors"][0]["message"].as_str() {
        return Err(format!("GitHub rechazó la consulta: {message}"));
    }

    let data = &value["data"];
    let pulls = data["search"]["nodes"]
        .as_array()
        .map(|nodes| nodes.iter().filter_map(parse_pull).collect())
        .unwrap_or_default();

    Ok(Snapshot {
        login: data["viewer"]["login"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        pulls,
    })
}

/// `None` for a search hit that is not a pull request (an empty node).
fn parse_pull(node: &Value) -> Option<PullRequest> {
    let number = node["number"].as_u64()?;
    let text = |key: &str| node[key].as_str().unwrap_or_default().to_string();
    let rollup = &node["commits"]["nodes"][0]["commit"]["statusCheckRollup"];

    Some(PullRequest {
        number,
        title: text("title"),
        url: text("url"),
        repo: node["repository"]["nameWithOwner"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        author: node["author"]["login"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        base: text("baseRefName"),
        head: text("headRefName"),
        draft: node["isDraft"].as_bool().unwrap_or(false),
        checks: parse_checks(rollup),
        failing_checks: failing_checks(rollup),
        review: parse_review(node),
        approvers: approvers(node),
        requested_reviewers: requested_reviewers(node),
        mergeable: match node["mergeable"].as_str() {
            Some("MERGEABLE") => Mergeable::Clean,
            Some("CONFLICTING") => Mergeable::Conflicting,
            _ => Mergeable::Unknown,
        },
        comments: node["comments"]["totalCount"].as_u64().unwrap_or(0),
        updated_at: node["updatedAt"]
            .as_str()
            .and_then(parse_timestamp)
            .unwrap_or(0),
        pipeline: Pipeline::latest(&node["recentComments"], Settings::get()),
    })
}

fn parse_checks(rollup: &Value) -> Checks {
    match rollup["state"].as_str() {
        Some("SUCCESS") => Checks::Passing,
        Some("FAILURE" | "ERROR") => Checks::Failing,
        Some("PENDING" | "EXPECTED") => Checks::Pending,
        _ => Checks::Missing,
    }
}

/// Each failed check once: a re-run check shows up again under the same name.
fn failing_checks(rollup: &Value) -> Vec<FailedCheck> {
    let Some(contexts) = rollup["contexts"]["nodes"].as_array() else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    contexts
        .iter()
        .filter_map(|context| {
            let failed = matches!(
                context["conclusion"].as_str(),
                Some("FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE")
            ) || matches!(context["state"].as_str(), Some("FAILURE" | "ERROR"));
            let name = context["name"].as_str().or(context["context"].as_str())?;
            (failed && seen.insert(name)).then(|| FailedCheck {
                name: name.to_string(),
                reason: failure_reason(context),
                url: context["detailsUrl"]
                    .as_str()
                    .or(context["targetUrl"].as_str())
                    .filter(|url| !url.is_empty())
                    .map(str::to_string),
            })
        })
        .collect()
}

/// The first failure annotation that is not GitHub Actions' "Process completed
/// with exit code N.", else the check's own title or description. Warnings and
/// notices are about the runner, not about what broke.
fn failure_reason(context: &Value) -> Option<String> {
    let annotations = context["annotations"]["nodes"]
        .as_array()
        .into_iter()
        .flatten();
    annotations
        .filter(|annotation| annotation["annotationLevel"].as_str() == Some("FAILURE"))
        .filter_map(|annotation| annotation["message"].as_str())
        .find(|message| !message.starts_with("Process completed with exit code"))
        .or(context["title"].as_str())
        .or(context["description"].as_str())
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        .map(str::to_string)
}

fn approvers(node: &Value) -> Vec<String> {
    let Some(reviews) = node["latestReviews"]["nodes"].as_array() else {
        return Vec::new();
    };

    reviews
        .iter()
        .filter(|review| review["state"].as_str() == Some("APPROVED"))
        .filter_map(|review| review["author"]["login"].as_str().map(str::to_string))
        .collect()
}

fn requested_reviewers(node: &Value) -> Vec<String> {
    let Some(requests) = node["reviewRequests"]["nodes"].as_array() else {
        return Vec::new();
    };

    requests
        .iter()
        .filter_map(|request| {
            let reviewer = &request["requestedReviewer"];
            reviewer["login"].as_str().or(reviewer["name"].as_str())
        })
        .map(str::to_string)
        .collect()
}

/// `reviewDecision` is only set when branch protection requires reviews; without
/// it, fall back to what the reviewers actually said.
fn parse_review(node: &Value) -> Review {
    match node["reviewDecision"].as_str() {
        Some("APPROVED") => return Review::Approved,
        Some("CHANGES_REQUESTED") => return Review::ChangesRequested,
        Some("REVIEW_REQUIRED") => return Review::Required,
        _ => {}
    }

    let states: Vec<&str> = node["latestReviews"]["nodes"]
        .as_array()
        .map(|reviews| reviews.iter().filter_map(|r| r["state"].as_str()).collect())
        .unwrap_or_default();

    if states.contains(&"CHANGES_REQUESTED") {
        Review::ChangesRequested
    } else if states.contains(&"APPROVED") {
        Review::Approved
    } else {
        Review::Missing
    }
}

/// Parses GitHub's `2026-10-05T17:46:59Z` into Unix seconds. GitHub always
/// answers in UTC, so the `Z` form is the only one handled.
pub fn parse_timestamp(text: &str) -> Option<i64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let mut time = time.splitn(3, ':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// algorithm), which spares a date crate for a single conversion.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn node(extra: Value) -> Value {
        let mut base = json!({
            "number": 539,
            "title": "Registrar la redirección",
            "url": "https://github.com/acme/payments/pull/539",
            "isDraft": false,
            "updatedAt": "2026-10-05T17:59:10Z",
            "baseRefName": "master",
            "headRefName": "feat/itau-log-redirect-location",
            "mergeable": "MERGEABLE",
            "reviewDecision": null,
            "author": { "login": "patriciogutierrez" },
            "repository": { "nameWithOwner": "acme/payments" },
            "comments": { "totalCount": 2 },
            "latestReviews": { "nodes": [] },
            "commits": { "nodes": [{ "commit": { "statusCheckRollup": null } }] }
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    #[test]
    fn parses_a_pull_request_node() {
        let pull = parse_pull(&node(json!({}))).unwrap();

        assert_eq!(pull.repo_name(), "payments");
        assert_eq!(pull.checks, Checks::Missing);
        assert_eq!(pull.review, Review::Missing);
        assert_eq!(pull.mergeable, Mergeable::Clean);
    }

    #[test]
    fn collects_the_names_of_failing_checks() {
        let rollup = json!({ "state": "FAILURE", "contexts": { "nodes": [
            { "name": "tests", "conclusion": "FAILURE" },
            { "name": "lint", "conclusion": "SUCCESS" },
            { "context": "deploy", "state": "ERROR" }
        ]}});

        let pull = parse_pull(&node(
            json!({ "commits": { "nodes": [{ "commit": { "statusCheckRollup": rollup } }] } }),
        ))
        .unwrap();

        let names: Vec<_> = pull
            .failing_checks
            .iter()
            .map(|check| check.name.as_str())
            .collect();

        assert_eq!(pull.checks, Checks::Failing);
        assert_eq!(names, vec!["tests", "deploy"]);
    }

    #[test]
    fn a_failed_check_explains_itself_past_the_exit_code() {
        let rollup = json!({ "state": "FAILURE", "contexts": { "nodes": [
            {
                "name": "tests",
                "conclusion": "FAILURE",
                "detailsUrl": "https://github.com/acme/payments/actions/runs/1",
                "annotations": { "nodes": [
                    { "message": "The ubuntu-latest label will migrate", "annotationLevel": "WARNING" },
                    { "message": "Process completed with exit code 1.", "annotationLevel": "FAILURE" },
                    { "message": "\nAssertionError: expected 2\nmore", "annotationLevel": "FAILURE" }
                ]}
            },
            { "name": "tests", "conclusion": "FAILURE" },
            { "context": "deploy", "state": "ERROR", "description": "Build failed", "targetUrl": "" }
        ]}});

        let pull = parse_pull(&node(
            json!({ "commits": { "nodes": [{ "commit": { "statusCheckRollup": rollup } }] } }),
        ))
        .unwrap();

        assert_eq!(
            pull.failing_checks,
            vec![
                FailedCheck {
                    name: "tests".to_string(),
                    reason: Some("AssertionError: expected 2".to_string()),
                    url: Some("https://github.com/acme/payments/actions/runs/1".to_string()),
                },
                FailedCheck {
                    name: "deploy".to_string(),
                    reason: Some("Build failed".to_string()),
                    url: None,
                },
            ]
        );
    }

    #[test]
    fn falls_back_to_reviews_when_there_is_no_review_decision() {
        let reviews =
            json!({ "nodes": [{ "state": "APPROVED" }, { "state": "CHANGES_REQUESTED" }] });

        let pull = parse_pull(&node(json!({ "latestReviews": reviews }))).unwrap();

        assert_eq!(pull.review, Review::ChangesRequested);
    }

    #[test]
    fn lists_who_approved() {
        let reviews = json!({ "nodes": [
            { "state": "APPROVED", "author": { "login": "ana" } },
            { "state": "COMMENTED", "author": { "login": "beto" } }
        ]});

        let pull = parse_pull(&node(json!({ "latestReviews": reviews }))).unwrap();

        assert_eq!(pull.approvers, ["ana"]);
    }

    #[test]
    fn lists_who_still_has_to_review() {
        let requests = json!({ "nodes": [
            { "requestedReviewer": { "login": "carla" } },
            { "requestedReviewer": { "name": "payments-cl" } },
            { "requestedReviewer": {} }
        ]});

        let pull = parse_pull(&node(json!({ "reviewRequests": requests }))).unwrap();

        assert_eq!(pull.requested_reviewers, ["carla", "payments-cl"]);
    }

    #[test]
    fn surfaces_graphql_errors() {
        let response = json!({ "errors": [{ "message": "Bad credentials" }] });

        let result = parse(&response);

        assert_eq!(
            result.err().unwrap(),
            "GitHub rechazó la consulta: Bad credentials"
        );
    }

    #[test]
    fn skips_search_hits_that_are_not_pull_requests() {
        let response = json!({ "data": { "viewer": { "login": "me" }, "search": { "nodes": [{}, node(json!({}))] } } });

        let snapshot = parse(&response).unwrap();

        assert_eq!(snapshot.pulls.len(), 1);
    }

    #[test]
    fn parses_timestamps_to_unix_seconds() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("2026-10-05T17:46:59Z"), Some(1_791_222_419));
        assert_eq!(parse_timestamp("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(parse_timestamp("not a date"), None);
    }

    /// Goes to GitHub for real, so it only runs with `GH_PR_STATUS_LIVE=1`.
    #[test]
    fn live_search_returns_the_viewer() {
        if std::env::var("GH_PR_STATUS_LIVE").as_deref() != Ok("1") {
            return;
        }

        let snapshot = fetch(Search::Mine).unwrap();

        assert!(!snapshot.login.is_empty());
        assert!(snapshot.pulls.iter().all(|pull| pull.updated_at > 0));
    }
}
