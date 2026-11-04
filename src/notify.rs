//! macOS notifications for news on the user's own PRs: each environment a
//! deploy reaches or breaks in, and a PR that becomes approved.
//!
//! Only transitions seen while the app is open notify: the first load sets the
//! baseline, so opening the app does not replay every old run or approval.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crate::github::{PullRequest, Review};
use crate::pipeline::{Env, Milestone};
use crate::tool;

/// The app's bundle identifier; must match `BUNDLE_ID` in `scripts/bundle.sh`.
const BUNDLE_ID: &str = "com.patriciogutierrez.gh-pr-status";

/// What was last seen of a PR.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    review: Review,
    pipeline: Option<RunSeen>,
}

/// A pipeline run: which bot comment, and the milestones it had reached.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RunSeen {
    comment: String,
    milestones: Vec<Milestone>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub subtitle: String,
    pub body: String,
    /// Opened when the notification is clicked, where the notifier allows it.
    pub url: String,
}

/// Compares the new load against what was seen before and returns the
/// notices to send, plus the new baseline.
pub fn transitions(
    previous: Option<&HashMap<String, Seen>>,
    pulls: &[PullRequest],
) -> (Vec<Notice>, HashMap<String, Seen>) {
    let mut notices = Vec::new();
    let mut seen = HashMap::new();

    for pull in pulls {
        let current = Seen {
            review: pull.review,
            pipeline: pull.pipeline.as_ref().map(|run| RunSeen {
                comment: run.url.clone(),
                milestones: run.milestones(),
            }),
        };

        if let Some(previous) = previous {
            let before = previous.get(&pull.url);

            let newly_approved = pull.review == Review::Approved
                && before.is_some_and(|before| before.review != Review::Approved);
            if newly_approved {
                notices.push(approval_notice(pull));
            }

            // Every milestone the run reached since the last poll is news. A new
            // run (a new bot comment) starts from nothing, so a run that started
            // and ended between polls still notifies.
            if let Some(run) = &current.pipeline {
                let already: &[Milestone] = before
                    .and_then(|before| before.pipeline.as_ref())
                    .filter(|seen| seen.comment == run.comment)
                    .map_or(&[], |seen| seen.milestones.as_slice());
                for milestone in run.milestones.iter().filter(|m| !already.contains(m)) {
                    notices.push(deploy_notice(pull, milestone));
                }
            }
        }

        seen.insert(pull.url.clone(), current);
    }

    (notices, seen)
}

fn subtitle(pull: &PullRequest) -> String {
    format!("{} #{}", pull.repo_name(), pull.number)
}

fn approval_notice(pull: &PullRequest) -> Notice {
    let body = if pull.approvers.is_empty() {
        pull.title.clone()
    } else {
        let by: Vec<String> = pull
            .approvers
            .iter()
            .map(|login| format!("@{login}"))
            .collect();
        format!("Por {}: {}", by.join(", "), pull.title)
    };

    Notice {
        title: "✓ PR aprobado".to_string(),
        subtitle: subtitle(pull),
        body,
        url: pull.url.clone(),
    }
}

fn deploy_notice(pull: &PullRequest, milestone: &Milestone) -> Notice {
    let (title, body) = match milestone {
        Milestone::Ready(Env::PreProd) => ("✓ Pre-prod listo".to_string(), pull.title.clone()),
        Milestone::Ready(env) => (
            format!("✓ Desplegado en {}", env.label()),
            pull.title.clone(),
        ),
        Milestone::Canary => ("🐦 Canary activo en prod".to_string(), pull.title.clone()),
        Milestone::Failed(env) => {
            let error = pull.pipeline.as_ref().and_then(|run| {
                run.steps
                    .iter()
                    .find(|step| step.kind == crate::pipeline::StepKind::Error && step.env == *env)
            });
            let service = error
                .and_then(|step| step.details.iter().find(|detail| detail.starts_with('📦')))
                .map(|detail| format!(" · {}", detail.trim_start_matches('📦').trim()))
                .unwrap_or_default();
            let what = error.map_or_else(String::new, |step| step.title.clone());
            let title = match env {
                Some(env) => format!("✗ Deploy falló en {}", env.label()),
                None => "✗ Pipeline detenido".to_string(),
            };
            (title, format!("{what}{service}"))
        }
    };

    Notice {
        title,
        subtitle: subtitle(pull),
        body,
        url: pull.url.clone(),
    }
}

