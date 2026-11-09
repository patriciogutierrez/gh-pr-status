//! A GPUI window over the user's open pull requests on GitHub.
//!
//! Two tabs: the PRs the user authored, grouped by what they are waiting on,
//! and the user's merged PRs. Under each of the user's PRs sits the latest CI/CD run as the pipeline
//! bot reports it. A sidebar holds the groups the user drags PRs into; picking
//! one narrows every tab to it. Each group can open an agent in Orca, and the
//! PRs from the worktrees that agent makes join the group on their own.
//!
//! The open PRs reload every minute; while one of the user's deploys is running,
//! "Mis PRs" reloads every 20 seconds so the run can be followed live. macOS
//! notifications announce when a deploy finishes or fails and when a PR is
//! approved.

mod db;
mod dock;
mod folders;
mod github;
mod merged;
mod notify;
mod orca;
mod pipeline;
mod review;
mod review_view;
mod slack;
mod stack;
mod status;
mod text_input;
mod theme;
mod tool;
mod ui;

use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnimationExt, App, Application, Bounds, ClipboardItem, Context, FocusHandle, Focusable,
    FontWeight, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, SharedString,
    TitlebarOptions, Window, WindowBounds, WindowOptions, actions, anchored, deferred, div,
    prelude::*, px, rgb, size,
};

use github::{PullRequest, Review, Snapshot};
use pipeline::{Pipeline, State};
use status::Group;

actions!(app, [Refresh, ShowMine, ShowMerged, Quit]);

/// How often the lists reload without being asked. Each reload is one GraphQL
/// call per tab, one or two points of GitHub's 5,000-point hourly budget.
const REFRESH_EVERY: Duration = Duration::from_secs(60);
/// How often "Mis PRs" reloads while one of the user's deploys is running.
const DEPLOY_REFRESH_EVERY: Duration = Duration::from_secs(20);
/// How often the merged list asks GitHub for today's merges; past days come
/// from the local file.
const MERGED_REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often the clock wakes up to check what is due and repaint the
/// relative times.
const TICK: Duration = Duration::from_secs(5);
/// Steps shown when a pipeline is expanded; errors are always shown on top.
const RECENT_STEPS: usize = 8;
/// The comment that starts the deploy pipeline on a PR.
const DEPLOY_COMMAND: &str = "/deploy";
/// The sidebar's width until the user drags its edge, and how far it goes.
const SIDEBAR_WIDTH: f32 = 180.;
const SIDEBAR_MIN: f32 = 140.;
const SIDEBAR_MAX: f32 = 480.;
/// The strip between the sidebar and the PRs that resizes the sidebar.
const HANDLE_WIDTH: f32 = 16.;
/// The room a sidebar entry's count takes; with padding, border and gap it
/// leaves the rest of the sidebar's width to the name.
const COUNT_WIDTH: f32 = 20.;
const ENTRY_CHROME: f32 = 2. * 8. + 2. + 8. + COUNT_WIDTH;
/// The room a group's fold arrow takes, with the gap after it.
const FOLD_WIDTH: f32 = 12. + 8.;
/// How far a subgroup sits in from its group, past the group's arrow.
const SUBGROUP_INDENT: f32 = FOLD_WIDTH + 6.;
/// How far a PR's lines sit in from its row, to start under its title: the
/// state dot's slot and the gap after it.
const TITLE_INDENT: f32 = DOT_SLOT + 8.;
/// The room the state dot takes, wide enough for the ring around it.
const DOT_SLOT: f32 = 16.;

/// How wide each column of a group's board is.
const BOARD_COLUMN: f32 = 300.;
/// The board's scrollbar: its height and the room kept under the columns.
const SCROLLBAR_HEIGHT: f32 = 6.;
/// A hovered name that doesn't fit waits `MARQUEE_WAIT`, slides left at
/// `MARQUEE_SPEED` pixels a second until its end shows, holds `MARQUEE_HOLD`,
/// slides back in `MARQUEE_RETURN`, and starts over.
const MARQUEE_SPEED: f32 = 30.;
const MARQUEE_WAIT: f32 = 1.;
const MARQUEE_HOLD: f32 = 2.;
const MARQUEE_RETURN: f32 = 0.4;

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
    Sending,
    Failed(String),
}

/// What a right click opened its menu on.
#[derive(Clone)]
enum MenuFor {
    Pull(String),
    Folder(i64),
}

/// The menu a right click opens, where the click happened.
struct Menu {
    target: MenuFor,
    at: Point<Pixels>,
}

/// Which PRs the sidebar lets through to the tabs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Filter {
    All,
    /// The PRs in no group.
    Loose,
    Folder(i64),
}

/// A group name being typed in the sidebar.
struct Editor {
    target: Editing,
    text: String,
}

enum Editing {
    /// A new group, which gets `pull` once created when it was started by
    /// dropping that PR on "Nuevo grupo"; a subgroup of `parent` when it has one.
    New {
        pull: Option<String>,
        parent: Option<i64>,
    },
    Rename(i64),
}

/// What the handle carries while the sidebar's edge is dragged. It draws
/// nothing; the sidebar itself follows the cursor.
struct ResizeSidebar;

impl Render for ResizeSidebar {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// What the board's scrollbar thumb carries while dragged; the board itself
/// follows the cursor.
struct ScrollThumb;

impl Render for ScrollThumb {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// What the board carries while its empty space is dragged to pan it.
struct PanBoard;

impl Render for PanBoard {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// What a PR row carries while it is dragged onto a group.
#[derive(Clone)]
struct Dragged {
    url: String,
    title: SharedString,
}

/// The chip that follows the cursor while a PR is dragged.
struct DragPreview {
    title: SharedString,
    /// Where the cursor grabbed the row, from the row's corner.
    at: Point<Pixels>,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().pl(self.at.x).pt(self.at.y).child(
            div()
                .max_w(px(280.))
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::ACCENT))
                .bg(rgb(theme::SURFACE_ACTIVE))
                .text_xs()
                .text_color(rgb(theme::TEXT))
                .shadow_md()
                .truncate()
                .child(self.title.clone()),
        )
    }
}

/// What a tab last got back from `gh`.
enum Load {
    Loading,
    Loaded(Snapshot),
    Failed(String),
}

/// The open PRs and the bookkeeping to reload them on their own schedule.
struct Feed {
    load: Load,
    in_flight: bool,
    fetched_at: Option<Instant>,
}

