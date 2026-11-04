//! What a pull request's state means for the person looking at it: which
//! section it belongs to and the short labels its row shows.

use crate::github::{Checks, Mergeable, PullRequest, Review};
use crate::pipeline::{self, StepKind};
use crate::theme;

/// Sections of the "Mis PRs" tab, in the order they are drawn.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Group {
    /// Something only the author can fix: red CI, requested changes, conflicts,
    /// a failed deploy.
    Attention,
    /// Approved, green and mergeable.
    Ready,
    /// Waiting on CI or on a reviewer.
    Waiting,
    Draft,
}

impl Group {
    pub const ALL: [Group; 4] = [Group::Attention, Group::Ready, Group::Waiting, Group::Draft];

    pub fn of(pull: &PullRequest, now: i64) -> Group {
        let deploy_failed = pull
            .pipeline
            .as_ref()
            .is_some_and(|run| run.state(now) == pipeline::State::Failed);

        if pull.draft {
            Group::Draft
        } else if pull.checks == Checks::Failing
            || pull.review == Review::ChangesRequested
            || pull.mergeable == Mergeable::Conflicting
            || deploy_failed
        {
            Group::Attention
        } else if pull.review == Review::Approved
            && matches!(pull.checks, Checks::Passing | Checks::Missing)
            && pull.mergeable == Mergeable::Clean
        {
            Group::Ready
        } else {
            Group::Waiting
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Group::Attention => "Necesitan tu atención",
            Group::Ready => "Listos para merge",
            Group::Waiting => "Esperando CI o review",
            Group::Draft => "Borradores",
        }
    }

    /// The group's name in the one-line summary above the list.
    pub fn short(self) -> &'static str {
        match self {
            Group::Attention => "atención",
            Group::Ready => "listos",
            Group::Waiting => "esperando",
            Group::Draft => "borradores",
        }
    }

    pub fn color(self) -> u32 {
        match self {
            Group::Attention => theme::RED,
            Group::Ready => theme::GREEN,
            Group::Waiting => theme::YELLOW,
            Group::Draft => theme::MUTED,
        }
    }
}

/// What the PR waits for next: the one verb its row leads with, and the task
/// for the group's agent when it is something an agent can do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub label: String,
    pub color: u32,
    pub task: Option<String>,
}

