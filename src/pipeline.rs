//! The CI/CD pipeline as a deploy bot reports it on a pull request.
//!
//! The bot posts one comment per run and keeps editing it: every stage appends
//! a `## <emoji> <title>` heading followed by a `> 2026-10-05 14:58:26 GMT-03`
//! line and a short body. Parsing those headings is enough to know where the
//! run is and whether it broke, without asking GitHub Actions separately.
//!
//! A normal run deploys pre-prod first and prod second, each one taking
//! traffic, running synthetics and closing with `🌟 CD: OK!`; the run itself
//! closes with `🌟 CD: 🚀 El despliegue fue aplicado correctamente`. So every
//! step is tagged with the environment it belongs to, and a run counts as
//! finished only on that closing heading.

use std::env;
use std::sync::OnceLock;

use serde_json::Value;

use crate::github::parse_timestamp;

/// Who runs the pipelines and which GCP project is which environment, read
/// once from the environment. Without a bot, no PR shows a pipeline.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// GitHub login of the bot that comments the runs (`GH_PR_STATUS_BOT`).
    pub bot: Option<String>,
    /// GCP project id of each environment, as it appears in the logs link.
    pub projects: Vec<(String, Env)>,
}

impl Settings {
    pub fn get() -> &'static Settings {
        static SETTINGS: OnceLock<Settings> = OnceLock::new();
        SETTINGS.get_or_init(Settings::from_env)
    }

    fn from_env() -> Settings {
        let var = |name: &str| env::var(name).ok().filter(|value| !value.trim().is_empty());
        let projects = [
            ("GH_PR_STATUS_GCP_PREPROD", Env::PreProd),
            ("GH_PR_STATUS_GCP_PROD", Env::Prod),
            ("GH_PR_STATUS_GCP_DEVELOPMENT", Env::Development),
        ]
        .into_iter()
        .filter_map(|(name, env)| var(name).map(|project| (project.trim().to_string(), env)))
        .collect();

        Settings {
            bot: var("GH_PR_STATUS_BOT").map(|bot| bot.trim().to_string()),
            projects,
        }
    }
}

/// A run with no new stage for this long is reported as stalled, not running.
const STALLED_AFTER_SECS: i64 = 30 * 60;

/// Every run comment opens with this heading; other bot comments (the
/// dependency list, unlock notices) do not.
const RUN_MARKER: &str = "Iniciando pipeline";

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Env {
    Development,
    PreProd,
    Prod,
}

impl Env {
    pub fn label(self) -> &'static str {
        match self {
            Env::Development => "development",
            Env::PreProd => "pre-prod",
            Env::Prod => "prod",
        }
    }

    /// The first environment named as a whole word, so `pre-prod` is never
    /// read as `prod`.
    fn named_in(text: &str) -> Option<Env> {
        text.split(|c: char| !(c.is_alphanumeric() || c == '-'))
            .find_map(|word| match word {
                "pre-prod" => Some(Env::PreProd),
                "prod" => Some(Env::Prod),
                "development" => Some(Env::Development),
                _ => None,
            })
    }

    /// Failure bodies name their environment only indirectly: `🌎 Ambiente: X`
    /// after a rollback, or the GCP project in the link to the logs.
    fn implied_by(line: &str, projects: &[(String, Env)]) -> Option<Env> {
        if let Some(name) = line.strip_prefix("🌎 Ambiente:") {
            return Env::named_in(name);
        }
        let (_, rest) = line.split_once("project=")?;
        let project: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        projects
            .iter()
            .find(|(id, _)| *id == project)
            .map(|(_, env)| *env)
    }
}

/// How a run ended, read from its closing heading.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Finish {
    Deployed,
    Hotfix,
    Canary,
    /// Nothing was deployed: only ignored files changed, or an unlock.
    NothingDeployed,
}

impl Finish {
    fn of(title: &str) -> Option<Finish> {
        if title.contains("despliegue fue aplicado")
            || title.contains("Deploy completado")
            || (title.contains("ipeline a")
                && (title.contains("terminó") || title.contains("completado")))
        {
            Some(Finish::Deployed)
        } else if title.contains("hotfix fue aplicado") {
            Some(Finish::Hotfix)
        } else if title.contains("Canary") && title.contains("activo") {
            Some(Finish::Canary)
        } else if title.contains("archivos ignorados") || title.contains("Entorno desbloqueado") {
            Some(Finish::NothingDeployed)
        } else {
            None
        }
    }
}

