//! The git worktrees the PRs are worked in, made with plain git so Orca
//! needn't be open.
//!
//! They go under `~/orca/workspaces/<repo>/<name>`, where Orca keeps its own,
//! so Orca shows them too when it is open. A worktree belongs to a group
//! through its branch's `prmanagergroup` git config, which its PRs follow
//! into the group. Worktrees Orca tagged with a group
//! (`--comment gh-pr-status:group=<id>`) get that config while Orca runs.
//! Everything here blocks and runs on the background executor.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::folders::Folders;
use crate::github::PullRequest;
use crate::tool;

/// The git config key, under the branch, naming the worktree's group.
pub const GROUP_KEY: &str = "prmanagergroup";
/// Where new worktrees go, under the home folder: Orca's own place.
const WORKSPACES_DIR: &str = "orca/workspaces";
/// What an Orca worktree's comment starts with when it belongs to a group.
const ORCA_TAG: &str = "gh-pr-status:group=";
/// How long Orca's CLI gets before it is given up on.
const ORCA_WAIT: Duration = Duration::from_secs(5);

/// A git worktree of one of the repos in the code folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    /// `owner/name`, lowercased, from the repo's `origin`.
    pub repo: String,
    pub branch: String,
    pub path: PathBuf,
    /// The repo's own checkout, which is never removed.
    pub main: bool,
    /// The group its branch is tagged with.
    pub folder: Option<i64>,
}

impl Worktree {
    /// Whether the PR's branch is the one checked out here.
    pub fn holds(&self, pull: &PullRequest) -> bool {
        self.repo.eq_ignore_ascii_case(&pull.repo) && self.branch == pull.head
    }
}

/// Every worktree of every repo cloned in `code`, after copying Orca's group
/// tags to git when Orca is running.
pub fn list(code: &Path) -> Vec<Worktree> {
    let repos = repos(code);
    import_orca_tags();
    repos
        .iter()
        .flat_map(|repo| repo_worktrees(repo).unwrap_or_default())
        .collect()
}

/// The PR's worktree, made the first time on its branch and tagged with
/// `folder`.
pub fn create(code: &Path, pull: &PullRequest, folder: Option<i64>) -> Result<PathBuf, String> {
    let repo = code.join(pull.repo_name());
    if !repo.join(".git").is_dir() {
        return Err(format!(
            "{} no está clonado en {}.",
            pull.repo_name(),
            code.display()
        ));
    }
    let path = home()?
        .join(WORKSPACES_DIR)
        .join(pull.repo_name())
        .join(dir_name(&pull.head));
    if path.exists() {
        return Err(format!("Ya existe {}.", path.display()));
    }
    git(&repo, &["fetch", "-q", "origin", &pull.head])?;
    let path_text = path.to_string_lossy().to_string();
    let local = git(
        &repo,
        &[
            "show-ref",
            "--verify",
            "-q",
            &format!("refs/heads/{}", pull.head),
        ],
    )
    .is_ok();
    if local {
        git(&repo, &["worktree", "add", "-q", &path_text, &pull.head])?;
    } else {
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--track",
                "-b",
                &pull.head,
                &path_text,
                &format!("origin/{}", pull.head),
            ],
        )?;
    }
    if let Some(folder) = folder {
        tag(&repo, &pull.head, folder)?;
    }
    Ok(path)
}

/// Removes the worktree's folder, refusing one with uncommitted changes. Its
/// branch stays.
pub fn remove(worktree: &Worktree) -> Result<(), String> {
    if worktree.main {
        return Err("Es el checkout del repo, no un worktree.".to_string());
    }
    let path = worktree.path.to_string_lossy().to_string();
    git(&worktree.path, &["worktree", "remove", &path]).map_err(|error| {
        if error.contains("modified or untracked") {
            format!(
                "{} tiene cambios sin commit; no se borró.",
                worktree.path.display()
            )
        } else {
            error
        }
    })
}

/// The open PRs that should join a group because their branch is a tagged
/// worktree's: those in no group yet, never claimed before, whose group still
/// exists.
pub fn claims(
    pulls: &[PullRequest],
    worktrees: &[Worktree],
    folders: &Folders,
) -> Vec<(String, i64)> {
    pulls
        .iter()
        .filter(|pull| {
            !folders.by_url.contains_key(&pull.url) && !folders.claimed.contains(&pull.url)
        })
        .filter_map(|pull| {
            let folder = worktrees
                .iter()
                .find(|worktree| worktree.holds(pull))?
                .folder?;
            folders
                .list
                .iter()
                .any(|known| known.id == folder)
                .then(|| (pull.url.clone(), folder))
        })
        .collect()
}

