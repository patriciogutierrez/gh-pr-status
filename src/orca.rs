//! Each group's agent: a Claude Code session in the app's terminal panel,
//! working from a folder of its own with every repo of `~/code` readable.
//!
//! The agent plans in its folder and makes the changes in Orca worktrees it
//! tags with the group (`--comment gh-pr-status:group=<id>`). Orca refuses a
//! parent worktree from another repo, so the tag is what ties a worktree to its
//! group; the PRs opened from tagged branches join the group on their own.
//! Subgroups share their group's agent: the app lists their tags in
//! `subgrupos.md`, so the agent tags each worktree with the subgroup it is for.
//! Everything here blocks and runs on the background executor.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::folders::{self, Folder, Folders};
use crate::github::PullRequest;
use crate::settings;
use crate::tool;

/// What a worktree's Orca comment starts with when it belongs to a group.
const TAG: &str = "gh-pr-status:group=";
/// Where the repos are cloned when `GH_PR_STATUS_CODE_DIR` doesn't say.
const CODE_DIR: &str = "code";
/// Where the groups' folders go, under the home folder.
const GROUPS_DIR: &str = "orca/groups";
/// The file in the group's folder that lists its subgroups' tags.
const SUBGROUPS_FILE: &str = "subgrupos.md";
/// What `CLAUDE.md` says about subgroups; added to the folders made before them.
const SUBGROUPS_SECTION: &str = "
## Subgrupos

El grupo se puede dividir en subgrupos, listados en `subgrupos.md` (lo mantiene gh-pr-status). \
Antes de crear un worktree, léelo y usa en `--comment` la marca del subgrupo al que pertenece el \
trabajo; si ninguno calza, usa la del grupo. Así cada PR aparece en su subgrupo.
";

/// An Orca worktree tagged with a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    /// `owner/name`, lowercased as Orca keeps it.
    pub repo: String,
    pub branch: String,
    pub folder: i64,
}

/// Where a group's agent runs and what starts it.
pub struct Launch {
    pub dir: PathBuf,
    pub command: String,
}

/// The group's agent: Claude Code in the group's folder, made the first
/// time, reading every repo, and back in its last conversation if it had
/// one. `folder` is a top-level group and `subgroups` its subgroups.
pub fn agent_launch(folder: &Folder, subgroups: &[Folder]) -> Result<Launch, String> {
    let dir = agent_dir(folder)?;
    write_subgroups_file(&dir, folder, subgroups)?;
    let mut command = format!("claude --add-dir {}", quote(&code_dir()?));
    if has_conversation(&dir)? {
        command.push_str(" --continue");
    }
    Ok(Launch { dir, command })
}

/// Whether Claude Code kept a conversation from `dir`, under the folder it
/// names after the path with every other character a dash.
fn has_conversation(dir: &Path) -> Result<bool, String> {
    let name: String = dir
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let entries = match std::fs::read_dir(home()?.join(".claude/projects").join(name)) {
        Ok(entries) => entries,
        Err(_) => return Ok(false),
    };
    Ok(entries
        .flatten()
        .any(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl")))
}

/// What the agent's terminal shows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    /// Claude Code's input line, ready for a request.
    Prompt,
    /// Claude Code asking whether to trust the folder.
    Trust,
    Other,
}

/// What the agent's screen shows, from its lines.
pub fn screen_of<'a>(lines: impl Iterator<Item = &'a str>) -> Screen {
    let mut prompt = false;
    for line in lines {
        if line.contains("safety check") {
            return Screen::Trust;
        }
        if line.trim_start().starts_with('❯') {
            prompt = true;
        }
    }
    if prompt {
        Screen::Prompt
    } else {
        Screen::Other
    }
}