/// Shows the notice. Blocks while the notifier runs, so call it off the UI
/// thread. Failing to notify is not worth surfacing in the window.
///
/// `osascript` is used only when `terminal-notifier` cannot run at all: its
/// notifications belong to Script Editor, so clicking one opens Script Editor.
pub fn send(notice: &Notice) {
    let message = if notice.body.is_empty() {
        &notice.title
    } else {
        &notice.body
    };
    let launched = Command::new(tool::path("terminal-notifier"))
        .args(["-title", &option_value(&notice.title)])
        .args(["-subtitle", &option_value(&notice.subtitle)])
        .args(["-message", &option_value(message)])
        .args(click_action(
            &notice.url,
            std::env::current_exe().ok().as_deref(),
        ))
        .args(["-sound", "Glass"])
        .status()
        .is_ok();
    if launched {
        return;
    }

    let script = format!(
        "display notification {} with title {} subtitle {} sound name \"Glass\"",
        applescript_string(&notice.body),
        applescript_string(&notice.title),
        applescript_string(&notice.subtitle),
    );
    let _ = Command::new(tool::path("osascript"))
        .args(["-e", &script])
        .status();
}

/// What clicking the notification does: bring the app forward when it runs as
/// the installed `.app`, or open the PR when it runs from `cargo run`, where
/// activating the bundle id would launch the installed copy instead.
fn click_action(url: &str, executable: Option<&Path>) -> [String; 2] {
    let bundled =
        executable.is_some_and(|path| path.to_string_lossy().contains(".app/Contents/MacOS/"));
    if bundled {
        ["-activate".to_string(), BUNDLE_ID.to_string()]
    } else {
        ["-open".to_string(), url.to_string()]
    }
}

/// Keeps `terminal-notifier` from reading a value that starts with `-` as an
/// option, which makes it exit without notifying. A zero-width space is invisible.
fn option_value(text: &str) -> String {
    if text.starts_with('-') {
        format!("\u{200B}{text}")
    } else {
        text.to_string()
    }
}

