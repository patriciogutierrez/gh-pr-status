//! A GPUI window over the user's open pull requests on GitHub.
//!
//! Two tabs: the PRs the user authored, grouped by what they are waiting on,
//! and the PRs where someone asked the user for a review. Under each of the
//! user's PRs sits the latest CI/CD run as the pipeline bot reports it.
//!
//! Both lists reload every minute; while one of the user's deploys is running,
//! "Mis PRs" reloads every 20 seconds so the run can be followed live. macOS
//! notifications announce when a deploy finishes or fails and when a PR is
//! approved.

mod github;
mod notify;
mod pipeline;
mod status;
mod theme;
mod tool;
mod ui;

use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    App, Application, Bounds, ClipboardItem, Context, FocusHandle, Focusable, KeyBinding,
    MouseButton, MouseDownEvent, Pixels, Point, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, anchored, deferred, div, prelude::*, px, rgb, size,
};

use github::{PullRequest, Review, Search, Snapshot};
use pipeline::{Pipeline, State};
use status::Group;

actions!(app, [Refresh, ShowMine, ShowReviews, Quit]);

/// How often the lists reload without being asked. Each reload is one GraphQL
/// call per tab, one or two points of GitHub's 5,000-point hourly budget.
const REFRESH_EVERY: Duration = Duration::from_secs(60);
/// How often "Mis PRs" reloads while one of the user's deploys is running.
const DEPLOY_REFRESH_EVERY: Duration = Duration::from_secs(20);
/// How often the clock wakes up to check what is due and repaint the
/// relative times.
const TICK: Duration = Duration::from_secs(5);
/// Steps shown when a pipeline is expanded; errors are always shown on top.
const RECENT_STEPS: usize = 8;
/// The comment that starts the deploy pipeline on a PR.
const DEPLOY_COMMAND: &str = "/deploy";
/// How long the deploy button waits for the confirming second click.
const CONFIRM_FOR: Duration = Duration::from_secs(5);

/// Where the "Slack" button next to each PR goes: the channel in
/// `GH_PR_STATUS_SLACK_CHANNEL`, straight in the Slack app when
/// `GH_PR_STATUS_SLACK_TEAM` names the workspace, otherwise through slack.com's
/// redirect. Workflow links only start from inside Slack, so the button opens
/// the channel and copies the PR link for the workflow's form. Without a
/// channel, there is no button.
fn slack_channel() -> Option<&'static str> {
    static LINK: OnceLock<Option<String>> = OnceLock::new();
    LINK.get_or_init(|| {
        let var = |name: &str| {
            env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let channel = var("GH_PR_STATUS_SLACK_CHANNEL")?;
        Some(match var("GH_PR_STATUS_SLACK_TEAM") {
            Some(team) => format!("slack://channel?team={team}&id={channel}"),
            None => format!("https://slack.com/app_redirect?channel={channel}"),
        })
    })
    .as_deref()
}

/// Where a PR's `/deploy` comment is. A PR missing from the map has none going.
enum Deploy {
    /// First click done; a second one before the deadline sends the comment.
    Confirming(Instant),
    Sending,
    Failed(String),
}

/// The menu a right click on a PR opens, where the click happened.
struct Menu {
    url: String,
    at: Point<Pixels>,
}

/// What a tab last got back from `gh`.
enum Load {
    Loading,
    Loaded(Snapshot),
    Failed(String),
}

/// One tab's data and the bookkeeping to reload it on its own schedule.
struct Feed {
    search: Search,
    load: Load,
    in_flight: bool,
    fetched_at: Option<Instant>,
}

impl Feed {
    fn new(search: Search) -> Self {
        Self {
            search,
            load: Load::Loading,
            in_flight: false,
            fetched_at: None,
        }
    }

    fn pulls(&self) -> &[PullRequest] {
        match &self.load {
            Load::Loaded(snapshot) => &snapshot.pulls,
            _ => &[],
        }
    }