/// Where one environment of the run stands.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EnvState {
    /// Revisions and readiness checks, no traffic yet.
    Deploying,
    /// Takes traffic; synthetics may still be running.
    Live,
    /// Synthetics passed (`🌟 CD: OK!`).
    Ready,
    Failed,
}

/// Something worth a notification, in the order the run reached it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Milestone {
    Ready(Env),
    /// `None` when the run stopped before deploying (missing approvals, a lock).
    Failed(Option<Env>),
    Canary,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// 🌟 / ✅ milestones: preCI, CI, CD.
    Milestone,
    /// 🚨 errors and failed checks.
    Error,
    /// Everything else the bot narrates.
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub title: String,
    /// `HH:MM:SS` in the bot's own timezone (Chile), when the heading has one.
    pub time: Option<String>,
    pub kind: StepKind,
    /// The environment the step belongs to; `None` for CI and pre-deploy steps.
    pub env: Option<Env>,
    /// The few body lines worth showing: the service, the outcome, the error.
    pub details: Vec<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Succeeded,
    Failed,
    /// Not finished, but the bot has not written anything in a while.
    Stalled,
}

#[derive(Clone, Debug)]
pub struct Pipeline {
    /// Link to the bot's comment.
    pub url: String,
    /// When the bot last edited the comment, in Unix seconds.
    pub updated_at: i64,
    pub steps: Vec<Step>,
}

impl Pipeline {
    /// The bot's most recent run comment among the PR's latest comments, if any.
    pub fn latest(comments: &Value, settings: &Settings) -> Option<Pipeline> {
        let bot = settings.bot.as_deref()?;
        let comment = comments["nodes"]
            .as_array()?
            .iter()
            .filter(|comment| comment["author"]["login"].as_str() == Some(bot))
            .filter(|comment| {
                comment["body"]
                    .as_str()
                    .is_some_and(|body| body.contains(RUN_MARKER))
            })
            .max_by_key(|comment| comment["updatedAt"].as_str().and_then(parse_timestamp))?;

        Some(Pipeline {
            url: comment["url"].as_str().unwrap_or_default().to_string(),
            updated_at: comment["updatedAt"]
                .as_str()
                .and_then(parse_timestamp)
                .unwrap_or(0),
            steps: parse_steps(
                comment["body"].as_str().unwrap_or_default(),
                &settings.projects,
            ),
        })
    }

    pub fn state(&self, now: i64) -> State {
        if self.blocking_error().is_some() {
            State::Failed
        } else if self.finish().is_some() {
            State::Succeeded
        } else if now - self.updated_at > STALLED_AFTER_SECS {
            State::Stalled
        } else {
            State::Running
        }
    }

    pub fn last_step(&self) -> Option<&Step> {
        self.steps.last()
    }

    fn finish(&self) -> Option<(Finish, &Step)> {
        self.steps
            .iter()
            .rev()
            .find_map(|step| Finish::of(&step.title).map(|finish| (finish, step)))
    }

    /// The environments the run touched, in the order they took traffic
    /// (pre-prod then prod, or the reverse for a hotfix). Environments still
    /// in readiness checks follow, in the order they appeared.
    pub fn environments(&self) -> Vec<(Env, EnvState)> {
        let first = |wanted: &dyn Fn(&Step) -> bool, env: Env| {
            self.steps
                .iter()
                .position(|step| step.env == Some(env) && wanted(step))
        };
        let mut order: Vec<Env> = Vec::new();
        for env in self.steps.iter().filter_map(|step| step.env) {
            if !order.contains(&env) {
                order.push(env);
            }
        }
        order.sort_by_key(|&env| {
            (
                first(&|step| opens_phase(&step.title), env).unwrap_or(usize::MAX),
                first(&|_| true, env).unwrap_or(usize::MAX),
            )
        });

        let ready = self.milestones();
        order
            .into_iter()
            .map(|env| {
                let steps = || self.steps.iter().filter(move |step| step.env == Some(env));
                let state = if steps().any(|step| step.kind == StepKind::Error) {
                    EnvState::Failed
                } else if ready.contains(&Milestone::Ready(env)) {
                    EnvState::Ready
                } else if steps().any(|step| step.title.contains("recibe tráfico")) {
                    EnvState::Live
                } else {
                    EnvState::Deploying
                };
                (env, state)
            })
            .collect()
    }