/// What the agent is asked when a PR's next action is handed to it.
pub fn task_prompt(pull: &PullRequest, task: &str, folder: &Folder, code: &str) -> String {
    let mut prompt = format!(
        "{task} en el PR #{} de {} ({}), rama `{}` sobre `{}`.",
        pull.number, pull.repo, pull.url, pull.head, pull.base
    );
    for (index, thread) in pull.threads.iter().enumerate() {
        let at = match thread.line {
            Some(line) => format!("{}:{line}", thread.path),
            None => thread.path.clone(),
        };
        prompt.push_str(&format!(
            " Hilo de review {} (@{} en {at}): {} ({}).",
            index + 1,
            thread.author,
            thread.body,
            thread.url
        ));
    }
    for check in &pull.failing_checks {
        prompt.push_str(&format!(" Check fallido `{}`", check.name));
        if let Some(reason) = &check.reason {
            prompt.push_str(&format!(": {reason}"));
        }
        if let Some(url) = &check.url {
            prompt.push_str(&format!(" (logs: {url})"));
        }
        prompt.push('.');
    }
    prompt.push_str(&format!(
        " Trabaja en el worktree de Orca que tenga esa rama (`orca worktree list --json`). Si no hay \
ninguno, créalo con `orca worktree create --repo path:{code}/{} --name {} --comment \"{TAG}{}\" \
--setup skip --json` y cámbialo a la rama con `git fetch origin {} && git checkout {}`. Al terminar, \
haz push y avísame.",
        pull.repo_name(),
        pull.head.rsplit('/').next().unwrap_or(&pull.head),
        folder.id,
        pull.head,
        pull.head
    ));
    prompt
}

/// Rewrites the group's `subgrupos.md`, if its agent was ever opened.
pub fn sync_subgroups(folder: &Folder, subgroups: &[Folder]) -> Result<(), String> {
    match folders::agent_dir(folder.id)? {
        Some(dir) if Path::new(&dir).exists() => {
            write_subgroups_file(Path::new(&dir), folder, subgroups)
        }
        _ => Ok(()),
    }
}

/// Every Orca worktree tagged with a group.
pub fn worktrees() -> Result<Vec<Worktree>, String> {
    orca(&["worktree", "list"]).map(|result| parse_worktrees(&result))
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
            let worktree = worktrees.iter().find(|worktree| {
                worktree.repo.eq_ignore_ascii_case(&pull.repo) && worktree.branch == pull.head
            })?;
            folders
                .list
                .iter()
                .any(|folder| folder.id == worktree.folder)
                .then(|| (pull.url.clone(), worktree.folder))
        })
        .collect()
}

fn parse_worktrees(result: &Value) -> Vec<Worktree> {
    result["worktrees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|worktree| {
            let folder = worktree["comment"]
                .as_str()?
                .trim()
                .strip_prefix(TAG)?
                .parse()
                .ok()?;
            let repo = worktree["projectId"].as_str()?.strip_prefix("github:")?;
            let branch = worktree["branch"].as_str()?;
            Some(Worktree {
                repo: repo.to_string(),
                branch: branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string(),
                folder,
            })
        })
        .collect()
}

/// The group's folder, made the first time: a git repo, since Orca only takes
/// those, holding the `CLAUDE.md` the agent starts from.
fn agent_dir(folder: &Folder) -> Result<PathBuf, String> {
    if let Some(dir) = folders::agent_dir(folder.id)? {
        let dir = PathBuf::from(dir);
        if dir.join(".git").exists() {
            return Ok(dir);
        }
    }

    let groups = home()?.join(GROUPS_DIR);
    let mut dir = groups.join(slug(&folder.name));
    if dir.exists() {
        dir = groups.join(format!("{}-{}", slug(&folder.name), folder.id));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("No se pudo crear {}: {error}", dir.display()))?;
    std::fs::write(dir.join("CLAUDE.md"), claude_md(folder, &code_dir()?))
        .map_err(|error| format!("No se pudo escribir el CLAUDE.md del grupo: {error}"))?;
    git(&dir, &["init", "-q", "-b", "main"])?;
    git(&dir, &["add", "CLAUDE.md"])?;
    git(
        &dir,
        &["commit", "-q", "-m", &format!("Grupo {}", folder.name)],
    )?;

    folders::save_agent_dir(folder.id, &dir.to_string_lossy())?;
    Ok(dir)
}

/// Writes `subgrupos.md`, and tells a `CLAUDE.md` from before subgroups about
/// it.
fn write_subgroups_file(dir: &Path, folder: &Folder, subgroups: &[Folder]) -> Result<(), String> {
    let error =
        |error: std::io::Error| format!("No se pudo actualizar la carpeta del grupo: {error}");
    std::fs::write(dir.join(SUBGROUPS_FILE), subgroups_md(folder, subgroups)).map_err(error)?;

    let claude = dir.join("CLAUDE.md");
    let text = std::fs::read_to_string(&claude).unwrap_or_default();
    if !text.contains(SUBGROUPS_FILE) {
        std::fs::write(&claude, format!("{}\n{SUBGROUPS_SECTION}", text.trim_end()))
            .map_err(error)?;
    }
    Ok(())
}