    /// How many of this tab's PRs have a deploy in progress.
    fn deploying(&self, now: i64) -> usize {
        self.pulls()
            .iter()
            .filter_map(|pull| pull.pipeline.as_ref())
            .filter(|run| run.state(now) == State::Running)
            .count()
    }

    fn every(&self, now: i64) -> Duration {
        if self.deploying(now) > 0 {
            DEPLOY_REFRESH_EVERY
        } else {
            REFRESH_EVERY
        }
    }

    fn due(&self, now: i64) -> bool {
        !self.in_flight
            && self
                .fetched_at
                .is_none_or(|at| at.elapsed() >= self.every(now))
    }
}

struct Root {
    tab: Search,
    mine: Feed,
    reviews: Feed,
    /// PRs whose pipeline is unfolded, by URL so it survives reloads.
    expanded: HashSet<String>,
    /// Each PR's review and pipeline as of the last load, to notice what changed.
    /// `None` until the first load, which only sets the baseline.
    last_seen: Option<HashMap<String, notify::Seen>>,
    /// `/deploy` comments being confirmed, sent or that failed, by PR URL.
    deploys: HashMap<String, Deploy>,
    /// The right-click menu, while it is open.
    menu: Option<Menu>,
    focus_handle: FocusHandle,
}

impl Root {
    fn new(cx: &mut Context<Self>) -> Self {
        let root = Self {
            tab: Search::Mine,
            mine: Feed::new(Search::Mine),
            reviews: Feed::new(Search::ReviewRequested),
            expanded: HashSet::new(),
            last_seen: None,
            deploys: HashMap::new(),
            menu: None,
            focus_handle: cx.focus_handle(),
        };
        root.start_clock(cx);
        root
    }