impl Feed {
    fn new() -> Self {
        Self {
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

    /// How many of the PRs have a deploy in progress.
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Tab {
    Mine,
    Merged,
}

/// The merged PRs as stored locally, and how the last sync went.
#[derive(Default)]
struct MergedFeed {
    /// `None` until the local file has been read once.
    pulls: Option<Vec<merged::Merged>>,
    /// Why the last sync or read failed; the stored rows are still shown.
    error: Option<String>,
    in_flight: bool,
    fetched_at: Option<Instant>,
}

impl MergedFeed {
    fn due(&self) -> bool {
        !self.in_flight
            && self
                .fetched_at
                .is_none_or(|at| at.elapsed() >= MERGED_REFRESH_EVERY)
    }
}

struct Root {
    tab: Tab,
    mine: Feed,
    merged: MergedFeed,
    /// PRs whose pipeline is unfolded, by URL so it survives reloads.
    expanded: HashSet<String>,
    /// Each PR's review and pipeline as of the last load, to notice what changed.
    /// `None` until the first load, which only sets the baseline.
    last_seen: Option<HashMap<String, notify::Seen>>,
    /// `/deploy` comments being confirmed, sent or that failed, by PR URL.
    deploys: HashMap<String, Deploy>,
    /// The right-click menu, while it is open.
    menu: Option<Menu>,
    /// The user's groups as stored locally.
    folders: folders::Folders,
    /// Why reading or saving the groups last failed.
    folders_error: Option<String>,
    filter: Filter,
    editor: Option<Editor>,
    /// The sidebar entry under the mouse, whose name scrolls if it is cut.
    hovered: Option<Filter>,
    sidebar_width: f32,
    /// The group whose agent is being opened in Orca.
    opening_agent: Option<i64>,
    /// PRs whose review threads are unfolded, by URL.
    threads_open: HashSet<String>,
    /// Where the board is scrolled to, for its scrollbar.
    board_scroll: gpui::ScrollHandle,
    /// Whether the scrollbar's thumb is being dragged; hover doesn't count
    /// during a drag, and the bar must not vanish under the cursor.
    board_dragging: bool,
    /// Where the mouse went down on the board and how far it was scrolled
    /// then, so dragging the board's empty space pans it.
    pan_from: Option<(f32, f32)>,
    /// The reviews asked for through Slack, by PR URL.
    review_asked: HashMap<String, review::Asked>,
    /// What the Dock icon's badge shows, so it is only set when it changes.
    badge: usize,
    focus_handle: FocusHandle,
}

impl Root {
    fn new(cx: &mut Context<Self>) -> Self {
        let root = Self {
            tab: Tab::Mine,
            mine: Feed::new(),
            merged: MergedFeed::default(),
            expanded: HashSet::new(),
            last_seen: None,
            deploys: HashMap::new(),
            menu: None,
            folders: folders::Folders::default(),
            folders_error: None,
            filter: Filter::All,
            editor: None,
            hovered: None,
            sidebar_width: SIDEBAR_WIDTH,
            opening_agent: None,
            threads_open: HashSet::new(),
            board_scroll: gpui::ScrollHandle::new(),
            board_dragging: false,
            pan_from: None,
            review_asked: HashMap::new(),
            badge: 0,
            focus_handle: cx.focus_handle(),
        };
        root.start_clock(cx);
        Self::load_folders(cx);
        Self::load_sidebar_width(cx);
        Self::load_review_asked(cx);
        root
    }

    fn load_review_asked(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { review::asked() })
                .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(asked) => this.review_asked = asked,
                    Err(error) => this.folders_error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Wakes up every `TICK`, reloads whichever tab is due and repaints, so
    /// the relative times stay honest between reloads.
    fn start_clock(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let alive = this
                    .update(cx, |this, cx| {
                        let now = now();
                        if this.mine.due(now) {
                            this.reload(cx);
                        }
                        if this.merged.due() {
                            this.reload_merged(cx);
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

    fn refresh(&mut self, _: &Refresh, cx: &mut Context<Self>) {
        self.reload(cx);
        self.reload_merged(cx);
    }

    /// Syncs today's merges into the local file, then reads it back. A failed
    /// sync still shows what is stored.
    fn reload_merged(&mut self, cx: &mut Context<Self>) {
        if self.merged.in_flight {
            return;
        }
        self.merged.in_flight = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let (pulls, error) = cx
                .background_executor()
                .spawn(async move {
                    let synced = merged::sync();
                    match merged::load() {
                        Ok(pulls) => (Some(pulls), synced.err()),
                        Err(error) => (None, Some(error)),
                    }
                })
                .await;

            this.update(cx, |this, cx| {
                let feed = &mut this.merged;
                if pulls.is_some() {
                    feed.pulls = pulls;
                }
                feed.error = error;
                feed.in_flight = false;
                feed.fetched_at = Some(Instant::now());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let feed = &mut self.mine;
        if feed.in_flight {
            return;
        }
        feed.in_flight = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let (result, worktrees) = cx
                .background_executor()
                .spawn(async move { (github::fetch(), orca::worktrees()) })
                .await;

            this.update(cx, |this, cx| {
                if let Ok(snapshot) = &result {
                    this.notify_changes(&snapshot.pulls, cx);
                    this.update_badge(&snapshot.pulls);
                    this.refresh_replies(&snapshot.pulls, cx);
                    // Without Orca there are no worktrees, and nothing to claim.
                    if let Ok(worktrees) = worktrees {
                        this.claim(&snapshot.pulls, &worktrees, cx);
                    }
                }
                let feed = &mut this.mine;
                feed.load = into_load(result);
                feed.in_flight = false;
                feed.fetched_at = Some(Instant::now());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Puts on the Dock icon how many of the user's PRs are ready to merge.
    fn update_badge(&mut self, pulls: &[PullRequest]) {
        let now = now();
        let ready = pulls
            .iter()
            .filter(|pull| Group::of(pull, now) == Group::Ready)
            .count();
        if ready != self.badge {
            self.badge = ready;
            dock::set_badge(ready);
        }
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

    /// Puts each new PR from a group's worktree in that group.
    fn claim(
        &mut self,
        pulls: &[PullRequest],
        worktrees: &[orca::Worktree],
        cx: &mut Context<Self>,
    ) {
        for (url, id) in orca::claims(pulls, worktrees, &self.folders) {
            self.folders.by_url.insert(url.clone(), id);
            self.folders.claimed.insert(url.clone());
            self.save(cx, move || folders::claim(&url, id));
        }
    }

    /// Opens the group's agent in Orca, or goes back to the one already open.
    /// A subgroup opens its group's.
    fn open_agent(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(folder) = self.folders.root(id).cloned() else {
            return;
        };
        let subgroups = self.subgroups(folder.id);
        if self.opening_agent.is_some() {
            return;
        }
        self.opening_agent = Some(id);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { orca::open_agent(&folder, &subgroups) })
                .await;

            this.update(cx, |this, cx| {
                this.opening_agent = None;
                this.folders_error = result.err();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Hands the PR's next action to its group's agent. A PR in no group has
    /// no agent to ask.
    fn ask_agent(&mut self, url: &str, cx: &mut Context<Self>) {
        let Some(pull) = self
            .mine
            .pulls()
            .iter()
            .find(|pull| pull.url == url)
            .cloned()
        else {
            return;
        };
        let below = stack::below(&pull, self.mine.pulls());
        let Some(task) = status::action(&pull, below, now()).task else {
            return;
        };
        let folder = self
            .folders
            .by_url
            .get(url)
            .and_then(|id| self.folders.root(*id))
            .cloned();
        let Some(folder) = folder else {
            self.folders_error =
                Some("Pon el PR en un grupo para pedírselo al agente del grupo.".to_string());
            cx.notify();
            return;
        };
        if self.opening_agent.is_some() {
            return;
        }
        let subgroups = self.subgroups(folder.id);
        self.opening_agent = Some(folder.id);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { orca::ask_agent(&folder, &subgroups, &pull, &task) })
                .await;

            this.update(cx, |this, cx| {
                this.opening_agent = None;
                this.folders_error = result.err();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens the review form for the PR in its own window.
    fn open_review(&mut self, url: &str, cx: &mut Context<Self>) {
        let Some(pull) = self.mine.pulls().iter().find(|pull| pull.url == url) else {
            return;
        };
        let Some(config) = review::config() else {
            return;
        };
        let (url, title) = (pull.url.clone(), pull.title.clone());
        let root = cx.entity().downgrade();
        self.folders_error = review_view::open(root, config, url, title, cx).err();
        cx.notify();
    }

    fn subgroups(&self, id: i64) -> Vec<folders::Folder> {
        self.folders.children(id).cloned().collect()
    }

    /// Tells the agent of `id`'s group about its subgroups as they are now.
    fn sync_subgroups(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(folder) = self.folders.root(id).cloned() else {
            return;
        };
        let subgroups = self.subgroups(folder.id);
        self.save(cx, move || orca::sync_subgroups(&folder, &subgroups));
    }

    fn show(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    fn toggle(&mut self, url: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(url) {
            self.expanded.insert(url.to_string());
        }
        cx.notify();
    }

    fn toggle_threads(&mut self, url: &str, cx: &mut Context<Self>) {
        if !self.threads_open.remove(url) {
            self.threads_open.insert(url.to_string());
        }
        cx.notify();
    }

    /// The PR's unresolved review threads: a line to unfold them, then each
    /// thread's author, place and gist. Clicking a thread opens it on GitHub.
    fn threads(&self, pull: &PullRequest, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if pull.threads.is_empty() {
            return Vec::new();
        }
        let open = self.threads_open.contains(&pull.url);
        let url = pull.url.clone();
        let count = pull.threads.len();
        let mut lines = vec![
            div()
                .id(SharedString::from(format!("threads-{}", pull.url)))
                .ml(px(TITLE_INDENT))
                .px_1()
                .rounded_md()
                .text_xs()
                .text_color(rgb(theme::SUBTEXT))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::SURFACE)))
                .on_click(
                    cx.listener(move |this, _event, _window, cx| this.toggle_threads(&url, cx)),
                )
                .child(format!(
                    "{} {count} hilo{} sin resolver",
                    if open { "▾" } else { "▸" },
                    if count == 1 { "" } else { "s" }
                ))
                .into_any_element(),
        ];
        if !open {
            return lines;
        }
        for (index, thread) in pull.threads.iter().enumerate() {
            let at = match thread.line {
                Some(line) => format!("{}:{line}", thread.path),
                None => thread.path.clone(),
            };
            let link = thread.url.clone();
            lines.push(
                div()
                    .id(SharedString::from(format!("thread-{index}-{}", pull.url)))
                    .ml(px(TITLE_INDENT))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(theme::MANTLE))
                    .text_xs()
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme::SURFACE)))
                    .on_click(move |_event, _window, cx| cx.open_url(&link))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .text_color(rgb(theme::MUTED))
                            .child(format!("@{}", thread.author))
                            .child(div().flex_1().min_w(px(0.)).truncate().child(at))
                            .when(thread.replies > 0, |this| {
                                this.child(format!(
                                    "{} respuesta{}",
                                    thread.replies,
                                    if thread.replies == 1 { "" } else { "s" }
                                ))
                            }),
                    )
                    .child(
                        div()
                            .text_color(rgb(theme::SUBTEXT))
                            .line_clamp(2)
                            .child(thread.body.clone()),
                    )
                    .into_any_element(),
            );
        }
        lines
    }

    /// Reads the groups from disk, replacing whatever is in memory.
    fn load_folders(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { folders::load() })
                .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(loaded) => {
                        if let Filter::Folder(id) = this.filter
                            && !loaded.list.iter().any(|folder| folder.id == id)
                        {
                            this.filter = Filter::All;
                        }
                        this.folders = loaded;
                    }
                    Err(error) => this.folders_error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_sidebar_width(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { folders::sidebar_width() })
                .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(Some(width)) => this.sidebar_width = width.clamp(SIDEBAR_MIN, SIDEBAR_MAX),
                    Ok(None) => {}
                    Err(error) => this.folders_error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Follows the cursor while the sidebar's edge is dragged; `left` is where
    /// the sidebar starts.
    fn resize_sidebar(&mut self, cursor: Pixels, left: Pixels, cx: &mut Context<Self>) {
        let width = (f32::from(cursor - left) - HANDLE_WIDTH / 2.).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        if width != self.sidebar_width {
            self.sidebar_width = width;
            cx.notify();
        }
    }

    /// The strip between the sidebar and the PRs. Dragging it resizes the
    /// sidebar; the width is saved once it is let go.
    fn sidebar_handle(&self) -> impl IntoElement {
        div()
            .id("sidebar-handle")
            .group("sidebar-handle")
            .flex()
            .justify_center()
            .w(px(HANDLE_WIDTH))
            .flex_shrink_0()
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .on_drag(ResizeSidebar, |_, _, _, cx| cx.new(|_| ResizeSidebar))
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .bg(rgb(theme::SURFACE))
                    .group_hover("sidebar-handle", |style| style.bg(rgb(theme::BORDER))),
            )
    }

    /// Writes a change already made in memory. If it fails, the error shows in
    /// the sidebar and the groups are read back so the window matches the disk.
    fn save(
        &mut self,
        cx: &mut Context<Self>,
        write: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) {
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { write() }).await;

            this.update(cx, |this, cx| {
                this.folders_error = result.err();
                if this.folders_error.is_some() {
                    Self::load_folders(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Whether the sidebar's filter lets the PR at `url` through.
    fn shows(&self, url: &str) -> bool {
        match self.filter {
            Filter::All => true,
            Filter::Loose => !self.folders.by_url.contains_key(url),
            Filter::Folder(id) => self
                .folders
                .by_url
                .get(url)
                .is_some_and(|member| self.folders.includes(id, *member)),
        }
    }

    fn select(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    /// Puts the PR at `url` in `folder`, or in none.
    fn move_to(&mut self, url: &str, folder: Option<i64>, cx: &mut Context<Self>) {
        if self.folders.by_url.get(url).copied() == folder {
            return;
        }
        match folder {
            Some(id) => self.folders.by_url.insert(url.to_string(), id),
            None => self.folders.by_url.remove(url),
        };
        let url = url.to_string();
        self.save(cx, move || folders::assign(&url, folder));
    }

    /// Folds or unfolds the group's subgroups in the sidebar.
    fn toggle_fold(&mut self, id: i64, cx: &mut Context<Self>) {
        if !self.folders.collapsed.remove(&id) {
            self.folders.collapsed.insert(id);
        }
        let collapsed = self.folders.collapsed.clone();
        self.save(cx, move || folders::save_collapsed(&collapsed));
        cx.notify();
    }

    fn start_editing(&mut self, target: Editing, cx: &mut Context<Self>) {
        if let Editing::New {
            parent: Some(parent),
            ..
        } = target
            && self.folders.collapsed.contains(&parent)
        {
            self.toggle_fold(parent, cx);
        }
        let text = match &target {
            Editing::Rename(id) => self
                .folders
                .list
                .iter()
                .find(|folder| folder.id == *id)
                .map(|folder| folder.name.clone())
                .unwrap_or_default(),
            Editing::New { .. } => String::new(),
        };
        self.editor = Some(Editor { target, text });
        cx.notify();
    }

    /// Typing into the group name: enter saves it, escape drops it. Without a
    /// platform text field there is no IME, so dead-key accents don't compose.
    fn edit_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "enter" => self.finish_editing(cx),
            "escape" => self.editor = None,
            "backspace" => {
                editor.text.pop();
            }
            _ if keystroke.modifiers.platform || keystroke.modifiers.control => return,
            _ => match &keystroke.key_char {
                Some(typed) if !typed.chars().any(char::is_control) => {
                    editor.text.push_str(typed);
                }
                _ => return,
            },
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Saves the name being typed. A blank one changes nothing.
    fn finish_editing(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        cx.notify();
        let name = editor.text.trim().to_string();
        if name.is_empty() {
            return;
        }

        match editor.target {
            Editing::Rename(id) => {
                if let Some(folder) = self.folders.list.iter_mut().find(|folder| folder.id == id) {
                    folder.name = name.clone();
                }
                self.save(cx, move || folders::rename(id, &name));
                self.sync_subgroups(id, cx);
            }
            Editing::New { pull, parent } => {
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move { folders::create(&name, parent) })
                        .await;

                    this.update(cx, |this, cx| {
                        match result {
                            Ok(folder) => {
                                let id = folder.id;
                                this.folders.list.push(folder);
                                if parent.is_some() {
                                    this.sync_subgroups(id, cx);
                                }
                                match pull {
                                    Some(url) => this.move_to(&url, Some(id), cx),
                                    None => this.filter = Filter::Folder(id),
                                }
                            }
                            Err(error) => this.folders_error = Some(error),
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
        }
    }

    /// Removes the group and its subgroups; their PRs go back to "Sin grupo".
    fn delete_folder(&mut self, id: i64, cx: &mut Context<Self>) {
        let parent = self.folders.get(id).and_then(|folder| folder.parent);
        let gone: HashSet<i64> = self
            .folders
            .list
            .iter()
            .filter(|folder| self.folders.includes(id, folder.id))
            .map(|folder| folder.id)
            .collect();
        self.folders
            .list
            .retain(|folder| !gone.contains(&folder.id));
        self.folders
            .by_url
            .retain(|_, folder| !gone.contains(folder));
        if let Filter::Folder(selected) = self.filter
            && gone.contains(&selected)
        {
            self.filter = Filter::All;
        }
        self.save(cx, move || folders::delete(id));
        if let Some(parent) = parent {
            self.sync_subgroups(parent, cx);
        }
        cx.notify();
    }

    /// Takes the draft out of draft and reloads "Mis PRs" so it moves section.
    fn mark_ready(&mut self, url: &str, cx: &mut Context<Self>) {
        let url = url.to_string();
        cx.spawn(async move |this, cx| {
            let target = url.clone();
            let result = cx
                .background_executor()
                .spawn(async move { github::mark_ready(&target) })
                .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.reload(cx),
                    Err(error) => this.folders_error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Asks, in the system's own dialog, before commenting `/deploy` on the
    /// PR; enter confirms, escape cancels.
    fn deploy(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pull) = self.mine.pulls().iter().find(|pull| pull.url == url) else {
            return;
        };
        let message = format!("¿Hacer deploy de {} #{}?", pull.repo_name(), pull.number);
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &message,
            Some(&pull.title),
            // `Cancel` is what escape picks; a plain "Cancelar" would not be.
            &[
                gpui::PromptButton::Other("Deploy".into()),
                gpui::PromptButton::Cancel("Cancelar".into()),
            ],
            cx,
        );
        let url = url.to_string();
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |this, cx| this.send_deploy(&url, cx)).ok();
            }
        })
        .detach();
    }

    /// Comments `/deploy` on the PR and reloads "Mis PRs" so the bot's run
    /// shows up.
    fn send_deploy(&mut self, url: &str, cx: &mut Context<Self>) {
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
                        this.reload(cx);
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

    /// A PR's menu: hand its next action to the group's agent, ask for a
    /// review on Slack, comment `/deploy` (after asking), open
    /// each failed check's logs, copy its URL and take it out of its group.
    /// Slack and `/deploy` only on the user's own open PRs.
    fn pull_menu(&self, url: &str, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let now = now();
        // The order is fixed: copy, the Slack thread, /deploy, then the rest.
        let copied = url.to_string();
        let mut items = vec![ui::menu_item(
            "menu-copy-url",
            "Copiar URL",
            move |_event, _window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
            },
        )];
        if let Some(pull) = self.mine.pulls().iter().find(|pull| pull.url == url) {
            let branch = pull.head.clone();
            items.push(ui::menu_item(
                "menu-copy-branch",
                "Copiar rama",
                move |_event, _window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(branch.clone()));
                },
            ));
        }
        if let Some(link) = self.request_link(url) {
            items.push(ui::menu_item(
                "menu-request",
                "Ver pedido en Slack",
                move |_event, _window, cx| cx.open_url(&link),
            ));
        }

        if let Some(pull) = self.mine.pulls().iter().find(|pull| pull.url == url) {
            let mine = pull.author == self.login().unwrap_or_default();
            let deploying = pull
                .pipeline
                .as_ref()
                .is_some_and(|run| run.state(now) == State::Running);
            if mine && !pull.draft && !deploying {
                let label = match self.deploys.get(url) {
                    Some(Deploy::Sending) => "Enviando deploy…",
                    _ => "Deploy",
                };
                let target = url.to_string();
                items.push(ui::menu_item(
                    "menu-deploy",
                    label,
                    cx.listener(move |this, _event, window, cx| this.deploy(&target, window, cx)),
                ));
            }
            // Once asked for, the review is on its way: the item goes until the
            // app restarts.
            let wants_review = mine
                && !pull.draft
                && pull.review != Review::Approved
                && !self.review_asked.contains_key(url);
            if wants_review && review::config().is_some() {
                let asked = url.to_string();
                items.push(ui::menu_item(
                    "menu-review",
                    "Solicitar review",
                    cx.listener(move |this, _event, _window, cx| this.open_review(&asked, cx)),
                ));
            } else if let Some(channel) = slack_channel().filter(|_| wants_review) {
                let copied = url.to_string();
                items.push(ui::menu_item(
                    "menu-slack",
                    "Solicitar review",
                    move |_event, _window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                        cx.open_url(channel);
                    },
                ));
            }
            if mine && pull.draft {
                let target = url.to_string();
                items.push(ui::menu_item(
                    "menu-ready",
                    "Marcar listo para review",
                    cx.listener(move |this, _event, _window, cx| this.mark_ready(&target, cx)),
                ));
            }
            let action = status::action(pull, stack::below(pull, self.mine.pulls()), now);
            // Review threads get answered by hand, never by the agent.
            let task = action.task.filter(|_| mine && pull.threads.is_empty());
            if let Some(task) = task {
                let asked = url.to_string();
                items.push(ui::menu_item(
                    "menu-ask",
                    if self.opening_agent.is_some() {
                        "Pidiendo al agente…".to_string()
                    } else {
                        format!("Pedir al agente: {task}")
                    },
                    cx.listener(move |this, _event, _window, cx| this.ask_agent(&asked, cx)),
                ));
            }
            // The docs check's logs never say more than its name does.
            let checks = pull
                .failing_checks
                .iter()
                .filter(|check| !check.name.contains("docs"));
            for (index, check) in checks.enumerate() {
                if let Some(logs) = check.url.clone() {
                    items.push(ui::menu_item(
                        SharedString::from(format!("menu-logs-{index}")),
                        format!("Ver logs de {}", check.name),
                        move |_event, _window, cx| cx.open_url(&logs),
                    ));
                }
            }
        }

        if self.folders.by_url.contains_key(url) {
            let url = url.to_string();
            items.push(ui::menu_item(
                "menu-ungroup",
                "Sacar del grupo",
                cx.listener(move |this, _event, _window, cx| this.move_to(&url, None, cx)),
            ));
        }
        items
    }

    /// The right-click menu, drawn over everything at the click. Any click
    /// outside it closes it.
    fn menu(&self, menu: &Menu, cx: &mut Context<Self>) -> impl IntoElement {
        let items: Vec<_> = match &menu.target {
            MenuFor::Pull(url) => self.pull_menu(url, cx),
            MenuFor::Folder(id) => {
                let id = *id;
                let subgroup = self
                    .folders
                    .get(id)
                    .is_some_and(|folder| folder.parent.is_some());
                let mut items = vec![ui::menu_item(
                    "menu-agent",
                    if subgroup {
                        "Abrir agente del grupo"
                    } else {
                        "Abrir agente en Orca"
                    },
                    cx.listener(move |this, _event, _window, cx| this.open_agent(id, cx)),
                )];
                if !subgroup {
                    items.push(ui::menu_item(
                        "menu-subgroup",
                        "Nuevo subgrupo",
                        cx.listener(move |this, _event, _window, cx| {
                            this.start_editing(
                                Editing::New {
                                    pull: None,
                                    parent: Some(id),
                                },
                                cx,
                            )
                        }),
                    ));
                }
                items.push(ui::menu_item(
                    "menu-rename",
                    "Renombrar",
                    cx.listener(move |this, _event, _window, cx| {
                        this.start_editing(Editing::Rename(id), cx)
                    }),
                ));
                items.push(ui::menu_item(
                    "menu-delete",
                    match (subgroup, self.folders.children(id).next().is_some()) {
                        (true, _) => "Eliminar subgrupo",
                        (false, true) => "Eliminar grupo y subgrupos",
                        (false, false) => "Eliminar grupo",
                    },
                    cx.listener(move |this, _event, _window, cx| this.delete_folder(id, cx)),
                ));
                items
            }
        };

        deferred(
            anchored().position(menu.at).snap_to_window().child(
                div()
                    .id("menu")
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
                    // Any item closes the menu once its own handler ran.
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .children(items),
            ),
        )
        .with_priority(1)
    }

    fn refreshing(&self) -> bool {
        self.mine.in_flight || self.merged.in_flight
    }

    fn login(&self) -> Option<&str> {
        match &self.mine.load {
            Load::Loaded(snapshot) if !snapshot.login.is_empty() => Some(snapshot.login.as_str()),
            _ => None,
        }
    }

    /// The URLs of the user's open PRs.
    fn open_urls(&self) -> HashSet<&str> {
        self.mine
            .pulls()
            .iter()
            .map(|pull| pull.url.as_str())
            .collect()
    }

    /// The groups down the left: everything, the PRs in no group, then each
    /// group with its subgroups under it. A PR dropped on a group moves into
    /// it; dropped on "Sin grupo", out of the one it was in; dropped on "Nuevo
    /// grupo", into a new one.
    fn sidebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.open_urls();
        let in_folder = |id: i64| {
            open.iter()
                .filter(|url| {
                    self.folders
                        .by_url
                        .get(**url)
                        .is_some_and(|member| self.folders.includes(id, *member))
                })
                .count()
        };
        let loose = open
            .iter()
            .filter(|url| !self.folders.by_url.contains_key(**url))
            .count();
        let editing = |target: &dyn Fn(&Editing) -> bool| {
            self.editor
                .as_ref()
                .filter(|editor| target(&editor.target))
                .map(|editor| editor.text.clone())
        };

        let mut folders = Vec::new();
        for root in self
            .folders
            .list
            .iter()
            .filter(|folder| folder.parent.is_none())
        {
            let folded = self.folders.collapsed.contains(&root.id);
            let children = self.folders.children(root.id).filter(|_| !folded);
            for folder in std::iter::once(root).chain(children) {
                let id = folder.id;
                let indented = folder.parent.is_some();
                folders.push(
                    match editing(
                        &|target| matches!(target, Editing::Rename(other) if *other == id),
                    ) {
                        Some(text) => self.name_field(text, indented, cx).into_any_element(),
                        None => self
                            .sidebar_entry(
                                folder.name.clone(),
                                in_folder(id),
                                Filter::Folder(id),
                                window,
                                cx,
                            )
                            .on_mouse_down(
                                MouseButton::Right,
                                self.open_menu(MenuFor::Folder(id), cx),
                            )
                            .into_any_element(),
                    },
                );
            }
            let parent = Some(root.id);
            if let Some(text) = editing(
                &|target| matches!(target, Editing::New { parent: other, .. } if *other == parent),
            ) {
                folders.push(self.name_field(text, true, cx).into_any_element());
            }
        }
        let new_name = editing(&|target| matches!(target, Editing::New { parent: None, .. }));

        div()
            .id("sidebar")
            .flex()
            .flex_col()
            .gap_1()
            .w(px(self.sidebar_width))
            .flex_shrink_0()
            .overflow_y_scroll()
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child("GRUPOS"),
            )
            .child(self.sidebar_entry("Todos".to_string(), open.len(), Filter::All, window, cx))
            .child(
                self.sidebar_entry("Sin grupo".to_string(), loose, Filter::Loose, window, cx)
                    .drag_over::<Dragged>(|style, _, _, _| drop_highlight(style))
                    .on_drop(cx.listener(|this, dragged: &Dragged, _window, cx| {
                        this.move_to(&dragged.url, None, cx)
                    })),
            )
            .child(div().h(px(6.)))
            .children(folders)
            .child(match new_name {
                Some(text) => self.name_field(text, false, cx).into_any_element(),
                None => div()
                    .id("folder-new")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .text_color(rgb(theme::MUTED))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
                    .child("+ Nuevo grupo")
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.start_editing(
                            Editing::New {
                                pull: None,
                                parent: None,
                            },
                            cx,
                        )
                    }))
                    .drag_over::<Dragged>(|style, _, _, _| drop_highlight(style))
                    .on_drop(cx.listener(|this, dragged: &Dragged, _window, cx| {
                        this.start_editing(
                            Editing::New {
                                pull: Some(dragged.url.clone()),
                                parent: None,
                            },
                            cx,
                        )
                    }))
                    .into_any_element(),
            })
            .when_some(self.folders_error.clone(), |this, error| {
                this.child(
                    div()
                        .px_2()
                        .pt_2()
                        .text_xs()
                        .text_color(rgb(theme::RED))
                        .child(error),
                )
            })
    }

    /// One clickable line of the sidebar; a group's is also a drop target.
    fn sidebar_entry(
        &self,
        label: String,
        count: usize,
        filter: Filter,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (id, folder) = match filter {
            Filter::All => (SharedString::from("filter-all"), None),
            Filter::Loose => (SharedString::from("filter-loose"), None),
            Filter::Folder(id) => (SharedString::from(format!("folder-{id}")), Some(id)),
        };
        let hovered = self.hovered == Some(filter);
        let entry = folder.and_then(|id| self.folders.get(id));
        let indent = match entry {
            Some(folder) if folder.parent.is_some() => SUBGROUP_INDENT,
            _ => 0.,
        };
        // A top-level group: `Some(folded)` when it has subgroups to fold.
        let fold = entry
            .filter(|folder| folder.parent.is_none())
            .map(|folder| {
                self.folders
                    .children(folder.id)
                    .next()
                    .map(|_| self.folders.collapsed.contains(&folder.id))
            });
        // A folded group stands in for its selected subgroup.
        let selected = self.filter == filter
            || matches!(
                (fold, self.filter, folder),
                (Some(Some(true)), Filter::Folder(selected), Some(id))
                    if self.folders.get(selected).and_then(|folder| folder.parent) == Some(id)
            );
        let label_width = self.sidebar_width
            - ENTRY_CHROME
            - indent
            - if fold.is_some() { FOLD_WIDTH } else { 0. };

        div()
            .id(id)
            .ml(px(indent))
            .flex()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .when(selected, |this| this.bg(rgb(theme::SURFACE)))
            .text_color(rgb(if selected {
                theme::ACCENT
            } else {
                theme::SUBTEXT
            }))
            .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
            .on_click(cx.listener(move |this, _event, _window, cx| this.select(filter, cx)))
            .on_hover(cx.listener(move |this, hovering: &bool, _window, cx| {
                if *hovering {
                    this.hovered = Some(filter);
                } else if this.hovered == Some(filter) {
                    this.hovered = None;
                }
                cx.notify();
            }))
            .when_some(fold, |this, fold| {
                this.child(
                    div()
                        .id(SharedString::from(format!(
                            "fold-{}",
                            folder.unwrap_or_default()
                        )))
                        .w(px(12.))
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(rgb(theme::MUTED))
                        .when_some(fold, |this, folded| {
                            let id = folder.unwrap_or_default();
                            this.hover(|style| style.text_color(rgb(theme::TEXT)))
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.toggle_fold(id, cx)
                                }))
                                .child(if folded { "▸" } else { "▾" })
                        }),
                )
            })
            .child(label_element(label, label_width, hovered, window))
            .child(
                div()
                    .w(px(COUNT_WIDTH))
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(count.to_string()),
            )
            .when_some(folder, |this, id| {
                this.drag_over::<Dragged>(|style, _, _, _| drop_highlight(style))
                    .on_drop(cx.listener(move |this, dragged: &Dragged, _window, cx| {
                        this.move_to(&dragged.url, Some(id), cx)
                    }))
            })
    }

    /// The group name being typed, with a caret. Clicking elsewhere saves it.
    fn name_field(&self, text: String, indented: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .when(indented, |this| this.ml(px(SUBGROUP_INDENT)))
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::ACCENT))
            .bg(rgb(theme::MANTLE))
            .text_sm()
            .on_mouse_down_out(cx.listener(|this, _event, _window, cx| this.finish_editing(cx)))
            .child(if text.is_empty() {
                div()
                    .text_color(rgb(theme::MUTED))
                    .child("Nombre del grupo▏")
            } else {
                div().child(format!("{text}▏"))
            })
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut caption = match (self.login(), self.mine.fetched_at) {
            (Some(_), Some(at)) => {
                format!("actualizado {}", status::ago(at.elapsed().as_secs() as i64))
            }
            _ => "consultando GitHub…".to_string(),
        };
        let deploying = self.mine.deploying(now());
        if deploying > 0 {
            caption.push_str(&format!(
                " · {deploying} despliegue{} en curso",
                if deploying == 1 { "" } else { "s" }
            ));
        }

        div()
            .flex()
            .items_center()
            .gap_3()
            .child(self.tabs(cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(caption),
            )
            .when_some(
                match self.filter {
                    Filter::Folder(id) => Some(id),
                    _ => None,
                },
                |this, id| {
                    let opening = self.opening_agent.is_some();
                    this.child(ui::button(
                        "agent",
                        if opening { "Abriendo…" } else { "Agente" },
                        !opening,
                        cx.listener(move |this, _event, _window, cx| this.open_agent(id, cx)),
                    ))
                },
            )
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
            Load::Loaded(snapshot) => format!(" ({})", self.visible(&snapshot.pulls).len()),
            _ => String::new(),
        };

        div()
            .flex()
            .gap_0p5()
            .p(px(3.))
            .rounded_lg()
            .bg(rgb(theme::MANTLE))
            .child(ui::toggle_button(
                "tab-mine",
                format!("Mis PRs{}", count(&self.mine)),
                self.tab == Tab::Mine,
                cx.listener(|this, _event, _window, cx| this.show(Tab::Mine, cx)),
            ))
            .child(ui::toggle_button(
                "tab-merged",
                "Mergeadas",
                self.tab == Tab::Merged,
                cx.listener(|this, _event, _window, cx| this.show(Tab::Merged, cx)),
            ))
    }

    fn body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.tab == Tab::Merged {
            return self.merged_list(cx).into_any_element();
        }

        match &self.mine.load {
            Load::Loading => message("Cargando…", theme::SUBTEXT).into_any_element(),
            Load::Failed(error) => message(error.clone(), theme::RED).into_any_element(),
            Load::Loaded(snapshot) => {
                if let Some(group) = self.board_group() {
                    return self.board(group, &snapshot.pulls, cx).into_any_element();
                }
                let pulls = self.visible(&snapshot.pulls);
                if pulls.is_empty() {
                    return message(
                        if self.filter == Filter::All {
                            "No tienes PRs abiertos."
                        } else {
                            "No hay PRs en este grupo."
                        },
                        theme::SUBTEXT,
                    )
                    .into_any_element();
                }
                self.mine_list(&pulls, cx).into_any_element()
            }
        }
    }

