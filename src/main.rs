//! A GPUI window over the user's open pull requests on GitHub.
//!
//! Two tabs: the PRs the user authored, grouped by what they are waiting on,
//! and the user's merged PRs. Under each of the user's PRs sits the latest CI/CD run as the pipeline
//! bot reports it. A sidebar holds the groups the user drags PRs into; picking
//! one narrows every tab to it.
//!
//! The open PRs reload every minute; while one of the user's deploys is running,
//! "Mis PRs" reloads every 20 seconds so the run can be followed live. macOS
//! notifications announce when a deploy finishes or fails and when a PR is
//! approved.

mod db;
mod folders;
mod github;
mod merged;
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
    AnimationExt, App, Application, Bounds, ClipboardItem, Context, FocusHandle, Focusable,
    KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Point, SharedString,
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
/// How long the deploy button waits for the confirming second click.
const CONFIRM_FOR: Duration = Duration::from_secs(5);
/// The sidebar's width, and the room its counts take, which leave a fixed
/// width for each name: 180 minus padding, border, gap and count.
const SIDEBAR_WIDTH: f32 = 180.;
const COUNT_WIDTH: f32 = 20.;
const LABEL_WIDTH: f32 = SIDEBAR_WIDTH - 2. * 8. - 2. - 8. - COUNT_WIDTH;
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
    /// First click done; a second one before the deadline sends the comment.
    Confirming(Instant),
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
    /// dropping that PR on "Nuevo grupo".
    New {
        pull: Option<String>,
    },
    Rename(i64),
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
            focus_handle: cx.focus_handle(),
        };
        root.start_clock(cx);
        Self::load_folders(cx);
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
            let result = cx
                .background_executor()
                .spawn(async move { github::fetch() })
                .await;

            this.update(cx, |this, cx| {
                if let Ok(snapshot) = &result {
                    this.notify_changes(&snapshot.pulls, cx);
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

    fn notify_changes(&mut self, pulls: &[PullRequest], cx: &mut Context<Self>) {
        let (notices, seen) = notify::transitions(self.last_seen.as_ref(), pulls);
        self.last_seen = Some(seen);

        if !notices.is_empty() {
            cx.background_executor()
                .spawn(async move { notices.iter().for_each(notify::send) })
                .detach();
        }
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
            Filter::Folder(id) => self.folders.by_url.get(url) == Some(&id),
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

    fn start_editing(&mut self, target: Editing, cx: &mut Context<Self>) {
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
            }
            Editing::New { pull } => {
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move { folders::create(&name) })
                        .await;

                    this.update(cx, |this, cx| {
                        match result {
                            Ok(folder) => {
                                let id = folder.id;
                                this.folders.list.push(folder);
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

    /// Removes the group; its PRs go back to "Sin grupo".
    fn delete_folder(&mut self, id: i64, cx: &mut Context<Self>) {
        self.folders.list.retain(|folder| folder.id != id);
        self.folders.by_url.retain(|_, folder| *folder != id);
        if self.filter == Filter::Folder(id) {
            self.filter = Filter::All;
        }
        self.save(cx, move || folders::delete(id));
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

    /// The buttons to the right of a PR: ask for a review on Slack until the PR
    /// is approved, and comment `/deploy` unless a run is already going. Only
    /// on the user's own PRs.
    fn actions(&self, pull: &PullRequest, cx: &mut Context<Self>) -> impl IntoElement {
        let mine = pull.author == self.login().unwrap_or_default();
        let slack =
            slack_channel().filter(|_| mine && !pull.draft && pull.review != Review::Approved);
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
            .gap_1()
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
        let items: Vec<_> = match &menu.target {
            MenuFor::Pull(url) => {
                let copied = url.clone();
                let mut items = vec![menu_item(
                    "menu-copy-url",
                    "Copiar URL del PR",
                    cx.listener(move |_this, _event, _window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                    }),
                )];
                if self.folders.by_url.contains_key(url) {
                    let url = url.clone();
                    items.push(menu_item(
                        "menu-ungroup",
                        "Sacar del grupo",
                        cx.listener(move |this, _event, _window, cx| this.move_to(&url, None, cx)),
                    ));
                }
                items
            }
            MenuFor::Folder(id) => {
                let id = *id;
                vec![
                    menu_item(
                        "menu-rename",
                        "Renombrar",
                        cx.listener(move |this, _event, _window, cx| {
                            this.start_editing(Editing::Rename(id), cx)
                        }),
                    ),
                    menu_item(
                        "menu-delete",
                        "Eliminar grupo",
                        cx.listener(move |this, _event, _window, cx| this.delete_folder(id, cx)),
                    ),
                ]
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
    /// group. A PR dropped on a group moves into it; dropped on "Sin grupo",
    /// out of the one it was in; dropped on "Nuevo grupo", into a new one.
    fn sidebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.open_urls();
        let in_folder = |id: i64| {
            open.iter()
                .filter(|url| self.folders.by_url.get(**url) == Some(&id))
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

        let folders = self
            .folders
            .list
            .iter()
            .map(|folder| {
                let id = folder.id;
                match editing(&|target| matches!(target, Editing::Rename(other) if *other == id)) {
                    Some(text) => self.name_field(text, cx).into_any_element(),
                    None => self
                        .sidebar_entry(
                            folder.name.clone(),
                            in_folder(id),
                            Filter::Folder(id),
                            window,
                            cx,
                        )
                        .on_mouse_down(MouseButton::Right, self.open_menu(MenuFor::Folder(id), cx))
                        .into_any_element(),
                }
            })
            .collect::<Vec<_>>();
        let new_name = editing(&|target| matches!(target, Editing::New { .. }));

        div()
            .id("sidebar")
            .flex()
            .flex_col()
            .gap_1()
            .w(px(SIDEBAR_WIDTH))
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
            .child(div().h(px(1.)).my_1().bg(rgb(theme::BORDER)))
            .children(folders)
            .child(match new_name {
                Some(text) => self.name_field(text, cx).into_any_element(),
                None => div()
                    .id("folder-new")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::BASE))
                    .text_sm()
                    .text_color(rgb(theme::MUTED))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
                    .child("+ Nuevo grupo")
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.start_editing(Editing::New { pull: None }, cx)
                    }))
                    .drag_over::<Dragged>(|style, _, _, _| drop_highlight(style))
                    .on_drop(cx.listener(|this, dragged: &Dragged, _window, cx| {
                        this.start_editing(
                            Editing::New {
                                pull: Some(dragged.url.clone()),
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
        let selected = self.filter == filter;
        let hovered = self.hovered == Some(filter);

        div()
            .id(id)
            .flex()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(theme::BASE))
            .text_sm()
            .cursor_pointer()
            .when(selected, |this| this.bg(rgb(theme::SURFACE_ACTIVE)))
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
            .child(label_element(label, hovered, window))
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
    fn name_field(&self, text: String, cx: &mut Context<Self>) -> impl IntoElement {
        div()
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
            .gap_2()
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
                let pulls = self.visible(&snapshot.pulls);
                let merged = self.folder_merged();
                if pulls.is_empty() && merged.is_empty() {
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
                self.mine_list(&pulls, &merged, cx).into_any_element()
            }
        }
    }

    /// The PRs of `pulls` the sidebar's filter lets through.
    fn visible<'a>(&self, pulls: &'a [PullRequest]) -> Vec<&'a PullRequest> {
        pulls.iter().filter(|pull| self.shows(&pull.url)).collect()
    }

    /// The merged PRs of the selected group, so a group's view shows what of
    /// it already shipped. Empty unless a group is selected.
    fn folder_merged(&self) -> Vec<&merged::Merged> {
        match (self.filter, &self.merged.pulls) {
            (Filter::Folder(_), Some(pulls)) => {
                pulls.iter().filter(|pull| self.shows(&pull.url)).collect()
            }
            _ => Vec::new(),
        }
    }

    /// The user's own PRs: a one-line count per group, then one section per
    /// group, and last the selected group's merged PRs.
    fn mine_list(
        &self,
        pulls: &[&PullRequest],
        merged: &[&merged::Merged],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now = now();
        let in_group = |group: Group| {
            pulls
                .iter()
                .copied()
                .filter(move |pull| Group::of(pull, now) == group)
        };

        let mut sections: Vec<_> = Group::ALL
            .into_iter()
            .filter_map(|group| {
                let rows: Vec<_> = in_group(group)
                    .map(|pull| self.row(pull, group.color(), cx).into_any_element())
                    .collect();
                (!rows.is_empty())
                    .then(|| section(group.title(), group.color(), rows).into_any_element())
            })
            .collect();
        if !merged.is_empty() {
            let rows = merged
                .iter()
                .map(|pull| self.merged_row(pull, true, cx).into_any_element())
                .collect();
            sections.push(section("Mergeadas", theme::ACCENT, rows).into_any_element());
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
                        section(&merged::day_label(day, today), color, rows)
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

    /// One PR. The colored stripe repeats the section's color so a row keeps its
    /// meaning when scrolled away from the heading. The PR and its pipeline are
    /// separate click targets: one opens GitHub, the other unfolds the run.
    fn row(&self, pull: &PullRequest, stripe: u32, cx: &mut Context<Self>) -> impl IntoElement {
        let expanded = self.expanded.contains(&pull.url);

        div()
            .id(SharedString::from(format!("row-{}", pull.url)))
            .on_drag(
                Dragged {
                    url: pull.url.clone(),
                    title: pull.title.clone().into(),
                },
                drag_preview,
            )
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
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        self.open_menu(MenuFor::Pull(pull.url.clone()), cx),
                                    )
                                    .child(summary(pull)),
                            )
                            .child(self.actions(pull, cx)),
                    )
                    .when(!pull.failing_checks.is_empty(), |this| {
                        this.child(failures(pull))
                    })
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

/// A sidebar name in its fixed width. Cut with an ellipsis, or, while hovered
/// and too long to fit, sliding back and forth so all of it can be read.
fn label_element(label: String, hovered: bool, window: &Window) -> gpui::AnyElement {
    let overflow = text_width(&label, window) - LABEL_WIDTH;
    let frame = div().w(px(LABEL_WIDTH)).flex_shrink_0().overflow_hidden();
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

fn menu_item(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
        .child(label)
        .on_click(on_click)
        .into_any_element()
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

fn section(title: &str, color: u32, rows: Vec<gpui::AnyElement>) -> impl IntoElement + use<> {
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
fn summary(pull: &PullRequest) -> impl IntoElement {
    let url = pull.url.clone();
    let context = format!(
        "{} #{} · {} ← {}",
        pull.repo_name(),
        pull.number,
        pull.base,
        pull.head
    );

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
                }),
        )
}

/// Every failed check with why it failed; clicking one opens its logs.
fn failures(pull: &PullRequest) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_0p5()
        .p_1()
        .rounded_md()
        .bg(rgb(theme::MANTLE))
        .text_xs()
        .children(
            pull.failing_checks
                .iter()
                .enumerate()
                .map(|(index, check)| {
                    let url = check.url.clone();
                    div()
                        .id(SharedString::from(format!("check-{index}-{}", pull.url)))
                        .flex()
                        .gap_2()
                        .px_1()
                        .rounded_sm()
                        .when_some(url, |this, url| {
                            this.cursor_pointer()
                                .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
                                .on_click(move |_event, _window, cx| cx.open_url(&url))
                        })
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(rgb(theme::RED))
                                .child(format!("✗ {}", check.name)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(rgb(theme::SUBTEXT))
                                .child(check.reason.clone().unwrap_or_else(|| {
                                    if check.url.is_some() {
                                        "ver logs".to_string()
                                    } else {
                                        String::new()
                                    }
                                })),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Root")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, action, _window, cx| this.refresh(action, cx)))
            .on_action(cx.listener(|this, _: &ShowMine, _window, cx| this.show(Tab::Mine, cx)))
            .on_action(cx.listener(|this, _: &ShowMerged, _window, cx| this.show(Tab::Merged, cx)))
            .on_key_down(cx.listener(|this, event, _window, cx| this.edit_key(event, cx)))
            .flex()
            .flex_col()
            .gap_3()
            .size_full()
            .p_4()
            .bg(rgb(theme::BASE))
            .text_color(rgb(theme::TEXT))
            .child(
                div()
                    .flex()
                    .gap_4()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.sidebar(window, cx))
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
                "clic abre el PR · arrástralo a un grupo · clic derecho para más · cmd-r actualiza · cmd-1/2 pestañas",
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
        ]);

        let bounds = Bounds::centered(None, size(px(960.), px(760.)), cx);
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