    /// Wakes up every `TICK`, reloads whichever tab is due and repaints, so
    /// the relative times stay honest between reloads.
    fn start_clock(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let alive = this
                    .update(cx, |this, cx| {
                        let now = now();
                        for search in [Search::Mine, Search::ReviewRequested] {
                            if this.feed(search).due(now) {
                                this.reload(search, cx);
                            }
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    // The view is gone; nothing left to keep fresh.
                    break;
                }
                cx.background_executor().timer(TICK).await;
            }
        })
        .detach();
    }

    fn feed(&self, search: Search) -> &Feed {
        match search {
            Search::Mine => &self.mine,
            Search::ReviewRequested => &self.reviews,
        }
    }

    fn feed_mut(&mut self, search: Search) -> &mut Feed {
        match search {
            Search::Mine => &mut self.mine,
            Search::ReviewRequested => &mut self.reviews,
        }
    }

    fn refresh(&mut self, _: &Refresh, cx: &mut Context<Self>) {
        self.reload(Search::Mine, cx);
        self.reload(Search::ReviewRequested, cx);
    }

    fn reload(&mut self, search: Search, cx: &mut Context<Self>) {
        let feed = self.feed_mut(search);
        if feed.in_flight {
            return;
        }
        feed.in_flight = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { github::fetch(search) })
                .await;

            this.update(cx, |this, cx| {
                if let (Search::Mine, Ok(snapshot)) = (search, &result) {
                    this.notify_changes(&snapshot.pulls, cx);
                }
                let feed = this.feed_mut(search);
                feed.load = into_load(result);
                feed.in_flight = false;
                feed.fetched_at = Some(Instant::now());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn notify_changes(&mut self, pulls: &[PullRequest], cx: &mut Context<Self>) {
        let (notices, seen) = notify::transitions(self.last_seen.as_ref(), pulls);
        self.last_seen = Some(seen);

        if !notices.is_empty() {
            cx.background_executor()
                .spawn(async move { notices.iter().for_each(notify::send) })
                .detach();
        }
    }

    fn show(&mut self, tab: Search, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    fn toggle(&mut self, url: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(url) {
            self.expanded.insert(url.to_string());
        }
        cx.notify();
    }

    /// The deploy button's click: the first one asks for confirmation, the
    /// second one, within `CONFIRM_FOR`, comments `/deploy` on the PR and
    /// reloads "Mis PRs" so the bot's run shows up.
    fn deploy(&mut self, url: &str, cx: &mut Context<Self>) {
        let confirmed = matches!(
            self.deploys.get(url),
            Some(Deploy::Confirming(at)) if at.elapsed() < CONFIRM_FOR
        );
        if !confirmed {
            self.deploys
                .insert(url.to_string(), Deploy::Confirming(Instant::now()));
            cx.notify();
            return;
        }

        self.deploys.insert(url.to_string(), Deploy::Sending);
        cx.notify();

        let url = url.to_string();
        cx.spawn(async move |this, cx| {
            let target = url.clone();
            let result = cx
                .background_executor()
                .spawn(async move { github::comment(&target, DEPLOY_COMMAND) })
                .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.deploys.remove(&url);
                        this.reload(Search::Mine, cx);
                    }
                    Err(error) => {
                        this.deploys.insert(url, Deploy::Failed(error));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The buttons to the right of a PR: ask for a review on Slack until the PR
    /// is approved, and comment `/deploy` unless a run is already going. Only
    /// on the user's own PRs.
    fn actions(&self, pull: &PullRequest, cx: &mut Context<Self>) -> impl IntoElement {
        let mine = pull.author == self.login().unwrap_or_default();
        let slack = slack_channel().filter(|_| mine && pull.review != Review::Approved);
        let deploying = pull
            .pipeline
            .as_ref()
            .is_some_and(|run| run.state(now()) == State::Running);
        let deploy = self.deploys.get(&pull.url);
        let label = match deploy {
            Some(Deploy::Confirming(at)) if at.elapsed() < CONFIRM_FOR => "¿Enviar /deploy?",
            Some(Deploy::Sending) => "Enviando…",
            _ => "/deploy",
        };
        let url = pull.url.clone();

        div()
            .flex()
            .flex_shrink_0()
            .gap_2()
            .when_some(slack, |this, channel| {
                let pull_url = pull.url.clone();
                this.child(ui::button(
                    SharedString::from(format!("slack-{}", pull.url)),
                    "Slack",
                    true,
                    move |_event, _window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(pull_url.clone()));
                        cx.open_url(channel);
                    },
                ))
            })
            .when(mine && !pull.draft && !deploying, |this| {
                this.child(ui::button(
                    SharedString::from(format!("deploy-{}", pull.url)),
                    label,
                    !matches!(deploy, Some(Deploy::Sending)),
                    cx.listener(move |this, _event, _window, cx| this.deploy(&url, cx)),
                ))
            })
    }

    /// The right-click menu, drawn over everything at the click. Any click
    /// outside it closes it.
    fn menu(&self, menu: &Menu, cx: &mut Context<Self>) -> impl IntoElement {
        let url = menu.url.clone();

        deferred(
            anchored().position(menu.at).snap_to_window().child(
                div()
                    .occlude()
                    .flex()
                    .flex_col()
                    .p_1()
                    .min_w(px(180.))
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::SURFACE))
                    .shadow_lg()
                    .text_sm()
                    .on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .id("menu-copy-url")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
                            .child("Copiar URL del PR")
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                                this.menu = None;
                                cx.notify();
                            })),
                    ),
            ),
        )
        .with_priority(1)
    }

    fn refreshing(&self) -> bool {
        self.mine.in_flight || self.reviews.in_flight
    }

    fn login(&self) -> Option<&str> {
        [&self.mine, &self.reviews]
            .into_iter()
            .find_map(|feed| match &feed.load {
                Load::Loaded(snapshot) if !snapshot.login.is_empty() => {
                    Some(snapshot.login.as_str())
                }
                _ => None,
            })
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut caption = match (self.login(), self.mine.fetched_at) {
            (Some(login), Some(at)) => format!(
                "@{login} · actualizado {}",
                status::ago(at.elapsed().as_secs() as i64)
            ),
            _ => "consultando GitHub…".to_string(),
        };
        let deploying = self.mine.deploying(now());
        if deploying > 0 {
            caption.push_str(&format!(
                " · siguiendo {deploying} despliegue{} cada {} s",
                if deploying == 1 { "" } else { "s" },
                DEPLOY_REFRESH_EVERY.as_secs()
            ));
        }

        div()
            .flex()
            .items_end()
            .gap_4()
            .child(div().flex_1().child(ui::heading("Pull requests", caption)))
            .child(ui::button(
                "refresh",
                if self.refreshing() {
                    "Actualizando…"
                } else {
                    "Actualizar"
                },
                !self.refreshing(),
                cx.listener(|this, _event, _window, cx| this.refresh(&Refresh, cx)),
            ))
    }

    fn tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = |feed: &Feed| match &feed.load {
            Load::Loaded(snapshot) => format!(" ({})", snapshot.pulls.len()),
            _ => String::new(),
        };

        div()
            .flex()
            .gap_2()
            .child(ui::toggle_button(
                "tab-mine",
                format!("Mis PRs{}", count(&self.mine)),
                self.tab == Search::Mine,
                cx.listener(|this, _event, _window, cx| this.show(Search::Mine, cx)),
            ))
            .child(ui::toggle_button(
                "tab-reviews",
                format!("Me piden review{}", count(&self.reviews)),
                self.tab == Search::ReviewRequested,
                cx.listener(|this, _event, _window, cx| this.show(Search::ReviewRequested, cx)),
            ))
    }

    fn body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let feed = self.feed(self.tab);

        match &feed.load {
            Load::Loading => message("Cargando…", theme::SUBTEXT).into_any_element(),
            Load::Failed(error) => message(error.clone(), theme::RED).into_any_element(),
            Load::Loaded(snapshot) if snapshot.pulls.is_empty() => message(
                match feed.search {
                    Search::Mine => "No tienes PRs abiertos.",
                    Search::ReviewRequested => "Nadie te está pidiendo review.",
                },
                theme::SUBTEXT,
            )
            .into_any_element(),
            Load::Loaded(snapshot) => match feed.search {
                Search::Mine => self.mine_list(&snapshot.pulls, cx).into_any_element(),
                Search::ReviewRequested => self.review_list(&snapshot.pulls, cx).into_any_element(),
            },
        }
    }

    /// The user's own PRs: a summary of the groups, then one section per group.
    fn mine_list(&self, pulls: &[PullRequest], cx: &mut Context<Self>) -> impl IntoElement {
        let now = now();
        let in_group = |group: Group| {
            pulls
                .iter()
                .filter(move |pull| Group::of(pull, now) == group)
        };

        let sections: Vec<_> = Group::ALL
            .into_iter()
            .filter_map(|group| {
                let rows: Vec<_> = in_group(group)
                    .map(|pull| self.row(pull, group.color(), false, cx).into_any_element())
                    .collect();
                (!rows.is_empty()).then(|| section(group.title(), group.color(), rows))
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .gap_4()
            .flex_1()
            .min_h(px(0.))
            .child(
                div().flex().gap_2().children(
                    Group::ALL.map(|group| {
                        ui::stat(in_group(group).count(), group.title(), group.color())
                    }),
                ),
            )
            .child(list("mine").children(sections))
    }

    /// PRs waiting on the user's review, in GitHub's most-recently-updated order.
    fn review_list(&self, pulls: &[PullRequest], cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = pulls
            .iter()
            .map(|pull| self.row(pull, theme::BLUE, true, cx).into_any_element())
            .collect();
        list("reviews").child(section("Te piden review", theme::BLUE, rows))
    }

    /// One PR. The colored stripe repeats the section's color so a row keeps its
    /// meaning when scrolled away from the heading. The PR and its pipeline are
    /// separate click targets: one opens GitHub, the other unfolds the run.
    fn row(
        &self,
        pull: &PullRequest,
        stripe: u32,
        show_author: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expanded = self.expanded.contains(&pull.url);

        div()
            .flex()
            .gap_3()
            .p_2()
            .rounded_lg()
            .bg(rgb(theme::SURFACE))
            .child(div().w(px(3.)).flex_shrink_0().rounded_sm().bg(rgb(stripe)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_w(px(0.))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_start()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .on_mouse_down(MouseButton::Right, {
                                        let url = pull.url.clone();
                                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                            this.menu = Some(Menu {
                                                url: url.clone(),
                                                at: event.position,
                                            });
                                            cx.notify();
                                        })
                                    })
                                    .child(summary(pull, show_author)),
                            )
                            .child(self.actions(pull, cx)),
                    )
                    .when_some(
                        match self.deploys.get(&pull.url) {
                            Some(Deploy::Failed(error)) => Some(error.clone()),
                            _ => None,
                        },
                        |this, error| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::RED))
                                    .child(format!("No se pudo enviar /deploy: {error}")),
                            )
                        },
                    )
                    .when_some(pull.pipeline.as_ref(), |this, run| {
                        this.child(self.pipeline_bar(pull, run, expanded, cx))
                            .when(expanded, |this| this.child(steps(pull, run)))
                    }),
            )
    }

    /// The collapsed pipeline line: state, the step worth reading, and when.
    fn pipeline_bar(
        &self,
        pull: &PullRequest,
        run: &Pipeline,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now = now();
        let state = status::pipeline_state(run.state(now));
        let headline = run.headline();
        let url = pull.url.clone();

        div()
            .id(SharedString::from(format!("pipeline-{}", pull.url)))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(theme::MANTLE))
            .text_xs()
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
            .on_click(cx.listener(move |this, _event, _window, cx| this.toggle(&url, cx)))
            .child(
                div()
                    .text_color(rgb(theme::MUTED))
                    .child(if expanded { "▾" } else { "▸" }),
            )
            .child(ui::badge(state.label, state.color))
            .children(run.environments().into_iter().map(|(env, env_state)| {
                let badge = status::environment(env, env_state);
                ui::badge(badge.label, badge.color)
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_color(rgb(
                        headline.map_or(theme::SUBTEXT, |step| status::step_color(step.kind))
                    ))
                    .child(headline.map_or_else(String::new, |step| step.title.clone())),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(rgb(theme::MUTED))
                    .child(status::ago(now - run.updated_at)),
            )
    }
}