    /// The PRs of `pulls` the sidebar's filter lets through.
    fn visible<'a>(&self, pulls: &'a [PullRequest]) -> Vec<&'a PullRequest> {
        pulls.iter().filter(|pull| self.shows(&pull.url)).collect()
    }

    /// The user's own PRs: a one-line count per group, then one section per
    /// group.
    fn mine_list(&self, pulls: &[&PullRequest], cx: &mut Context<Self>) -> impl IntoElement {
        let now = now();
        let in_group = |group: Group| {
            pulls
                .iter()
                .copied()
                .filter(move |pull| Group::of(pull, now) == group)
        };

        let stacks = self.stacks(pulls);
        let sections: Vec<_> = Group::ALL
            .into_iter()
            .filter_map(|group| {
                let (count, rows) = self.with_stacks(in_group(group), &stacks, false, cx);
                (!rows.is_empty())
                    .then(|| section(group.title(), group.color(), count, rows).into_any_element())
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .text_xs()
                    .children(Group::ALL.map(|group| {
                        let count = in_group(group).count();
                        div()
                            .text_color(rgb(if count == 0 {
                                theme::MUTED
                            } else {
                                group.color()
                            }))
                            .child(format!("{count} {}", group.short()))
                    })),
            )
            .child(list("mine").children(sections))
    }

    /// The selected group when "Mis PRs" shows it as a board: a top-level
    /// group with subgroups.
    fn board_group(&self) -> Option<i64> {
        match self.filter {
            Filter::Folder(id)
                if self
                    .folders
                    .get(id)
                    .is_some_and(|folder| folder.parent.is_none())
                    && self.folders.children(id).next().is_some() =>
            {
                Some(id)
            }
            _ => None,
        }
    }

    /// A group with subgroups: the counts for all of it, then one column per
    /// subgroup, led by one for the PRs in the group itself when it has any.
    /// The columns scroll sideways when they don't fit.
    fn board(&self, id: i64, pulls: &[PullRequest], cx: &mut Context<Self>) -> impl IntoElement {
        let now = now();
        let visible = self.visible(pulls);
        let in_column = |column: i64| -> Vec<&PullRequest> {
            visible
                .iter()
                .copied()
                .filter(|pull| self.folders.by_url.get(&pull.url) == Some(&column))
                .collect()
        };

        let mut columns = Vec::new();
        let open = in_column(id);
        if !open.is_empty() {
            columns.push(self.board_column(id, &open, cx).into_any_element());
        }
        for subgroup in self.folders.children(id) {
            let open = in_column(subgroup.id);
            columns.push(self.board_column(subgroup.id, &open, cx).into_any_element());
        }

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .text_xs()
                    .children(Group::ALL.map(|group| {
                        let count = visible
                            .iter()
                            .filter(|pull| Group::of(pull, now) == group)
                            .count();
                        div()
                            .text_color(rgb(if count == 0 {
                                theme::MUTED
                            } else {
                                group.color()
                            }))
                            .child(format!("{count} {}", group.short()))
                    })),
            )
            .child(
                div()
                    .id("board-frame")
                    .group("board")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .min_h(px(0.))
                    .on_drag_move(cx.listener(
                        |this, event: &gpui::DragMoveEvent<ScrollThumb>, _window, cx| {
                            this.scroll_board_to(event.event.position.x, event.bounds, cx)
                        },
                    ))
                    .on_drag_move(cx.listener(
                        |this, event: &gpui::DragMoveEvent<PanBoard>, _window, cx| {
                            this.pan_board(event.event.position.x, cx)
                        },
                    ))
                    .child(
                        div()
                            .id("board")
                            .flex()
                            .gap_3()
                            .flex_1()
                            .min_h(px(0.))
                            .overflow_x_scroll()
                            .map(own_axis_only)
                            .track_scroll(&self.board_scroll)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                                    let scrolled = -f32::from(this.board_scroll.offset().x);
                                    this.pan_from = Some((f32::from(event.position.x), scrolled));
                                    cx.notify();
                                }),
                            )
                            .on_drag(PanBoard, |_, _, _, cx| cx.new(|_| PanBoard))
                            // An open hand says the board can be dragged; it
                            // closes while the mouse is down.
                            .cursor(if self.pan_from.is_some() {
                                gpui::CursorStyle::ClosedHand
                            } else {
                                gpui::CursorStyle::OpenHand
                            })
                            .children(columns),
                    )
                    .child(self.board_scrollbar()),
            )
    }

    /// Scrolls the board so the thumb's middle sits under the cursor, from a
    /// drag over `frame`, the board's frame.
    fn scroll_board_to(&mut self, cursor: Pixels, frame: Bounds<Pixels>, cx: &mut Context<Self>) {
        let viewport = f32::from(self.board_scroll.bounds().size.width);
        let content = viewport + f32::from(self.board_scroll.max_offset().width);
        let track = f32::from(frame.size.width);
        if content <= viewport || track <= 0. {
            return;
        }
        let along = (f32::from(cursor - frame.left()) / track).clamp(0., 1.);
        let offset = (along * content - viewport / 2.).clamp(0., content - viewport);
        self.board_scroll
            .set_offset(Point::new(px(-offset), px(0.)));
        self.board_dragging = true;
        cx.notify();
    }

    fn stop_dragging(&mut self, cx: &mut Context<Self>) {
        self.pan_from = None;
        if self.board_dragging {
            self.board_dragging = false;
            cx.notify();
        }
    }

    /// Pans the board along with a drag of its empty space: the content
    /// follows the cursor from where the mouse went down.
    fn pan_board(&mut self, cursor: Pixels, cx: &mut Context<Self>) {
        let Some((from_x, from_offset)) = self.pan_from else {
            return;
        };
        let viewport = f32::from(self.board_scroll.bounds().size.width);
        let content = viewport + f32::from(self.board_scroll.max_offset().width);
        let offset =
            (from_offset + from_x - f32::from(cursor)).clamp(0., (content - viewport).max(0.));
        self.board_scroll
            .set_offset(Point::new(px(-offset), px(0.)));
        self.board_dragging = true;
        cx.notify();
    }

    /// A thin horizontal scrollbar under the columns, shown while the mouse
    /// is over the board and only when the columns don't fit. Dragging the
    /// thumb scrolls.
    fn board_scrollbar(&self) -> impl IntoElement {
        let viewport = f32::from(self.board_scroll.bounds().size.width);
        let content = viewport + f32::from(self.board_scroll.max_offset().width);
        let scrolled = (-f32::from(self.board_scroll.offset().x)).max(0.);
        let needed = content > viewport + 1.;
        let (thumb, left) = if needed {
            (viewport / content, scrolled / content)
        } else {
            (1., 0.)
        };

        div()
            .relative()
            .h(px(SCROLLBAR_HEIGHT))
            .flex_shrink_0()
            .rounded_full()
            .when(needed, |this| {
                this.opacity(if self.board_dragging { 1. } else { 0. })
                    .group_hover("board", |style| style.opacity(1.))
                    .bg(rgb(theme::MANTLE))
                    .child(
                        div()
                            .id("board-thumb")
                            .absolute()
                            .top_0()
                            .h_full()
                            .left(gpui::relative(left))
                            .w(gpui::relative(thumb))
                            .rounded_full()
                            .bg(rgb(theme::SURFACE_ACTIVE))
                            .hover(|style| style.bg(rgb(theme::MUTED)))
                            .cursor_pointer()
                            .on_drag(ScrollThumb, |_, _, _, cx| cx.new(|_| ScrollThumb)),
                    )
            })
    }

    /// One column of the board, for a subgroup or, as "Sin subgrupo", for the
    /// PRs in the group itself: its name and count, and its PRs by what they
    /// wait on. A PR dropped on it moves into its group; a right click on the
    /// name opens the group's menu.
    fn board_column(
        &self,
        id: i64,
        open: &[&PullRequest],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now = now();
        let name = match self.folders.get(id) {
            Some(folder) if folder.parent.is_some() => folder.name.clone(),
            _ => "Sin subgrupo".to_string(),
        };
        let stacks = self.stacks(open);
        let ordered = Group::ALL.into_iter().flat_map(|group| {
            open.iter()
                .copied()
                .filter(move |pull| Group::of(pull, now) == group)
        });
        let (_, mut cards) = self.with_stacks(ordered, &stacks, true, cx);
        if cards.is_empty() {
            cards.push(
                div()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_dashed()
                    .border_color(rgb(theme::BORDER))
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child("Sin PRs abiertos. Arrastra uno aquí.")
                    .into_any_element(),
            );
        }
        div()
            .id(SharedString::from(format!("column-{id}")))
            .flex()
            .flex_col()
            .gap_2()
            .w(px(BOARD_COLUMN))
            .flex_shrink_0()
            .p_2()
            .rounded_lg()
            .border_1()
            .border_color(rgb(theme::BASE))
            .bg(rgb(theme::MANTLE))
            .drag_over::<Dragged>(|style, _, _, _| drop_highlight(style))
            .on_drop(cx.listener(move |this, dragged: &Dragged, _window, cx| {
                this.move_to(&dragged.url, Some(id), cx)
            }))
            .child(
                div()
                    .id(SharedString::from(format!("column-name-{id}")))
                    .flex()
                    .gap_2()
                    .items_center()
                    .px_1()
                    .text_sm()
                    .text_color(rgb(theme::SUBTEXT))
                    .on_mouse_down(MouseButton::Right, self.open_menu(MenuFor::Folder(id), cx))
                    .child(div().flex_1().min_w(px(0.)).truncate().child(name))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child(open.len().to_string()),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("column-cards-{id}")))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .map(own_axis_only)
                    .children(cards),
            )
    }

    /// The user's merged PRs, one section per local day, newest first.
    fn merged_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let feed = &self.merged;
        let Some(pulls) = &feed.pulls else {
            return match &feed.error {
                Some(error) => message(error.clone(), theme::RED).into_any_element(),
                None => message("Cargando…", theme::SUBTEXT).into_any_element(),
            };
        };

        let today = merged::day_of(now());
        let mut days: Vec<(chrono::NaiveDate, Vec<gpui::AnyElement>)> = Vec::new();
        for pull in pulls.iter().filter(|pull| self.shows(&pull.url)) {
            let day = merged::day_of(pull.merged_at);
            let row = self.merged_row(pull, false, cx).into_any_element();
            match days.last_mut() {
                Some((last, rows)) if *last == day => rows.push(row),
                _ => days.push((day, vec![row])),
            }
        }

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.))
            .when_some(feed.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::RED))
                        .truncate()
                        .child(format!("No se pudo actualizar: {error}")),
                )
            })
            .child(if days.is_empty() {
                message(
                    if self.filter == Filter::All {
                        "Todavía no hay PRs mergeados en el registro."
                    } else {
                        "No hay PRs mergeados en este grupo."
                    },
                    theme::SUBTEXT,
                )
                .into_any_element()
            } else {
                list("merged")
                    .children(days.into_iter().map(|(day, rows)| {
                        let color = if day == today {
                            theme::ACCENT
                        } else {
                            theme::SUBTEXT
                        };
                        section(&merged::day_label(day, today), color, rows.len(), rows)
                    }))
                    .into_any_element()
            })
            .into_any_element()
    }

    /// One merged PR: repo, branches, title and when it merged, the day too
    /// when it isn't under a day's heading. Clicking it opens the PR; a right
    /// click opens the menu; dragging it onto a group moves it there.
    fn merged_row(
        &self,
        pull: &merged::Merged,
        with_day: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let url = pull.url.clone();
        let mut when = merged::time_of(pull.merged_at);
        if with_day {
            let day = merged::day_label(merged::day_of(pull.merged_at), merged::day_of(now()));
            when = format!("{day} {when}");
        }

        div()
            .id(SharedString::from(format!("merged-{}", pull.url)))
            .on_drag(
                Dragged {
                    url: pull.url.clone(),
                    title: pull.title.clone().into(),
                },
                drag_preview,
            )
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(rgb(theme::SURFACE))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
            .on_click(move |_event, _window, cx| cx.open_url(&url))
            .on_mouse_down(
                MouseButton::Right,
                self.open_menu(MenuFor::Pull(pull.url.clone()), cx),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(div().flex_1().min_w(px(0.)).truncate().child(format!(
                        "{} #{} · {} ← {}",
                        pull.repo_name(),
                        pull.number,
                        pull.base,
                        pull.head
                    )))
                    .child(div().flex_shrink_0().font_family("Menlo").child(when)),
            )
            .child(div().text_sm().truncate().child(pull.title.clone()))
    }

    /// The right-mouse-down handler that opens the menu for `target` at the click.
    fn open_menu(
        &self,
        target: MenuFor,
        cx: &mut Context<Self>,
    ) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
            this.menu = Some(Menu {
                target: target.clone(),
                at: event.position,
            });
            cx.notify();
        })
    }

    /// The stacks among `pulls` with two or more of them, each bottom first:
    /// the PR on the long-lived branch, then the ones on top of it.
    fn stacks<'a>(&'a self, pulls: &[&'a PullRequest]) -> Vec<Vec<&'a PullRequest>> {
        let all = self.mine.pulls();
        let shown = |url: &str| pulls.iter().any(|pull| pull.url == url);
        pulls
            .iter()
            .filter(|pull| stack::below(pull, all).is_none())
            .map(|pull| {
                stack::chain(pull, all)
                    .into_iter()
                    .filter(|member| shown(&member.url))
                    .collect::<Vec<_>>()
            })
            .filter(|chain| chain.len() > 1)
            .collect()
    }

    /// `pulls` as rows or cards, a stack drawn whole where its bottom PR
    /// falls and its other PRs skipped where they would; with how many PRs
    /// that is.
    fn with_stacks<'a>(
        &self,
        pulls: impl Iterator<Item = &'a PullRequest>,
        stacks: &[Vec<&PullRequest>],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> (usize, Vec<gpui::AnyElement>) {
        let stacked = |url: &str| {
            stacks
                .iter()
                .any(|chain| chain[1..].iter().any(|pull| pull.url == url))
        };
        let mut count = 0;
        let mut rows = Vec::new();
        for pull in pulls.filter(|pull| !stacked(&pull.url)) {
            match stacks.iter().find(|chain| chain[0].url == pull.url) {
                Some(chain) => {
                    count += chain.len();
                    rows.push(self.stack_unit(chain, compact, cx).into_any_element());
                }
                None => {
                    count += 1;
                    rows.push(self.pull_block(pull, compact, None, cx).into_any_element());
                }
            }
        }
        (count, rows)
    }

    /// A stack, bottom first under the branch it merges into, its PRs joined
    /// by a rail with an arrow at each pointing at the one it merges into.
    fn stack_unit(
        &self,
        chain: &[&PullRequest],
        compact: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let base = chain[0];
        let last = chain.len() - 1;

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(theme::SURFACE_HOVER))
                            .text_color(rgb(theme::SUBTEXT))
                            .child(base.base.clone()),
                    )
                    .child(format!("{} · stack de {}", base.repo_name(), chain.len())),
            )
            .children(chain.iter().enumerate().map(|(index, pull)| {
                let rail = Rail {
                    first: index == 0,
                    last: index == last,
                };
                self.pull_block(pull, compact, Some(rail), cx)
                    .into_any_element()
            }))
    }

    /// A PR: its title behind the state's dot, its next action as the one
    /// colored pill, the rest of its state in a muted line, the stack or
    /// twin it has, and its pipeline. Clicking the title opens the PR on
    /// GitHub; "⋯", shown while the mouse is over the PR, and a right click
    /// open its menu. Dragging it onto a group moves it there.
    fn pull_block(
        &self,
        pull: &PullRequest,
        compact: bool,
        rail: Option<Rail>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now = now();
        let pulls = self.mine.pulls();
        let expanded = self.expanded.contains(&pull.url);
        let action = status::action(pull, stack::below(pull, pulls), now);
        let group = SharedString::from(format!("pull-{}", pull.url));
        let open = pull.url.clone();

        div()
            .id(group.clone())
            .group(group.clone())
            .on_drag(
                Dragged {
                    url: pull.url.clone(),
                    title: pull.title.clone().into(),
                },
                drag_preview,
            )
            .on_mouse_down(
                MouseButton::Right,
                self.open_menu(MenuFor::Pull(pull.url.clone()), cx),
            )
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .rounded_lg()
            .hover(|style| style.bg(rgb(theme::ROW_HOVER)))
            .when_some(rail, |this, rail| this.relative().children(rail.pieces()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(state_dot(pull, self.review_asked.contains_key(&pull.url)))
                    .child(
                        div()
                            .id(SharedString::from(format!("open-{}", pull.url)))
                            .flex_1()
                            .min_w(px(0.))
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(if pull.draft {
                                theme::SUBTEXT
                            } else {
                                theme::TEXT
                            }))
                            .cursor_pointer()
                            .when(compact, |this| this.line_clamp(2))
                            .when(!compact, |this| this.truncate())
                            .on_click(move |_event, _window, cx| cx.open_url(&open))
                            .child(pull.title.clone()),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("more-{}", pull.url)))
                            .flex_shrink_0()
                            .px_1()
                            .rounded_md()
                            .text_sm()
                            .text_color(rgb(theme::MUTED))
                            .cursor_pointer()
                            .opacity(0.)
                            .group_hover(group.clone(), |style| style.opacity(1.))
                            .hover(|style| {
                                style.bg(rgb(theme::SURFACE)).text_color(rgb(theme::TEXT))
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                self.open_menu(MenuFor::Pull(pull.url.clone()), cx),
                            )
                            .child("⋯"),
                    ),
            )
            .child(
                // The action, then the rest of the state and the age: on one
                // line, or under the action in a narrow card.
                div()
                    .flex()
                    .when(compact, |this| this.flex_col().items_start())
                    .when(!compact, |this| this.items_center())
                    .gap_1()
                    .pl(px(TITLE_INDENT))
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    // In a column, a lone pill measures as zero wide; a row
                    // around it gives it its content's width.
                    .child(
                        div()
                            .flex()
                            .min_w(px(0.))
                            .max_w_full()
                            .when(compact, |this| this.w_full())
                            .child(ui::pill(action.label, action.color)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .flex_1()
                            .min_w(px(0.))
                            .when(compact, |this| this.w_full())
                            .child(div().flex_1().min_w(px(0.)).truncate().child(meta(pull)))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .child(status::ago(now - pull.updated_at)),
                            ),
                    ),
            )
            .children(relations(pull, pulls))
            .children(self.threads(pull, cx))
            .when_some(
                match self.deploys.get(&pull.url) {
                    Some(Deploy::Failed(error)) => Some(error.clone()),
                    _ => None,
                },
                |this, error| {
                    this.child(
                        div()
                            .pl(px(TITLE_INDENT))
                            .text_xs()
                            .text_color(rgb(theme::RED))
                            .child(format!("No se pudo enviar el deploy: {error}")),
                    )
                },
            )
            .when_some(pull.pipeline.as_ref(), |this, run| {
                this.child(
                    div()
                        .pl(px(TITLE_INDENT))
                        .child(self.pipeline_bar(pull, run, expanded, cx)),
                )
                .when(expanded, |this| {
                    this.child(div().pl(px(TITLE_INDENT)).child(steps(pull, run)))
                })
            })
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