fn subgroups_md(folder: &Folder, subgroups: &[Folder]) -> String {
    let line = |name: &str, id: i64| format!("- {name}: `{TAG}{id}`\n");
    let mut text = format!(
        "# Subgrupos de \"{}\"

Lo mantiene gh-pr-status; no lo edites. Al crear un worktree, usa en `--comment` la marca del \
subgrupo al que pertenece el trabajo, y sus PRs aparecerán en ese subgrupo.

",
        folder.name
    );
    text.push_str(&line(
        &format!("{} (el grupo completo)", folder.name),
        folder.id,
    ));
    if subgroups.is_empty() {
        text.push_str("\nTodavía no hay subgrupos. El usuario los crea en gh-pr-status.\n");
    }
    for subgroup in subgroups {
        text.push_str(&line(&subgroup.name, subgroup.id));
    }
    text
}

fn claude_md(folder: &Folder, code: &str) -> String {
    format!(
        "# Grupo: {name}

Espacio de trabajo del grupo \"{name}\" de gh-pr-status. Aquí se conversa qué hay que hacer y en \
qué repos. El código se cambia en worktrees de Orca, nunca en esta carpeta ni directo en `{code}`.

## Objetivo

Sin definir. Al empezar, pregúntale al usuario qué quiere lograr y anótalo aquí.

## Repos

Todos los repos de Toku están clonados en `{code}/<repo>`, y los puedes leer. Úsalos para \
investigar y decidir qué repos hay que tocar.

## Cómo trabajar en un repo

1. Si el repo no está en Orca (`orca repo list`), regístralo: `orca repo add --path {code}/<repo>`.
2. Crea el worktree marcado con el grupo (o con su subgrupo, ver \"Subgrupos\"), para que sus PRs \
aparezcan solos en gh-pr-status:

   ```
   orca worktree create --repo path:{code}/<repo> --name <nombre-corto> --comment \"{TAG}{id}\" --setup skip --json
   ```

3. Orca nombra la rama `<usuario>/<nombre-corto>`. Renómbrala a la convención de Toku antes de \
hacer commits: `git -C <path del worktree> branch -m feat/<nombre-corto>` (o `fix/`, `chore/`…).
4. Anota el repo, la rama y el path en la sección \"Worktrees\".

## Worktrees

Ninguno todavía.
{SUBGROUPS_SECTION}",
        name = folder.name,
        id = folder.id,
    )
}

/// A folder name for the group: lowercase ASCII words joined by dashes.
fn slug(name: &str) -> String {
    let plain: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'ä' => 'a',
            'é' | 'è' | 'ë' => 'e',
            'í' | 'ì' | 'ï' => 'i',
            'ó' | 'ò' | 'ö' => 'o',
            'ú' | 'ù' | 'ü' => 'u',
            'ñ' => 'n',
            c if c.is_ascii_alphanumeric() => c,
            _ => ' ',
        })
        .collect();
    let slug = plain.split_whitespace().collect::<Vec<_>>().join("-");
    if slug.is_empty() {
        "grupo".to_string()
    } else {
        slug
    }
}

/// Wraps `text` in single quotes for the shell.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

pub fn code_dir() -> Result<String, String> {
    match settings::get(settings::CODE_DIR) {
        Some(dir) => Ok(dir),
        None => Ok(home()?.join(CODE_DIR).to_string_lossy().to_string()),
    }
}

fn home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "No se encontró HOME".to_string())
}