fn into_load(result: Result<Snapshot, String>) -> Load {
    match result {
        Ok(snapshot) => Load::Loaded(snapshot),
        Err(error) => Load::Failed(error),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn message(text: impl Into<SharedString>, color: u32) -> impl IntoElement {
    div()
        .p_4()
        .rounded_lg()
        .bg(rgb(theme::SURFACE))
        .text_sm()
        .text_color(rgb(color))
        .child(text.into())
}

/// The scrolling area both tabs draw into.
fn list(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .gap_4()
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
}

fn section(title: &str, color: u32, rows: Vec<gpui::AnyElement>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .text_xs()
                .text_color(rgb(color))
                .child(title.to_uppercase())
                .child(
                    div()
                        .text_color(rgb(theme::MUTED))
                        .child(rows.len().to_string()),
                ),
        )
        .children(rows)
}

/// Repo, branches, title and badges. Clicking it opens the PR on GitHub.
fn summary(pull: &PullRequest, show_author: bool) -> impl IntoElement {
    let url = pull.url.clone();
    let mut context = format!(
        "{} #{} · {} ← {}",
        pull.repo_name(),
        pull.number,
        pull.base,
        pull.head
    );
    if show_author {
        context = format!("@{} · {context}", pull.author);
    }

    div()
        .id(SharedString::from(format!("open-{}", pull.url)))
        .flex()
        .flex_col()
        .gap_1()
        .p_1()
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
        .active(|style| style.bg(rgb(theme::SURFACE_ACTIVE)))
        .on_click(move |_event, _window, cx| cx.open_url(&url))
        .child(
            div()
                .flex()
                .gap_3()
                .text_xs()
                .text_color(rgb(theme::MUTED))
                .child(div().flex_1().min_w(px(0.)).truncate().child(context))
                .child(
                    div()
                        .flex_shrink_0()
                        .child(status::ago(now() - pull.updated_at)),
                ),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme::TEXT))
                .truncate()
                .child(pull.title.clone()),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .items_center()
                .children(
                    status::badges(pull)
                        .into_iter()
                        .map(|badge| ui::badge(badge.label, badge.color)),
                )
                .when(pull.comments > 0, |this| {
                    this.child(ui::badge(format!("💬 {}", pull.comments), theme::SUBTEXT))
                })
                .when(!pull.failing_checks.is_empty(), |this| {
                    this.child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_xs()
                            .text_color(rgb(theme::RED))
                            .child(format!("falla: {}", pull.failing_checks.join(", "))),
                    )
                }),
        )
}