    /// Ready environments, failures and canaries, each once, in run order.
    pub fn milestones(&self) -> Vec<Milestone> {
        let mut milestones: Vec<Milestone> = Vec::new();
        let mut push = |milestone: Milestone| {
            if !milestones.contains(&milestone) {
                milestones.push(milestone);
            }
        };

        for step in &self.steps {
            if step.kind == StepKind::Error {
                push(Milestone::Failed(step.env));
                continue;
            }
            match Finish::of(&step.title) {
                Some(Finish::Deployed) => {
                    // A Helm run closes with "Deploy completado en pre-prod y prod".
                    let env = if step.title.contains(" prod") {
                        Env::Prod
                    } else {
                        Env::named_in(&step.title).or(step.env).unwrap_or(Env::Prod)
                    };
                    push(Milestone::Ready(env));
                }
                Some(Finish::Hotfix) => push(Milestone::Ready(step.env.unwrap_or(Env::Prod))),
                Some(Finish::Canary) => push(Milestone::Canary),
                Some(Finish::NothingDeployed) => {}
                None if step.title.starts_with("🌟 CD") => {
                    if let Some(env) = step.env {
                        push(Milestone::Ready(env));
                    }
                }
                None => {}
            }
        }
        milestones
    }

    /// The step to quote on the collapsed row: the error that stopped the run,
    /// otherwise the latest stage.
    pub fn headline(&self) -> Option<&Step> {
        self.blocking_error().or_else(|| self.last_step())
    }

    /// The first error the run never got past. A run that hit an active lock
    /// waits and carries on, so an error followed by a later `🌟` milestone no
    /// longer stops it.
    fn blocking_error(&self) -> Option<&Step> {
        self.steps.iter().enumerate().find_map(|(index, step)| {
            let recovered = self.steps[index + 1..]
                .iter()
                .any(|later| later.kind == StepKind::Milestone);
            (step.kind == StepKind::Error && !recovered).then_some(step)
        })
    }
}

fn parse_steps(body: &str, projects: &[(String, Env)]) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    let mut in_code = false;

    for line in body.lines().map(str::trim) {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code || line.is_empty() {
            continue;
        }

        if let Some(title) = line.strip_prefix("## ").or_else(|| line.strip_prefix("# ")) {
            let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
            steps.push(Step {
                kind: kind_of(&title),
                env: Env::named_in(&title),
                title,
                time: None,
                details: Vec::new(),
            });
            continue;
        }

        let Some(step) = steps.last_mut() else {
            continue;
        };
        if step.env.is_none() {
            step.env = Env::implied_by(line, projects);
        }
        if let Some(stamp) = line.strip_prefix("> ") {
            if step.time.is_none() {
                step.time = stamp.split_whitespace().nth(1).map(str::to_string);
            }
        } else if let Some(detail) = detail_line(line, step.kind)
            && step.details.len() < 4
        {
            step.details.push(detail);
        }
    }

    inherit_environments(&mut steps);
    steps
}

/// The headings that start an environment's traffic phase.
fn opens_phase(title: &str) -> bool {
    title.contains("Logs de") || title.contains("recibe tráfico") || title.contains("Deploy en")
}

/// Steps such as `🌟 CD: OK!`, `Cambio de tráfico` or the synthetics name no
/// environment: they belong to the one that just started taking traffic.
/// Readiness checks run both environments in parallel, so they never set it.
fn inherit_environments(steps: &mut [Step]) {
    let mut current = None;
    for step in steps {
        match step.env {
            Some(env) if opens_phase(&step.title) => current = Some(env),
            Some(_) => {}
            None => step.env = current,
        }
    }
}

fn kind_of(title: &str) -> StepKind {
    if title.contains('🚨')
        || title.contains('❌')
        || title.contains("Error")
        || title.contains("Falló")
    {
        StepKind::Error
    } else if title.starts_with('🌟') || title.starts_with('✅') {
        StepKind::Milestone
    } else {
        StepKind::Info
    }
}