/// A sidebar name in its fixed width. Cut with an ellipsis, or, while hovered
/// and too long to fit, sliding back and forth so all of it can be read.
fn label_element(label: String, width: f32, hovered: bool, window: &Window) -> gpui::AnyElement {
    let overflow = text_width(&label, window) - width;
    let frame = div().w(px(width)).flex_shrink_0().overflow_hidden();
    if !hovered || overflow <= 0. {
        return frame.truncate().child(label).into_any_element();
    }

    let cycle = MARQUEE_WAIT + overflow / MARQUEE_SPEED + MARQUEE_HOLD + MARQUEE_RETURN;
    frame
        .child(
            div()
                .whitespace_nowrap()
                .child(label.clone())
                .with_animation(
                    SharedString::from(format!("marquee-{label}")),
                    gpui::Animation::new(Duration::from_secs_f32(cycle)).repeat(),
                    move |this: gpui::Div, delta| {
                        this.ml(px(-marquee_offset(delta * cycle, overflow)))
                    },
                ),
        )
        .into_any_element()
}

/// How far left a name that overflows by `overflow` pixels is, `elapsed`
/// seconds into its cycle.
fn marquee_offset(elapsed: f32, overflow: f32) -> f32 {
    let slide = overflow / MARQUEE_SPEED;
    let t = elapsed - MARQUEE_WAIT;
    if t <= 0. {
        0.
    } else if t < slide {
        t * MARQUEE_SPEED
    } else if t < slide + MARQUEE_HOLD {
        overflow
    } else {
        let back = ((t - slide - MARQUEE_HOLD) / MARQUEE_RETURN).min(1.);
        overflow * (1. - back)
    }
}