/// Quotes a value as an AppleScript string literal.
fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{Checks, Mergeable};
    use crate::pipeline::{Pipeline, Step, StepKind};

    const NOW: i64 = 10_000;

    fn step(title: &str, kind: StepKind, env: Option<Env>) -> Step {
        Step {
            title: title.to_string(),
            time: None,
            kind,
            env,
            details: vec!["📦 Servicio: payments-api".to_string()],
        }
    }

    fn pull(comment: &str, steps: Vec<Step>) -> PullRequest {
        PullRequest {
            number: 539,
            title: "Registrar la redirección".to_string(),
            url: "https://github.com/acme/payments/pull/539".to_string(),
            repo: "acme/payments".to_string(),
            author: String::new(),
            base: "master".to_string(),
            head: "feat/x".to_string(),
            draft: false,
            checks: Checks::Passing,
            failing_checks: Vec::new(),
            review: Review::Approved,
            approvers: vec!["ana".to_string()],
            requested_reviewers: Vec::new(),
            mergeable: Mergeable::Clean,
            comments: 0,
            updated_at: NOW,
            pipeline: Some(Pipeline {
                url: comment.to_string(),
                updated_at: NOW,
                steps,
            }),
        }
    }

    fn running(comment: &str) -> PullRequest {
        pull(comment, vec![step("🌟 CI: OK!", StepKind::Milestone, None)])
    }

    fn preprod_ready(comment: &str) -> PullRequest {
        pull(
            comment,
            vec![
                step("🌟 CI: OK!", StepKind::Milestone, None),
                step("🌟 CD: OK!", StepKind::Milestone, Some(Env::PreProd)),
            ],
        )
    }

    fn succeeded(comment: &str) -> PullRequest {
        pull(
            comment,
            vec![
                step("🌟 CD: OK!", StepKind::Milestone, Some(Env::PreProd)),
                step("🌟 CD: OK!", StepKind::Milestone, Some(Env::Prod)),
                step(
                    "🌟 CD: 🚀 El despliegue fue aplicado correctamente",
                    StepKind::Milestone,
                    Some(Env::Prod),
                ),
            ],
        )
    }

    fn failed(comment: &str) -> PullRequest {
        pull(
            comment,
            vec![step(
                "🚨 Error: 🚀 Falló el readiness check",
                StepKind::Error,
                Some(Env::PreProd),
            )],
        )
    }

    #[test]
    fn the_first_load_only_sets_the_baseline() {
        let (notices, seen) = transitions(None, &[failed("c1")]);

        assert!(notices.is_empty());
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn each_environment_notifies_once_as_the_run_reaches_it() {
        let (_, baseline) = transitions(None, &[running("c1")]);

        let (preprod, seen) = transitions(Some(&baseline), &[preprod_ready("c1")]);
        let (prod, seen) = transitions(Some(&seen), &[succeeded("c1")]);
        let (again, _) = transitions(Some(&seen), &[succeeded("c1")]);

        let titles =
            |notices: &[Notice]| notices.iter().map(|n| n.title.clone()).collect::<Vec<_>>();
        assert_eq!(titles(&preprod), ["✓ Pre-prod listo"]);
        assert_eq!(titles(&prod), ["✓ Desplegado en prod"]);
        assert!(again.is_empty());
    }

    #[test]
    fn a_failure_names_the_error_and_the_service() {
        let (_, baseline) = transitions(None, &[running("c1")]);

        let (notices, _) = transitions(Some(&baseline), &[failed("c1")]);

        assert_eq!(notices[0].subtitle, "payments #539");
        assert_eq!(notices[0].title, "✗ Deploy falló en pre-prod");
        assert_eq!(
            notices[0].body,
            "🚨 Error: 🚀 Falló el readiness check · Servicio: payments-api"
        );
    }

    #[test]
    fn a_new_run_that_ends_between_polls_still_notifies() {
        let (_, baseline) = transitions(None, &[succeeded("c1")]);

        let (notices, _) = transitions(Some(&baseline), &[failed("c2")]);

        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn a_run_blocked_before_deploying_says_so() {
        let blocked = pull(
            "c1",
            vec![step("🚨 Error: 🚩 Faltan approves", StepKind::Error, None)],
        );
        let (_, baseline) = transitions(None, &[running("c1")]);

        let (notices, _) = transitions(Some(&baseline), &[blocked]);

        assert_eq!(notices[0].title, "✗ Pipeline detenido");
    }

    #[test]
    fn a_run_still_in_progress_does_not_notify() {
        let (_, baseline) = transitions(None, &[running("c1")]);

        let (notices, _) = transitions(Some(&baseline), &[running("c1")]);

        assert!(notices.is_empty());
    }

    fn reviewed(review: Review) -> PullRequest {
        PullRequest {
            review,
            pipeline: None,
            ..running("c1")
        }
    }

    #[test]
    fn an_approval_notifies_once_with_who_approved() {
        let (_, baseline) = transitions(None, &[reviewed(Review::Required)]);

        let (notices, seen) = transitions(Some(&baseline), &[reviewed(Review::Approved)]);
        let (again, _) = transitions(Some(&seen), &[reviewed(Review::Approved)]);

        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].title, "✓ PR aprobado");
        assert_eq!(notices[0].body, "Por @ana: Registrar la redirección");
        assert!(again.is_empty());
    }

    #[test]
    fn a_pr_already_approved_when_first_seen_does_not_notify() {
        let (_, baseline) = transitions(None, &[]);

        let (notices, _) = transitions(Some(&baseline), &[reviewed(Review::Approved)]);

        assert!(notices.is_empty());
    }

    #[test]
    fn a_click_brings_the_installed_app_forward() {
        let executable =
            Path::new("/Users/me/Applications/GH PR Status.app/Contents/MacOS/gh-pr-status");

        let action = click_action("https://github.com/x/pull/1", Some(executable));

        assert_eq!(action, ["-activate", BUNDLE_ID]);
    }

    #[test]
    fn a_click_opens_the_pr_when_not_bundled() {
        let executable = Path::new("/Users/me/code/gh-pr-status/target/debug/gh-pr-status");

        let action = click_action("https://github.com/x/pull/1", Some(executable));

        assert_eq!(action, ["-open", "https://github.com/x/pull/1"]);
    }

    #[test]
    fn a_leading_dash_is_not_read_as_an_option() {
        assert_eq!(option_value("-fix: algo"), "\u{200B}-fix: algo");
        assert_eq!(option_value("fix: -algo"), "fix: -algo");
    }

    #[test]
    fn quotes_are_escaped_for_applescript() {
        assert_eq!(
            applescript_string(r#"say "hi" \o/"#),
            r#""say \"hi\" \\o/""#
        );
    }
}