/// Keeps the lines that say what happened; drops links to logs, HTML from
/// the collapsible log block and boilerplate.
fn detail_line(line: &str, kind: StepKind) -> Option<String> {
    let line = line.trim_start_matches('#').trim();
    let skip = line.starts_with('<')
        || line.starts_with("Puedes")
        || line.starts_with("Revisa")
        || line.starts_with("Profiler")
        || line.starts_with("Todo update")
        || line.starts_with("Ahora se le agrega");
    if skip {
        return None;
    }

    let worth_it = line.starts_with('📦')
        || line.starts_with('✅')
        || line.starts_with('❌')
        || kind == StepKind::Error;
    worth_it.then(|| strip_links(line))
}

/// `[logs](https://...)` -> `logs`.
fn strip_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|i| open + i) else {
            break;
        };
        let Some(end) = rest[close..].find(')').map(|i| close + i) else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..close]);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn settings() -> Settings {
        Settings {
            bot: Some("deploybot".to_string()),
            projects: vec![
                ("acme-staging".to_string(), Env::PreProd),
                ("acme-prod".to_string(), Env::Prod),
                ("acme-dev".to_string(), Env::Development),
            ],
        }
    }

    const RUNNING: &str = "# 🚀 Iniciando pipeline
> 2026-10-05 14:49:50 GMT-03

Todo update actualizará este mensaje!