/// The folders in `code` that are a repo's own checkout.
fn repos(code: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(code) else {
        return Vec::new();
    };
    let mut repos: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join(".git").is_dir())
        .collect();
    repos.sort();
    repos
}

fn repo_worktrees(repo: &Path) -> Result<Vec<Worktree>, String> {
    let config = output(
        repo,
        &[
            "config",
            "--get-regexp",
            &format!(r"^(remote\.origin\.url|branch\..*\.{GROUP_KEY})$"),
        ],
    )
    .unwrap_or_default();
    let (name, tags) = parse_config(&config);
    let Some(name) = name else {
        return Ok(Vec::new());
    };
    let list = output(repo, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktrees(&list)
        .into_iter()
        .map(|(path, branch)| Worktree {
            repo: name.clone(),
            main: path == repo,
            folder: tags
                .iter()
                .find(|(tagged, _)| *tagged == branch)
                .map(|(_, folder)| *folder),
            branch,
            path,
        })
        .collect())
}

/// The repo's `owner/name` and each branch's group, from
/// `git config --get-regexp`.
fn parse_config(config: &str) -> (Option<String>, Vec<(String, i64)>) {
    let mut name = None;
    let mut tags = Vec::new();
    for line in config.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        if key == "remote.origin.url" {
            name = repo_name(value);
        } else if let Some(branch) = key
            .strip_prefix("branch.")
            .and_then(|rest| rest.strip_suffix(&format!(".{GROUP_KEY}")))
            && let Ok(folder) = value.trim().parse()
        {
            tags.push((branch.to_string(), folder));
        }
    }
    (name, tags)
}

/// `owner/name`, lowercased, from an SSH or HTTPS GitHub remote.
fn repo_name(url: &str) -> Option<String> {
    let path = url
        .trim()
        .strip_prefix("git@github.com:")
        .or_else(|| url.trim().strip_prefix("https://github.com/"))
        .or_else(|| url.trim().strip_prefix("ssh://git@github.com/"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    path.contains('/').then(|| path.to_lowercase())
}

/// Each worktree's path and branch, from `git worktree list --porcelain`;
/// detached ones have no branch and are left out.
fn parse_worktrees(list: &str) -> Vec<(PathBuf, String)> {
    list.split("\n\n")
        .filter_map(|block| {
            let mut path = None;
            let mut branch = None;
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(rest));
                } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
                    branch = Some(rest.to_string());
                }
            }
            Some((path?, branch?))
        })
        .collect()
}

/// A folder name for the branch: its slashes as dashes.
fn dir_name(branch: &str) -> String {
    branch.replace('/', "-")
}

fn tag(repo: &Path, branch: &str, folder: i64) -> Result<(), String> {
    git(
        repo,
        &[
            "config",
            &format!("branch.{branch}.{GROUP_KEY}"),
            &folder.to_string(),
        ],
    )
}

/// Copies to git the groups Orca's comments name, for the worktrees whose
/// branch has none yet. Nothing happens while Orca isn't running.
fn import_orca_tags() {
    let Some(result) = orca(&["worktree", "list"]) else {
        return;
    };
    for (path, branch, folder) in parse_orca_worktrees(&result) {
        let key = format!("branch.{branch}.{GROUP_KEY}");
        if output(&path, &["config", "--get", &key]).is_err() {
            tag(&path, &branch, folder).ok();
        }
    }
}

/// Each tagged Orca worktree's path, branch and group.
fn parse_orca_worktrees(result: &Value) -> Vec<(PathBuf, String, i64)> {
    result["worktrees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|worktree| {
            let folder = worktree["comment"]
                .as_str()?
                .trim()
                .strip_prefix(ORCA_TAG)?
                .parse()
                .ok()?;
            let path = worktree["path"].as_str()?;
            let branch = worktree["branch"].as_str()?;
            Some((
                PathBuf::from(path),
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string(),
                folder,
            ))
        })
        .collect()
}

/// Runs `orca <args> --json` and returns its `result`, or nothing when Orca
/// isn't installed, isn't running or doesn't answer in time.
fn orca(args: &[&str]) -> Option<Value> {
    let mut child = Command::new(tool::path("orca"))
        .args(args)
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < ORCA_WAIT => {
                std::thread::sleep(Duration::from_millis(50))
            }
            _ => {
                child.kill().ok();
                child.wait().ok();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    let value: Value = serde_json::from_slice(&output.stdout).ok()?;
    (value["ok"].as_bool() == Some(true)).then(|| value["result"].clone())
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "No se encontró HOME".to_string())
}