/// The unfolded run: every error plus the latest steps, oldest first, and a
/// link to the bot's comment for everything else.
fn steps(pull: &PullRequest, run: &Pipeline) -> impl IntoElement {
    let total = run.steps.len();
    let first_recent = total.saturating_sub(RECENT_STEPS);
    let shown: Vec<_> = run
        .steps
        .iter()
        .enumerate()
        .filter(|(index, step)| *index >= first_recent || step.kind == pipeline::StepKind::Error)
        .map(|(_, step)| step)
        .collect();
    let hidden = total - shown.len();
    let comment = run.url.clone();

    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded_md()
        .bg(rgb(theme::MANTLE))
        .text_xs()
        .when(hidden > 0, |this| {
            this.child(
                div()
                    .text_color(rgb(theme::MUTED))
                    .child(format!("… {hidden} etapas anteriores")),
            )
        })
        .children(shown.into_iter().map(|step| {
            div()
                .flex()
                .gap_2()
                .child(
                    div()
                        .w(px(68.))
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .font_family("Menlo")
                        .text_color(rgb(theme::MUTED))
                        .child(step.time.clone().unwrap_or_default()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.))
                        .child(
                            div()
                                .truncate()
                                .text_color(rgb(status::step_color(step.kind)))
                                .child(step.title.clone()),
                        )
                        .children(step.details.iter().map(|detail| {
                            div()
                                .truncate()
                                .text_color(rgb(theme::SUBTEXT))
                                .child(detail.clone())
                        })),
                )
        }))
        .child(div().pt_1().child(ui::button(
            SharedString::from(format!("comment-{}", pull.url)),
            "Abrir comentario del bot",
            !comment.is_empty(),
            move |_event, _window, cx| cx.open_url(&comment),
        )))
}