Puedes ver el progreso en el [action](https://github.com/x)
## 🌟 preCI: OK!
> 2026-10-05 14:51:06 GMT-03

Pasaron todos los checks previos a la ejecución del pipeline.
## 💡 Info prod: Readiness check terminado
> 2026-10-05 14:58:47 GMT-03

📦 Servicio: payments-api

✅ El readiness check terminó satisfactoriamente.";

    const FAILED: &str = "# 🚀 Iniciando pipeline
## 🌟 CI: OK!
> 2026-10-05 14:54:22 GMT-03
## 🚨 Error: 🚀 Falló el readiness check
> 2026-10-05 14:58:26 GMT-03

📦 Servicio: payments-api
La nueva versión no pudo desplegar correctamente.

### ❌ Error de tu servicio
<details open><summary>Últimas líneas</summary>

```
Redis Cluster Error ConnectionTimeoutError: Connection timeout
```
</details>

Revisa los [logs](https://console.cloud.google.com) en GCP!";

    fn pipeline(body: &str, updated_at: i64) -> Pipeline {
        Pipeline {
            url: String::new(),
            updated_at,
            steps: parse_steps(body, &settings().projects),
        }
    }

    #[test]
    fn splits_the_comment_into_timed_steps() {
        let steps = parse_steps(RUNNING, &settings().projects);

        let titles: Vec<_> = steps.iter().map(|step| step.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "🚀 Iniciando pipeline",
                "🌟 preCI: OK!",
                "💡 Info prod: Readiness check terminado"
            ]
        );
        assert_eq!(steps[2].time.as_deref(), Some("14:58:47"));
        assert_eq!(
            steps[2].details,
            [
                "📦 Servicio: payments-api",
                "✅ El readiness check terminó satisfactoriamente."
            ]
        );
    }

    #[test]
    fn an_error_step_keeps_what_broke_and_drops_logs_and_links() {
        let steps = parse_steps(FAILED, &settings().projects);

        assert_eq!(steps[2].kind, StepKind::Error);
        assert_eq!(
            steps[2].details,
            [
                "📦 Servicio: payments-api",
                "La nueva versión no pudo desplegar correctamente.",
                "❌ Error de tu servicio"
            ]
        );
    }

    #[test]
    fn state_follows_the_headings_and_the_clock() {
        let now = 10_000;

        assert_eq!(pipeline(RUNNING, now - 60).state(now), State::Running);
        assert_eq!(pipeline(RUNNING, now - 3_600).state(now), State::Stalled);
        assert_eq!(pipeline(FAILED, now).state(now), State::Failed);
        let done = format!("{RUNNING}\n## 🌟 CD: 🚀 El despliegue fue aplicado correctamente");
        assert_eq!(pipeline(&done, now - 3_600).state(now), State::Succeeded);
    }

    #[test]
    fn the_headline_of_a_failed_run_is_its_first_error() {
        let failed = pipeline(
            &format!("{FAILED}\n## 💡 Info prod: Readiness check terminado"),
            0,
        );

        assert_eq!(
            failed.headline().unwrap().title,
            "🚨 Error: 🚀 Falló el readiness check"
        );
    }

    #[test]
    fn latest_picks_the_most_recently_edited_bot_comment() {
        let comments = json!({ "nodes": [
            { "url": "old", "updatedAt": "2026-10-05T10:00:00Z", "author": { "login": "deploybot" }, "body": RUNNING },
            { "url": "human", "updatedAt": "2026-10-05T12:00:00Z", "author": { "login": "someone" }, "body": "/deploy" },
            { "url": "new", "updatedAt": "2026-10-05T11:00:00Z", "author": { "login": "deploybot" }, "body": FAILED }
        ]});

        let pipeline = Pipeline::latest(&comments, &settings()).unwrap();

        assert_eq!(pipeline.url, "new");
    }

    #[test]
    fn no_bot_comment_means_no_pipeline() {
        let comments = json!({ "nodes": [{ "author": null, "updatedAt": "2026-10-05T10:00:00Z", "body": "hi" }] });

        assert!(Pipeline::latest(&comments, &settings()).is_none());
    }

    const PREPROD_DONE: &str = "# 🚀 Iniciando pipeline
## 🌟 CI: OK!
## 💡 Info: Readiness check en paralelo
## 💡 Info pre-prod: Creando nueva revisión
## 💡 Info prod: Creando nueva revisión
## 💡 Info prod: Readiness check terminado
## 💡 Info pre-prod: Readiness check terminado
## 🚀 Logs de pre-prod
## 💡 Info: Cambio de tráfico
## 🚀 Info: El ambiente pre-prod ya recibe tráfico
## 💡 Info: Comenzando test sintéticos
## 💡 Info: Validación de los sintéticos
## 🌟 CD: OK!";

    const PROD_LIVE: &str = "
## 🚀 Logs de prod
## 💡 Info: Cambio de tráfico
## 🚀 Info: El ambiente prod ya recibe tráfico";

    const PROD_DONE: &str = "
## 💡 Info: Validación de los sintéticos
## 🌟 CD: OK!
## 🌟 CD: 🚀 El despliegue fue aplicado correctamente";

    fn run(body: &str) -> Pipeline {
        pipeline(body, 10_000)
    }

    #[test]
    fn steps_without_an_environment_belong_to_the_one_taking_traffic() {
        let steps = parse_steps(&format!("{PREPROD_DONE}{PROD_LIVE}"), &settings().projects);

        let env_of = |title: &str| {
            steps
                .iter()
                .rev()
                .find(|step| step.title == title)
                .unwrap()
                .env
        };
        assert_eq!(env_of("🌟 CI: OK!"), None);
        assert_eq!(env_of("🌟 CD: OK!"), Some(Env::PreProd));
        assert_eq!(env_of("💡 Info: Cambio de tráfico"), Some(Env::Prod));
    }

    #[test]
    fn pre_prod_passing_its_synthetics_is_not_the_end_of_the_run() {
        let run = run(PREPROD_DONE);

        assert_eq!(run.state(10_000), State::Running);
        assert_eq!(run.milestones(), [Milestone::Ready(Env::PreProd)]);
        assert_eq!(
            run.environments(),
            [
                (Env::PreProd, EnvState::Ready),
                (Env::Prod, EnvState::Deploying)
            ]
        );
    }

    #[test]
    fn the_run_follows_prod_until_the_closing_heading() {
        let live = run(&format!("{PREPROD_DONE}{PROD_LIVE}"));
        let done = run(&format!("{PREPROD_DONE}{PROD_LIVE}{PROD_DONE}"));

        assert_eq!(live.environments()[1], (Env::Prod, EnvState::Live));
        assert_eq!(done.state(10_000), State::Succeeded);
        assert_eq!(
            done.milestones(),
            [Milestone::Ready(Env::PreProd), Milestone::Ready(Env::Prod)]
        );
    }

    #[test]
    fn a_failure_takes_its_environment_from_the_body() {
        let readiness = "## 🚨 Error: 🚀 Falló el readiness check
Revisa los [logs](https://console.cloud.google.com/logs/query?project=acme-staging) en GCP!";
        let rollback = "## 🚨 Error: El deploy falló. Rollback completado.
🌎 Ambiente: development";

        assert_eq!(
            run(readiness).milestones(),
            [Milestone::Failed(Some(Env::PreProd))]
        );
        assert_eq!(
            run(rollback).milestones(),
            [Milestone::Failed(Some(Env::Development))]
        );
    }

    #[test]
    fn a_lock_the_run_waited_out_does_not_fail_it() {
        let waited = run(&format!(
            "## 🌟 preCI: OK!\n## 🚨 Error: 🔒 Bloqueo activo\n{PREPROD_DONE}{PROD_LIVE}{PROD_DONE}"
        ));

        assert_eq!(waited.state(10_000), State::Succeeded);
        assert_eq!(
            waited.headline().unwrap().title,
            "🌟 CD: 🚀 El despliegue fue aplicado correctamente"
        );
    }

    #[test]
    fn environments_follow_the_order_they_took_traffic() {
        let body = PREPROD_DONE.replace(
            "## 💡 Info pre-prod: Creando nueva revisión\n## 💡 Info prod: Creando nueva revisión",
            "## 💡 Info prod: Creando nueva revisión\n## 💡 Info pre-prod: Creando nueva revisión",
        );

        let envs: Vec<Env> = run(&body)
            .environments()
            .into_iter()
            .map(|(env, _)| env)
            .collect();

        assert_eq!(envs, [Env::PreProd, Env::Prod]);
    }

    #[test]
    fn a_run_blocked_before_deploying_has_no_environment() {
        let blocked = run("## 🌟 preCI: OK!\n## 🚨 Error: 🚩 Faltan approves");

        assert_eq!(blocked.milestones(), [Milestone::Failed(None)]);
        assert!(blocked.environments().is_empty());
    }

    #[test]
    fn a_canary_and_an_empty_run_close_without_a_deploy() {
        let canary = run(&format!(
            "{PREPROD_DONE}\n## 🐦 Canary: 🚀 El canary está activo en producción"
        ));
        let ignored = run("## 🌟 preCI: OK!\n## 📝️ Solo hay cambios en archivos ignorados");

        assert!(canary.milestones().contains(&Milestone::Canary));
        assert_eq!(ignored.state(10_000), State::Succeeded);
        assert!(ignored.milestones().is_empty());
    }

    #[test]
    fn without_a_configured_bot_there_is_no_pipeline() {
        let comments = json!({ "nodes": [
            { "url": "run", "updatedAt": "2026-10-05T10:00:00Z", "author": { "login": "deploybot" }, "body": PREPROD_DONE }
        ]});

        assert!(Pipeline::latest(&comments, &Settings::default()).is_none());
    }

    #[test]
    fn a_project_matches_only_by_its_full_id() {
        let projects = settings().projects;

        assert_eq!(
            Env::implied_by("[logs](https://x/query?project=acme-prod)", &projects),
            Some(Env::Prod)
        );
        assert_eq!(
            Env::implied_by("[logs](https://x/query?project=acme-prod-eu)", &projects),
            None
        );
    }

    #[test]
    fn latest_skips_bot_comments_that_are_not_a_run() {
        let comments = json!({ "nodes": [
            { "url": "run", "updatedAt": "2026-10-05T10:00:00Z", "author": { "login": "deploybot" }, "body": PREPROD_DONE },
            { "url": "deps", "updatedAt": "2026-10-05T11:00:00Z", "author": { "login": "deploybot" }, "body": "# Lista de dependencias" }
        ]});

        assert_eq!(Pipeline::latest(&comments, &settings()).unwrap().url, "run");
    }

    #[test]
    fn environment_names_are_whole_words() {
        assert_eq!(Env::named_in("Info pre-prod: Creando"), Some(Env::PreProd));
        assert_eq!(Env::named_in("Logs de prod"), Some(Env::Prod));
        assert_eq!(Env::named_in("activo en producción"), None);
    }

    #[test]
    fn strips_markdown_links() {
        assert_eq!(
            strip_links("Revisa los [logs](https://x) en [GCP](y)!"),
            "Revisa los logs en GCP!"
        );
    }
}