fn git(dir: &Path, args: &[&str]) -> Result<(), String> {
    output(dir, args).map(|_| ())
}

fn output(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(tool::path("git"))
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| format!("No se pudo ejecutar git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("git terminó con error: {}", stderr.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    use crate::folders::Folder;
    use crate::github::{Checks, Mergeable, Review};

    fn pull(repo: &str, head: &str) -> PullRequest {
        PullRequest {
            number: 12,
            title: "Reintentar el login".to_string(),
            url: format!("https://github.com/{repo}/pull/12"),
            repo: repo.to_string(),
            author: String::new(),
            base: "master".to_string(),
            head: head.to_string(),
            draft: false,
            checks: Checks::Passing,
            failing_checks: Vec::new(),
            review: Review::Missing,
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

    fn worktree(branch: &str, folder: Option<i64>) -> Worktree {
        Worktree {
            repo: "toku-ia/databases".to_string(),
            branch: branch.to_string(),
            path: PathBuf::from("/w"),
            main: false,
            folder,
        }
    }

    fn folders(ids: &[i64]) -> Folders {
        Folders {
            list: ids
                .iter()
                .map(|&id| Folder {
                    id,
                    name: format!("Grupo {id}"),
                    parent: None,
                })
                .collect(),
            ..Folders::default()
        }
    }

    #[test]
    fn the_config_names_the_repo_and_each_tagged_branch() {
        let config = "remote.origin.url git@github.com:Toku-IA/databases.git\n\
branch.feat/pagos.prmanagergroup 4\n\
branch.fix/x.prmanagergroup nope\n";

        let (name, tags) = parse_config(config);

        assert_eq!(name.as_deref(), Some("toku-ia/databases"));
        assert_eq!(tags, vec![("feat/pagos".to_string(), 4)]);
    }

    #[test]
    fn https_remotes_name_the_repo_too() {
        assert_eq!(
            repo_name("https://github.com/Toku-IA/code.git").as_deref(),
            Some("toku-ia/code")
        );
        assert_eq!(repo_name("/some/local/path"), None);
    }

    #[test]
    fn detached_worktrees_are_left_out() {
        let list = "worktree /code/db\nHEAD abc\nbranch refs/heads/master\n\n\
worktree /w/db/x\nHEAD def\ndetached\n\n\
worktree /w/db/feat-pagos\nHEAD 123\nbranch refs/heads/feat/pagos\n";

        let worktrees = parse_worktrees(list);

        assert_eq!(
            worktrees,
            vec![
                (PathBuf::from("/code/db"), "master".to_string()),
                (PathBuf::from("/w/db/feat-pagos"), "feat/pagos".to_string()),
            ]
        );
    }

    #[test]
    fn only_orca_worktrees_tagged_with_a_group_are_imported() {
        let result = json!({"worktrees": [
            {"path": "/w/a", "branch": "refs/heads/feat/pagos", "comment": "gh-pr-status:group=4"},
            {"path": "/w/b", "branch": "refs/heads/feat/otra", "comment": "otra cosa"},
            {"path": "/w/c", "branch": "refs/heads/feat/c", "comment": ""},
        ]});

        let tagged = parse_orca_worktrees(&result);

        assert_eq!(
            tagged,
            vec![(PathBuf::from("/w/a"), "feat/pagos".to_string(), 4)]
        );
    }

    #[test]
    fn a_pull_request_from_a_tagged_branch_joins_its_group_once() {
        let pulls = [
            pull("Toku-IA/databases", "feat/pagos"),
            pull("Toku-IA/databases", "feat/suelta"),
        ];
        let worktrees = [
            worktree("feat/pagos", Some(4)),
            worktree("feat/suelta", None),
        ];
        let mut known = folders(&[4]);

        let first = claims(&pulls, &worktrees, &known);
        known.claimed.insert(pulls[0].url.clone());
        let second = claims(&pulls, &worktrees, &known);

        assert_eq!(first, vec![(pulls[0].url.clone(), 4)]);
        assert!(second.is_empty());
    }

    #[test]
    fn a_branch_becomes_a_folder_name() {
        assert_eq!(dir_name("feat/pay5-353-models"), "feat-pay5-353-models");
    }
}