impl Focusable for Root {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Root")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, action, _window, cx| this.refresh(action, cx)))
            .on_action(cx.listener(|this, _: &ShowMine, _window, cx| this.show(Search::Mine, cx)))
            .on_action(cx.listener(|this, _: &ShowReviews, _window, cx| {
                this.show(Search::ReviewRequested, cx)
            }))
            .flex()
            .flex_col()
            .gap_4()
            .size_full()
            .p_5()
            .bg(rgb(theme::BASE))
            .text_color(rgb(theme::TEXT))
            .child(self.header(cx))
            .child(self.tabs(cx))
            .child(self.body(cx))
            .when_some(self.menu.as_ref(), |this, menu| this.child(self.menu(menu, cx)))
            .child(div().text_xs().text_color(rgb(theme::MUTED)).child(
                "clic en el PR lo abre · clic derecho copia su URL · clic en el pipeline lo despliega · cmd-r actualiza · cmd-1 / cmd-2 cambian de pestaña",
            ))
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-r", Refresh, Some("Root")),
            KeyBinding::new("cmd-1", ShowMine, Some("Root")),
            KeyBinding::new("cmd-2", ShowReviews, Some("Root")),
        ]);

        let bounds = Bounds::centered(None, size(px(760.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Pull requests")),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                let root = cx.new(Root::new);
                // Nothing in the window is focusable on its own, so the root
                // takes focus for the keybindings to reach it.
                window.focus(&root.read(cx).focus_handle);
                root
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
