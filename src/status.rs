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

    pub fn color(self) -> u32 {
        match self {
            Group::Attention => theme::RED,
            Group::Ready => theme::GREEN,
            Group::Waiting => theme::YELLOW,
            Group::Draft => theme::MUTED,
        }
    }
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

    match pull.checks {
        Checks::Passing => push("✓ CI", theme::GREEN),
        Checks::Failing => push("✗ CI", theme::RED),
        Checks::Pending => push("● CI corriendo", theme::YELLOW),
        Checks::Missing => {}
    }
    match pull.review {
        Review::Approved => push("Aprobado", theme::GREEN),
        Review::ChangesRequested => push("Cambios pedidos", theme::RED),
        Review::Required => push("Falta review", theme::YELLOW),
        Review::Missing => push("Sin reviews", theme::MUTED),
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
            mergeable: Mergeable::Clean,
            comments: 0,
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
    fn ago_picks_the_largest_whole_unit() {
        assert_eq!(ago(30), "recién");
        assert_eq!(ago(150), "hace 2 min");
        assert_eq!(ago(7_300), "hace 2 h");
        assert_eq!(ago(200_000), "hace 2 d");
    }
}
