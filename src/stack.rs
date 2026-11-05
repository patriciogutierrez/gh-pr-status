//! How the user's open PRs relate to each other: stacks, where a PR's base
//! is another PR's branch, and [MASTER]/[DEV] twins, the same change opened
//! against both long-lived branches.

use crate::github::PullRequest;

/// The prefixes a twin pair carries, as the title shows them.
const TWIN_PREFIXES: [&str; 2] = ["[MASTER]", "[DEV]"];

/// The PR `pull` is stacked on: the open one, in the same repo, whose branch
/// is `pull`'s base.
pub fn below<'a>(pull: &PullRequest, pulls: &'a [PullRequest]) -> Option<&'a PullRequest> {
    pulls
        .iter()
        .find(|other| other.url != pull.url && other.repo == pull.repo && other.head == pull.base)
}

/// The whole stack `pull` is in, bottom first: the PR on the long-lived
/// branch, then each PR on the one before it. A PR in no stack is alone.
pub fn chain<'a>(pull: &'a PullRequest, pulls: &'a [PullRequest]) -> Vec<&'a PullRequest> {
    let mut bottom = pull;
    while let Some(under) = below(bottom, pulls) {
        if under.url == pull.url {
            // A cycle; GitHub wouldn't open one, but don't spin on it.
            break;
        }
        bottom = under;
    }

    let mut chain = vec![bottom];
    loop {
        let top = chain[chain.len() - 1];
        let above = pulls.iter().find(|other| {
            other.repo == top.repo
                && other.base == top.head
                && !chain.iter().any(|seen| seen.url == other.url)
        });
        match above {
            Some(next) => chain.push(next),
            None => return chain,
        }
    }
}

/// The [MASTER]/[DEV] twin of `pull`: same repo, same title under the other
/// prefix.
pub fn twin<'a>(pull: &PullRequest, pulls: &'a [PullRequest]) -> Option<&'a PullRequest> {
    let (prefix, title) = split_prefix(&pull.title)?;
    pulls.iter().find(|other| {
        other.url != pull.url
            && other.repo == pull.repo
            && split_prefix(&other.title).is_some_and(|(p, t)| p != prefix && t == title)
    })
}

/// `[DEV]` and the rest of the title, for a title that starts with a twin
/// prefix.
pub fn split_prefix(title: &str) -> Option<(&str, &str)> {
    TWIN_PREFIXES.iter().find_map(|prefix| {
        title
            .strip_prefix(prefix)
            .map(|rest| (*prefix, rest.trim_start()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::github::{Checks, Mergeable, Review};

    fn pull(number: u64, title: &str, base: &str, head: &str) -> PullRequest {
        PullRequest {
            number,
            title: title.to_string(),
            url: format!("https://github.com/acme/demo/pull/{number}"),
            repo: "acme/demo".to_string(),
            author: String::new(),
            base: base.to_string(),
            head: head.to_string(),
            draft: false,
            checks: Checks::Passing,
            failing_checks: Vec::new(),
            review: Review::Approved,
            approvers: Vec::new(),
            requested_reviewers: Vec::new(),
            requested_teams: Vec::new(),
            mergeable: Mergeable::Clean,
            comments: 0,
            threads: Vec::new(),
            updated_at: 0,
            pipeline: None,
        }
    }

    #[test]
    fn a_stack_reads_bottom_up_from_the_long_lived_branch() {
        let pulls = [
            pull(3, "c", "feat/b", "feat/c"),
            pull(1, "a", "master", "feat/a"),
            pull(2, "b", "feat/a", "feat/b"),
            pull(9, "alone", "master", "fix/z"),
        ];

        let numbers = |pull: &PullRequest| {
            chain(pull, &pulls)
                .iter()
                .map(|member| member.number)
                .collect::<Vec<_>>()
        };

        assert_eq!(numbers(&pulls[2]), [1, 2, 3]);
        assert_eq!(below(&pulls[0], &pulls).map(|p| p.number), Some(2));
        assert_eq!(numbers(&pulls[3]), [9]);
    }

    #[test]
    fn twins_share_a_title_under_the_other_prefix() {
        let pulls = [
            pull(1, "[MASTER] 🐛 Fix login", "master", "fix/login"),
            pull(2, "[DEV] 🐛 Fix login", "development", "fix/login-dev"),
            pull(3, "[MASTER] 🐛 Fix logout", "master", "fix/logout"),
            pull(4, "🐛 Fix login", "master", "fix/login-2"),
        ];

        assert_eq!(twin(&pulls[0], &pulls).map(|p| p.number), Some(2));
        assert!(twin(&pulls[2], &pulls).is_none());
        assert!(twin(&pulls[3], &pulls).is_none());
    }
}