/// The next action on the PR, by what blocks it first: a failed deploy,
/// conflicts, red CI, requested changes, open review threads; then what it
/// waits on, starting with the PR it is stacked on (`below`); then merging.
pub fn action(pull: &PullRequest, below: Option<&PullRequest>, now: i64) -> Action {
    let act = |label: String, color: u32, task: Option<String>| Action { label, color, task };
    let names = |logins: &[String]| {
        logins
            .iter()
            .map(|login| format!("@{login}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let failing = || {
        let checks: Vec<_> = pull
            .failing_checks
            .iter()
            .map(|check| check.name.as_str())
            .collect();
        if checks.is_empty() {
            "Arreglar CI".to_string()
        } else {
            format!("Arreglar CI: {}", checks.join(", "))
        }
    };
    let deploy = pull.pipeline.as_ref().map(|run| run.state(now));

    if pull.draft {
        return if pull.checks == Checks::Failing {
            let label = failing();
            act(
                format!("{label} antes de sacarlo de borrador"),
                theme::MUTED,
                Some(label),
            )
        } else if let Some(below) = below {
            act(
                format!("Esperando el merge de #{} (borrador)", below.number),
                theme::MUTED,
                None,
            )
        } else {
            act("Sacar de borrador".to_string(), theme::MUTED, None)
        };
    }
    if deploy == Some(pipeline::State::Failed) {
        let step = pull
            .pipeline
            .as_ref()
            .and_then(|run| run.headline())
            .map(|step| format!(": {}", step.title))
            .unwrap_or_default();
        let label = format!("Revisar el deploy fallido{step}");
        return act(label.clone(), theme::RED, Some(label));
    }
    if pull.mergeable == Mergeable::Conflicting {
        let label = format!("Resolver conflictos con {}", pull.base);
        return act(label.clone(), theme::RED, Some(label));
    }
    if pull.checks == Checks::Failing {
        let label = failing();
        return act(label.clone(), theme::RED, Some(label));
    }
    if pull.review == Review::ChangesRequested {
        let label = "Atender los cambios pedidos en la review".to_string();
        return act(label.clone(), theme::RED, Some(label));
    }
    if let Some(below) = below {
        return act(
            format!("Esperando el merge de #{}", below.number),
            theme::YELLOW,
            None,
        );
    }
    if !pull.threads.is_empty() {
        let label = match pull.threads.len() {
            1 => "Responder 1 hilo de review".to_string(),
            n => format!("Responder {n} hilos de review"),
        };
        return act(label.clone(), theme::RED, Some(label));
    }
    if pull.checks == Checks::Pending {
        return act("Esperando CI".to_string(), theme::YELLOW, None);
    }
    if deploy == Some(pipeline::State::Running) {
        return act("Desplegando".to_string(), theme::YELLOW, None);
    }
    if pull.review != Review::Approved && !pull.requested_reviewers.is_empty() {
        return act(
            format!("Esperando review de {}", names(&pull.requested_reviewers)),
            theme::YELLOW,
            None,
        );
    }
    if pull.review == Review::Required {
        return act("Pedir review".to_string(), theme::YELLOW, None);
    }
    if pull.mergeable == Mergeable::Unknown {
        return act("Esperando a GitHub".to_string(), theme::MUTED, None);
    }
    act("Mergear".to_string(), theme::GREEN, None)
}

/// A short status label and its color.
pub struct Badge {
    pub label: String,
    pub color: u32,
}

/// The badges for a row, in a fixed order so the eye finds CI, review and
/// conflicts in the same place on every row.
pub fn badges(pull: &PullRequest) -> Vec<Badge> {
    let mut badges = Vec::new();
    let mut push = |label: &str, color: u32| {
        badges.push(Badge {
            label: label.to_string(),
            color,
        })
    };
    let names = |logins: &[String]| {
        logins
            .iter()
            .map(|login| format!("@{login}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    match pull.checks {
        Checks::Passing => push("✓ CI", theme::GREEN),
        Checks::Failing => push("✗ CI", theme::RED),
        Checks::Pending => push("● CI corriendo", theme::YELLOW),
        Checks::Missing => {}
    }
    match pull.review {
        Review::Approved if !pull.approvers.is_empty() => push(
            &format!("Aprobado por {}", names(&pull.approvers)),
            theme::GREEN,
        ),
        Review::Approved => push("Aprobado", theme::GREEN),
        Review::ChangesRequested => push("Cambios pedidos", theme::RED),
        Review::Required | Review::Missing if !pull.requested_reviewers.is_empty() => push(
            &format!("Esperando a {}", names(&pull.requested_reviewers)),
            theme::YELLOW,
        ),
        Review::Required => push("Falta review", theme::YELLOW),
        Review::Missing => {}
    }
    if pull.mergeable == Mergeable::Conflicting {
        push("Conflictos", theme::RED);
    }
    if pull.draft {
        push("Borrador", theme::MUTED);
    }

    badges
}

/// Label and color for a pipeline run.
pub fn pipeline_state(state: pipeline::State) -> Badge {
    let (label, color) = match state {
        pipeline::State::Running => ("● Desplegando", theme::YELLOW),
        pipeline::State::Succeeded => ("✓ Desplegado", theme::GREEN),
        pipeline::State::Failed => ("✗ Deploy falló", theme::RED),
        pipeline::State::Stalled => ("◌ Sin avances", theme::MUTED),
    };
    Badge {
        label: label.to_string(),
        color,
    }
}

/// `pre-prod ✓`, `prod ●`: where each environment of the run stands.
pub fn environment(env: pipeline::Env, state: pipeline::EnvState) -> Badge {
    let (mark, color) = match state {
        pipeline::EnvState::Deploying => ("…", theme::SUBTEXT),
        pipeline::EnvState::Live => ("●", theme::YELLOW),
        pipeline::EnvState::Ready => ("✓", theme::GREEN),
        pipeline::EnvState::Failed => ("✗", theme::RED),
    };
    Badge {
        label: format!("{} {mark}", env.label()),
        color,
    }
}

pub fn step_color(kind: StepKind) -> u32 {
    match kind {
        StepKind::Milestone => theme::GREEN,
        StepKind::Error => theme::RED,
        StepKind::Info => theme::SUBTEXT,
    }
}

/// `hace 5 min`, `hace 3 h`, `hace 2 d`.
pub fn ago(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..60 => "recién".to_string(),
        60..3_600 => format!("hace {} min", seconds / 60),
        3_600..86_400 => format!("hace {} h", seconds / 3_600),
        _ => format!("hace {} d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pull() -> PullRequest {
        PullRequest {
            number: 1,
            title: String::new(),
            url: String::new(),
            repo: "acme/demo".to_string(),
            author: String::new(),
            base: "master".to_string(),
            head: "feat/x".to_string(),
            draft: false,
            checks: Checks::Passing,
            failing_checks: Vec::new(),
            review: Review::Approved,
            approvers: Vec::new(),
            requested_reviewers: Vec::new(),
            mergeable: Mergeable::Clean,
            comments: 0,
            threads: Vec::new(),
            updated_at: 0,
            pipeline: None,
        }
    }

    #[test]
    fn approved_green_and_clean_is_ready() {
        assert_eq!(Group::of(&pull(), 0), Group::Ready);
    }

    #[test]
    fn anything_the_author_must_fix_needs_attention() {
        let red_ci = PullRequest {
            checks: Checks::Failing,
            ..pull()
        };
        let changes = PullRequest {
            review: Review::ChangesRequested,
            ..pull()
        };
        let conflicts = PullRequest {
            mergeable: Mergeable::Conflicting,
            ..pull()
        };

        for pull in [red_ci, changes, conflicts] {
            assert_eq!(Group::of(&pull, 0), Group::Attention);
        }
    }

    #[test]
    fn pending_ci_or_missing_review_is_waiting() {
        let pending = PullRequest {
            checks: Checks::Pending,
            ..pull()
        };
        let unreviewed = PullRequest {
            review: Review::Required,
            ..pull()
        };
        let unknown = PullRequest {
            mergeable: Mergeable::Unknown,
            ..pull()
        };

        for pull in [pending, unreviewed, unknown] {
            assert_eq!(Group::of(&pull, 0), Group::Waiting);
        }
    }

    #[test]
    fn a_failed_deploy_needs_attention() {
        let run = pipeline::Pipeline {
            url: String::new(),
            updated_at: 0,
            steps: vec![pipeline::Step {
                title: "🚨 Error: 🚀 Falló el readiness check".to_string(),
                time: None,
                kind: StepKind::Error,
                env: Some(pipeline::Env::PreProd),
                details: Vec::new(),
            }],
        };
        let failed = PullRequest {
            pipeline: Some(run),
            ..pull()
        };

        assert_eq!(Group::of(&failed, 0), Group::Attention);
    }

    #[test]
    fn a_draft_is_a_draft_even_when_ci_fails() {
        let draft = PullRequest {
            draft: true,
            checks: Checks::Failing,
            ..pull()
        };

        assert_eq!(Group::of(&draft, 0), Group::Draft);
    }

    #[test]
    fn the_review_badge_names_who_approved_or_who_is_pending() {
        let approved = PullRequest {
            approvers: vec!["ana".to_string(), "beto".to_string()],
            ..pull()
        };
        let pending = PullRequest {
            review: Review::Required,
            requested_reviewers: vec!["carla".to_string()],
            ..pull()
        };
        let unasked = PullRequest {
            review: Review::Missing,
            ..pull()
        };
        let label = |pull: &PullRequest| {
            badges(pull)
                .into_iter()
                .map(|badge| badge.label)
                .find(|label| label != "✓ CI")
        };

        assert_eq!(
            label(&approved).as_deref(),
            Some("Aprobado por @ana, @beto")
        );
        assert_eq!(label(&pending).as_deref(), Some("Esperando a @carla"));
        assert_eq!(label(&unasked), None);
    }

    #[test]
    fn the_next_action_is_what_blocks_the_pull_request_first() {
        let red = PullRequest {
            checks: Checks::Failing,
            failing_checks: vec![
                crate::github::FailedCheck {
                    name: "pytest".to_string(),
                    reason: None,
                    url: None,
                },
                crate::github::FailedCheck {
                    name: "docs-check".to_string(),
                    reason: None,
                    url: None,
                },
            ],
            mergeable: Mergeable::Conflicting,
            ..pull()
        };
        let waiting = PullRequest {
            review: Review::Required,
            requested_reviewers: vec!["carla".to_string()],
            ..pull()
        };
        let draft = PullRequest {
            draft: true,
            ..pull()
        };

        let conflicts = action(&red, None, 0);
        let asked = action(&waiting, None, 0);
        let to_publish = action(&draft, None, 0);
        let ready = action(&pull(), None, 0);
        let stacked = action(&pull(), Some(&waiting), 0);
        let threaded = action(
            &PullRequest {
                threads: vec![crate::github::Thread {
                    author: "ana".to_string(),
                    path: "a.py".to_string(),
                    line: None,
                    body: String::new(),
                    url: String::new(),
                    replies: 0,
                }],
                ..pull()
            },
            None,
            0,
        );

        assert_eq!(conflicts.label, "Resolver conflictos con master");
        assert_eq!(
            conflicts.task.as_deref(),
            Some("Resolver conflictos con master")
        );
        assert_eq!(
            action(
                &PullRequest {
                    mergeable: Mergeable::Clean,
                    ..red
                },
                None,
                0
            )
            .label,
            "Arreglar CI: pytest, docs-check"
        );
        assert_eq!(stacked.label, "Esperando el merge de #1");
        assert_eq!(threaded.label, "Responder 1 hilo de review");
        assert!(threaded.task.is_some());
        assert_eq!(asked.label, "Esperando review de @carla");
        assert_eq!(asked.task, None);
        assert_eq!(to_publish.label, "Sacar de borrador");
        assert_eq!(ready.label, "Mergear");
    }

    #[test]
    fn ago_picks_the_largest_whole_unit() {
        assert_eq!(ago(30), "recién");
        assert_eq!(ago(150), "hace 2 min");
        assert_eq!(ago(7_300), "hace 2 h");
        assert_eq!(ago(200_000), "hace 2 d");
    }
}