/// Runs `orca <args> --json` and returns its `result`.
fn orca(args: &[&str]) -> Result<Value, String> {
    let output = Command::new(tool::path("orca"))
        .args(args)
        .arg("--json")
        .output()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                "No se encontró el comando orca. Instala Orca y su CLI.".to_string()
            }
            _ => format!("No se pudo ejecutar orca: {error}"),
        })?;
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        format!("orca terminó con error: {}", stderr.trim())
    })?;
    if value["ok"].as_bool() == Some(true) {
        Ok(value["result"].clone())
    } else {
        Err(format!(
            "orca terminó con error: {}",
            value["error"]["message"].as_str().unwrap_or("sin detalle")
        ))
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new(tool::path("git"))
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| format!("No se pudo ejecutar git: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("git terminó con error: {}", stderr.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

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

    fn worktree(comment: &str) -> Value {
        json!({
            "projectId": "github:toku-ia/databases",
            "branch": "refs/heads/feat/pagos",
            "comment": comment,
        })
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
    fn only_worktrees_tagged_with_a_group_are_read() {
        let result = json!({ "worktrees": [worktree("gh-pr-status:group=7"), worktree("notas")] });

        let worktrees = parse_worktrees(&result);

        assert_eq!(
            worktrees,
            [Worktree {
                repo: "toku-ia/databases".to_string(),
                branch: "feat/pagos".to_string(),
                folder: 7,
            }]
        );
    }

    #[test]
    fn a_pull_request_from_a_tagged_branch_joins_its_group_once() {
        let pull = pull("Toku-IA/databases", "feat/pagos");
        let worktrees =
            parse_worktrees(&json!({ "worktrees": [worktree("gh-pr-status:group=7")] }));
        let mut claimed = folders(&[7]);
        claimed.claimed.insert(pull.url.clone());

        let first = claims(std::slice::from_ref(&pull), &worktrees, &folders(&[7]));
        let again = claims(std::slice::from_ref(&pull), &worktrees, &claimed);
        let deleted = claims(std::slice::from_ref(&pull), &worktrees, &folders(&[8]));

        assert_eq!(first, [(pull.url.clone(), 7)]);
        assert!(again.is_empty());
        assert!(deleted.is_empty());
    }

    #[test]
    fn the_subgroups_file_lists_the_group_and_each_subgroup_tag() {
        let group = Folder {
            id: 4,
            name: "Activaciones".to_string(),
            parent: None,
        };
        let front = Folder {
            id: 7,
            name: "Front".to_string(),
            parent: Some(4),
        };

        let text = subgroups_md(&group, &[front]);

        assert!(text.contains("- Activaciones (el grupo completo): `gh-pr-status:group=4`"));
        assert!(text.contains("- Front: `gh-pr-status:group=7`"));
    }

    #[test]
    fn an_old_claude_md_learns_about_subgroups_once() {
        let dir =
            std::env::temp_dir().join(format!("gh-pr-status-subgroups-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "# Grupo: Activaciones\n").unwrap();
        let group = Folder {
            id: 4,
            name: "Activaciones".to_string(),
            parent: None,
        };

        write_subgroups_file(&dir, &group, &[]).unwrap();
        write_subgroups_file(&dir, &group, &[]).unwrap();
        let claude = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(claude.matches("## Subgrupos").count(), 1);
        assert!(claude.starts_with("# Grupo: Activaciones"));
    }

    #[test]
    fn the_task_prompt_names_the_pull_request_its_checks_and_the_worktree_tag() {
        let mut pull = pull("Toku-IA/databases", "feat/pagos");
        pull.failing_checks = vec![crate::github::FailedCheck {
            name: "pytest".to_string(),
            reason: Some("3 failed".to_string()),
            url: Some("https://ci/1".to_string()),
        }];
        let group = Folder {
            id: 4,
            name: "Pagos".to_string(),
            parent: None,
        };

        let prompt = task_prompt(&pull, "Arreglar CI: pytest", &group, "/code");

        assert!(prompt.starts_with("Arreglar CI: pytest en el PR #12 de Toku-IA/databases"));
        assert!(prompt.contains("Check fallido `pytest`: 3 failed (logs: https://ci/1)."));
        assert!(prompt.contains(
            "--repo path:/code/databases --name pagos --comment \"gh-pr-status:group=4\""
        ));
        assert!(prompt.contains("git checkout feat/pagos"));
    }

    #[test]
    fn the_trust_question_is_told_apart_from_the_prompt() {
        let trust = [
            "Quick safety check: Is this a project you trust?",
            " ❯ No, exit",
        ];
        let ready = ["──────", "❯ ", "? for shortcuts"];
        let shell = ["➜  grupo claude --add-dir '/code'"];

        assert_eq!(screen_of(trust.into_iter()), Screen::Trust);
        assert_eq!(screen_of(ready.into_iter()), Screen::Prompt);
        assert_eq!(screen_of(shell.into_iter()), Screen::Other);
    }

    #[test]
    fn a_group_name_becomes_a_plain_folder_name() {
        assert_eq!(
            slug("Mejoras Santander: reintentos"),
            "mejoras-santander-reintentos"
        );
        assert_eq!(slug("Conciliación BCI"), "conciliacion-bci");
        assert_eq!(slug("¿?"), "grupo");
    }

    #[test]
    fn a_path_with_a_quote_survives_the_shell() {
        assert_eq!(quote("/Users/o'neil/code"), r"'/Users/o'\''neil/code'");
    }
}