/// The width `text` takes in the sidebar's font.
fn text_width(text: &str, window: &Window) -> f32 {
    let style = window.text_style();
    let size = gpui::rems(0.875).to_pixels(window.rem_size());
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        size,
        &[style.to_run(text.len())],
        None,
    );
    f32::from(line.width)
}

/// How a sidebar entry looks while a PR is dragged over it.
fn drop_highlight(style: gpui::StyleRefinement) -> gpui::StyleRefinement {
    style
        .bg(rgb(theme::SURFACE_ACTIVE))
        .border_color(rgb(theme::ACCENT))
}

/// Builds the chip a dragged PR row shows under the cursor.
fn drag_preview(
    dragged: &Dragged,
    at: Point<Pixels>,
    _: &mut Window,
    cx: &mut App,
) -> gpui::Entity<DragPreview> {
    let title = dragged.title.clone();
    cx.new(|_| DragPreview { title, at })
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

/// Makes a scrollable area ignore wheel deltas on the other axis. By default
/// GPUI turns a vertical wheel into sideways scrolling on a horizontal area
/// and the reverse, so the board and the column under the mouse would both
/// move on every wheel. GPUI 0.2 has no setter for this.
fn own_axis_only<E: Styled>(mut element: E) -> E {
    element.style().restrict_scroll_to_axis = Some(true);
    element
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

fn section(
    title: &str,
    color: u32,
    count: usize,
    rows: Vec<gpui::AnyElement>,
) -> impl IntoElement + use<> {
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
                .child(div().text_color(rgb(theme::MUTED)).child(count.to_string())),
        )
        .children(rows)
}

/// Where a PR sits in a drawn stack: a line runs up from each dot to what
/// it merges into, with the arrow at that end, under the branch for the
/// first PR and under the previous PR's dot for the rest.
#[derive(Copy, Clone)]
struct Rail {
    first: bool,
    last: bool,
}

impl Rail {
    /// Where the dot's middle is, from the top of the PR's block: its top
    /// padding and half its title line.
    const DOT_MIDDLE: f32 = 8. + 11.;
    /// The rail's left edge, under the dot's middle.
    const LEFT: f32 = 12. + DOT_SLOT / 2. - 1.;
    /// The arrow's height; the line starts a bit inside it, under its tip.
    const ARROW: f32 = 10.;

    /// The line into this PR's dot from above, with the arrow at the
    /// branch for the first, and the arrow under this dot that the line
    /// from the next PR ends at.
    fn pieces(self) -> Vec<gpui::AnyElement> {
        let dot_top = Self::DOT_MIDDLE - DOT_SLOT / 2.;
        let dot_bottom = Self::DOT_MIDDLE + DOT_SLOT / 2.;
        let mut pieces = Vec::new();
        if self.first {
            pieces.push(Self::arrow(-3.));
            pieces.push(Self::line(-3. + Self::ARROW - 2., Some(dot_top)));
        } else {
            pieces.push(Self::line(0., Some(dot_top)));
        }
        if !self.last {
            pieces.push(Self::arrow(dot_bottom));
            pieces.push(Self::line(dot_bottom + Self::ARROW - 2., None));
        }
        pieces
    }

    fn arrow(top: f32) -> gpui::AnyElement {
        div()
            .absolute()
            .top(px(top))
            .left(px(Self::LEFT - 4.))
            .w(px(10.))
            .h(px(Self::ARROW))
            .flex()
            .justify_center()
            .text_size(px(Self::ARROW))
            .line_height(px(Self::ARROW))
            .text_color(rgb(theme::MUTED))
            .child("▲")
            .into_any_element()
    }

    /// A stretch of the line from `top` down to `bottom`, or to the end of
    /// the PR's block without one.
    fn line(top: f32, bottom: Option<f32>) -> gpui::AnyElement {
        div()
            .absolute()
            .top(px(top))
            .map(|this| match bottom {
                Some(bottom) => this.h(px(bottom - top)),
                None => this.bottom(px(0.)),
            })
            .left(px(Self::LEFT))
            .w(px(2.))
            .bg(rgb(theme::SURFACE_ACTIVE))
            .into_any_element()
    }
}

/// A PR at a glance: a dot in its CI's color, a teal ring around it once a
/// review was asked for in Slack, and a green check on its corner once it is
/// approved. The section, not the dot, says what to do next.
fn state_dot(pull: &PullRequest, asked: bool) -> impl IntoElement {
    let ci = match pull.checks {
        github::Checks::Passing => theme::GREEN,
        github::Checks::Failing => theme::RED,
        github::Checks::Pending => theme::YELLOW,
        github::Checks::Missing => theme::MUTED,
    };

    div()
        .relative()
        .w(px(DOT_SLOT))
        .h(px(DOT_SLOT))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .when(asked, |this| this.border_2().border_color(rgb(theme::TEAL)))
        .child(div().w(px(8.)).h(px(8.)).rounded_full().bg(rgb(ci)))
        .when(pull.review == Review::Approved, |this| {
            // On the corner, ringed in the window's background so it reads
            // apart from the dot and the Slack ring under it.
            this.child(
                div()
                    .absolute()
                    .right(px(-8.))
                    .bottom(px(-7.))
                    .w(px(13.))
                    .h(px(13.))
                    .rounded_full()
                    .border_2()
                    .border_color(rgb(theme::BASE))
                    .bg(rgb(theme::GREEN))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(9.))
                    .line_height(px(9.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(theme::BASE))
                    .child("✓"),
            )
        })
}

/// What else matters about a PR after its action, in one muted line: repo and
/// number, whether it is a draft, CI still running, approval and comments.
/// A failing CI, conflicts and who to wait for are the action's to say; the
/// branch is in the menu.
fn meta(pull: &PullRequest) -> String {
    let mut parts = vec![format!("{} #{}", pull.repo_name(), pull.number)];
    if pull.draft {
        parts.push("borrador".to_string());
    }
    if pull.checks == github::Checks::Pending {
        parts.push("CI corriendo".to_string());
    }
    if pull.review == Review::Approved {
        parts.push("aprobado".to_string());
    }
    match pull.comments {
        0 => {}
        1 => parts.push("1 comentario".to_string()),
        n => parts.push(format!("{n} comentarios")),
    }
    parts.join(" · ")
}

/// The PR's [MASTER]/[DEV] twin, when it has one.
fn relations(pull: &PullRequest, pulls: &[PullRequest]) -> Vec<gpui::AnyElement> {
    let line = |text: String| {
        div()
            .pl(px(TITLE_INDENT))
            .text_xs()
            .text_color(rgb(theme::MUTED))
            .truncate()
            .child(text)
            .into_any_element()
    };
    let mut lines = Vec::new();
    if let Some(twin) = stack::twin(pull, pulls) {
        let prefix = stack::split_prefix(&twin.title).map_or("", |(prefix, _)| prefix);
        let state: Vec<_> = status::badges(twin)
            .into_iter()
            .map(|badge| badge.label)
            .collect();
        lines.push(line(format!(
            "Gemela {prefix} #{} · {}",
            twin.number,
            if state.is_empty() {
                "sin checks".to_string()
            } else {
                state.join(" · ")
            }
        )));
    }
    lines
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

impl review_view::Requester for Root {
    fn review_asked(
        &mut self,
        url: &str,
        channel: &str,
        github: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let asked = review::Asked {
            at: now(),
            channel: channel.to_string(),
            ..review::Asked::default()
        };
        self.review_asked.insert(url.to_string(), asked.clone());
        let saved = url.to_string();
        self.save(cx, move || review::remember(&saved, &asked));
        self.locate_request(url, cx);
        if github.is_some() {
            self.folders_error = github;
        }
        // The reviewer just asked on GitHub shows on the next load.
        self.reload(cx);
        cx.notify();
    }
}

impl Root {
    /// How long the workflow gets to post before its message is looked for.
    const LOCATE_AFTER: Duration = Duration::from_secs(8);

    /// Finds the message the workflow posted for the PR, a moment after the
    /// request, and keeps its link. Without a Slack token there is nothing
    /// to look with.
    fn locate_request(&mut self, url: &str, cx: &mut Context<Self>) {
        let (Some(token), Some(asked)) = (slack::token(), self.review_asked.get(url)) else {
            return;
        };
        let (url, channel) = (url.to_string(), asked.channel.clone());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Self::LOCATE_AFTER).await;
            let needle = url.clone();
            let found = cx
                .background_executor()
                .spawn(async move { slack::find_message(&token, &channel, &needle) })
                .await;

            this.update(cx, |this, cx| {
                match found {
                    Ok(Some(message)) => {
                        if let Some(asked) = this.review_asked.get_mut(&url) {
                            asked.ts = Some(message.ts);
                            asked.permalink = Some(message.permalink);
                            let (saved, asked) = (url.clone(), asked.clone());
                            this.save(cx, move || review::remember(&saved, &asked));
                        }
                    }
                    Ok(None) => {}
                    Err(error) => this.folders_error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Counts the replies under each request's message, for the PRs still
    /// waiting on their review, and tells about new ones. Runs with each
    /// reload of "Mis PRs".
    fn refresh_replies(&mut self, pulls: &[PullRequest], cx: &mut Context<Self>) {
        let Some(token) = slack::token() else {
            return;
        };
        let threads: Vec<(String, String, String, String, i64)> = pulls
            .iter()
            .filter(|pull| pull.review != Review::Approved)
            .filter_map(|pull| {
                let asked = self.review_asked.get(&pull.url)?;
                Some((
                    pull.url.clone(),
                    pull.title.clone(),
                    asked.channel.clone(),
                    asked.ts.clone()?,
                    asked.replies,
                ))
            })
            .collect();
        if threads.is_empty() {
            return;
        }

        cx.spawn(async move |this, cx| {
            let counted = cx
                .background_executor()
                .spawn(async move {
                    threads
                        .into_iter()
                        .filter_map(|(url, title, channel, ts, before)| {
                            let replies = slack::replies(&token, &channel, &ts).ok()?;
                            Some((url, title, before, replies as i64))
                        })
                        .collect::<Vec<_>>()
                })
                .await;

            this.update(cx, |this, cx| {
                for (url, title, before, replies) in counted {
                    let Some(asked) = this.review_asked.get_mut(&url) else {
                        continue;
                    };
                    if replies == before {
                        continue;
                    }
                    asked.replies = replies;
                    let (saved, snapshot) = (url.clone(), asked.clone());
                    let permalink = snapshot.permalink.clone().unwrap_or_default();
                    this.save(cx, move || review::remember(&saved, &snapshot));
                    if replies > before {
                        let notice = notify::Notice {
                            title: format!(
                                "{} respuesta{} en Slack",
                                replies,
                                if replies == 1 { "" } else { "s" }
                            ),
                            subtitle: "Pedido de review".to_string(),
                            body: title,
                            url: permalink,
                        };
                        cx.background_executor()
                            .spawn(async move { notify::send(&notice) })
                            .detach();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Where the PR's review request lives in Slack: the thread when it was
    /// found, else the channel.
    fn request_link(&self, url: &str) -> Option<String> {
        let asked = self.review_asked.get(url)?;
        if let Some(permalink) = &asked.permalink {
            return Some(permalink.clone());
        }
        let channel = &asked.channel;
        Some(match env::var("GH_PR_STATUS_SLACK_TEAM") {
            Ok(team) if !team.trim().is_empty() => {
                format!("slack://channel?team={}&id={channel}", team.trim())
            }
            _ => format!("https://slack.com/app_redirect?channel={channel}"),
        })
    }
}

impl Focusable for Root {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Root")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, action, _window, cx| this.refresh(action, cx)))
            .on_action(cx.listener(|this, _: &ShowMine, _window, cx| this.show(Tab::Mine, cx)))
            .on_action(cx.listener(|this, _: &ShowMerged, _window, cx| this.show(Tab::Merged, cx)))
            .on_key_down(cx.listener(|this, event, _window, cx| this.edit_key(event, cx)))
            // Letting go of the scrollbar's thumb, anywhere.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| this.stop_dragging(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| this.stop_dragging(cx)),
            )
            .flex()
            .flex_col()
            .gap_3()
            .size_full()
            .p_4()
            .bg(rgb(theme::BASE))
            .text_color(rgb(theme::TEXT))
            .child(
                div()
                    .id("columns")
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .on_drag_move(cx.listener(
                        |this, event: &gpui::DragMoveEvent<ResizeSidebar>, _window, cx| {
                            this.resize_sidebar(event.event.position.x, event.bounds.left(), cx)
                        },
                    ))
                    .on_drop(cx.listener(|this, _: &ResizeSidebar, _window, cx| {
                        let width = this.sidebar_width;
                        this.save(cx, move || folders::save_sidebar_width(width));
                    }))
                    .child(self.sidebar(window, cx))
                    .child(self.sidebar_handle())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .flex_1()
                            .min_w(px(0.))
                            .child(self.header(cx))
                            .child(self.body(cx)),
                    ),
            )
            .when_some(self.menu.as_ref(), |this, menu| this.child(self.menu(menu, cx)))
            .child(div().text_xs().text_color(rgb(theme::MUTED)).child(
                "clic en el título abre el PR · ⋯ o clic derecho: acciones · arrástralo a un grupo · cmd-r actualiza · cmd-1/2 pestañas",
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
            KeyBinding::new("cmd-2", ShowMerged, Some("Root")),
            // The comment field of the review window.
            KeyBinding::new("enter", text_input::Newline, Some("TextInput")),
            KeyBinding::new("backspace", text_input::Backspace, Some("TextInput")),
            KeyBinding::new("delete", text_input::Delete, Some("TextInput")),
            KeyBinding::new(
                "cmd-backspace",
                text_input::DeleteToStart,
                Some("TextInput"),
            ),
            KeyBinding::new("alt-backspace", text_input::DeleteWord, Some("TextInput")),
            KeyBinding::new("left", text_input::Left, Some("TextInput")),
            KeyBinding::new("right", text_input::Right, Some("TextInput")),
            KeyBinding::new("shift-left", text_input::SelectLeft, Some("TextInput")),
            KeyBinding::new("shift-right", text_input::SelectRight, Some("TextInput")),
            KeyBinding::new("cmd-left", text_input::Home, Some("TextInput")),
            KeyBinding::new("cmd-right", text_input::End, Some("TextInput")),
            KeyBinding::new("home", text_input::Home, Some("TextInput")),
            KeyBinding::new("end", text_input::End, Some("TextInput")),
            KeyBinding::new("up", text_input::Home, Some("TextInput")),
            KeyBinding::new("down", text_input::End, Some("TextInput")),
            KeyBinding::new("shift-cmd-left", text_input::SelectHome, Some("TextInput")),
            KeyBinding::new("shift-cmd-right", text_input::SelectEnd, Some("TextInput")),
            KeyBinding::new("cmd-a", text_input::SelectAll, Some("TextInput")),
            KeyBinding::new("cmd-c", text_input::Copy, Some("TextInput")),
            KeyBinding::new("cmd-x", text_input::Cut, Some("TextInput")),
            KeyBinding::new("cmd-v", text_input::Paste, Some("TextInput")),
        ]);

        let bounds = Bounds::centered(None, size(px(960.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("PR Manager")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_name_waits_slides_to_its_end_holds_and_comes_back() {
        let overflow = 60.;
        let slide = overflow / MARQUEE_SPEED;

        let at = |elapsed: f32| marquee_offset(elapsed, overflow);

        assert_eq!(at(MARQUEE_WAIT / 2.), 0.);
        assert_eq!(at(MARQUEE_WAIT + slide / 2.), overflow / 2.);
        assert_eq!(at(MARQUEE_WAIT + slide + MARQUEE_HOLD / 2.), overflow);
        assert_eq!(at(MARQUEE_WAIT + slide + MARQUEE_HOLD + MARQUEE_RETURN), 0.);
    }
}
