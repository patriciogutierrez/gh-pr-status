//! The window that asks for a review: who, how urgent, where and a comment,
//! for one PR. It is its own window so the main one keeps its place; sending
//! posts to the Slack workflow and closes it.

use gpui::{
    App, Bounds, Context, FocusHandle, Focusable, FontWeight, KeyDownEvent, MouseButton,
    MouseDownEvent, Render, SharedString, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowOptions, deferred, div, prelude::*, px, rgb, size,
};

use crate::review::{self, Config, Request, URGENCIES};
use crate::{theme, ui};

/// The window's size: enough for the fields and a short comment.
const WIDTH: f32 = 460.;
const HEIGHT: f32 = 380.;

/// The form's choices.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Field {
    Reviewer,
    Urgency,
    Channel,
}

/// What the main window hears back once the request is sent.
pub trait Requester: 'static {
    /// The request for `url` went to `channel`.
    fn review_asked(&mut self, url: &str, channel: &str, cx: &mut Context<Self>)
    where
        Self: Sized;
}

pub struct ReviewView<R: Requester> {
    root: WeakEntity<R>,
    config: Config,
    url: String,
    title: String,
    /// Nobody until the user picks someone.
    reviewer: Option<usize>,
    urgency: usize,
    channel: usize,
    comment: String,
    sending: bool,
    error: Option<String>,
    /// The choice whose options are open under its field.
    menu: Option<Field>,
    /// What was typed into the open choice, to filter its options.
    query: String,
    focus_handle: FocusHandle,
}

/// Opens the form in a window of its own, centered, for the PR at `url`.
pub fn open<R: Requester>(
    root: WeakEntity<R>,
    config: Config,
    url: String,
    title: String,
    cx: &mut App,
) -> Result<(), String> {
    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    cx.open_window(
        WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from("Pedir review")),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| ReviewView {
                root,
                config,
                url,
                title,
                reviewer: None,
                urgency: 1,
                channel: 0,
                comment: String::new(),
                sending: false,
                error: None,
                menu: None,
                query: String::new(),
                focus_handle: cx.focus_handle(),
            });
            window.focus(&view.read(cx).focus_handle);
            view
        },
    )
    .map(drop)
    .map_err(|error| format!("No se pudo abrir la ventana: {error}"))
}

impl<R: Requester> ReviewView<R> {
    /// Typing goes to the open choice's filter when one is open (enter picks
    /// the first match, escape closes it), else to the comment (enter sends,
    /// escape closes the window).
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let typed = match keystroke.key.as_str() {
            "enter" | "escape" | "backspace" => None,
            _ if keystroke.modifiers.platform || keystroke.modifiers.control => return,
            _ => match &keystroke.key_char {
                Some(typed) if !typed.chars().any(char::is_control) => Some(typed.clone()),
                _ => return,
            },
        };
        if let Some(field) = self.menu {
            match (keystroke.key.as_str(), typed) {
                ("enter", _) => {
                    if let Some((index, _)) = self.matches(field).into_iter().next() {
                        self.pick(field, index);
                    }
                    self.close_menu();
                }
                ("escape", _) => self.close_menu(),
                ("backspace", _) => {
                    self.query.pop();
                }
                (_, Some(typed)) => self.query.push_str(&typed),
                _ => return,
            }
        } else {
            match (keystroke.key.as_str(), typed) {
                ("enter", _) if !self.sending => self.send(window, cx),
                ("escape", _) if !self.sending => window.remove_window(),
                ("backspace", _) => {
                    self.comment.pop();
                }
                (_, Some(typed)) => self.comment.push_str(&typed),
                _ => return,
            }
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn open_menu(&mut self, field: Field) {
        self.menu = Some(field);
        self.query.clear();
    }

    fn close_menu(&mut self) {
        self.menu = None;
        self.query.clear();
    }

    /// The options of `field` that the typed filter lets through, with their
    /// indexes; every option when nothing is typed. Case and accents don't
    /// count.
    fn matches(&self, field: Field) -> Vec<(usize, String)> {
        let wanted = review::plain(&self.query).to_lowercase();
        self.options(field)
            .into_iter()
            .enumerate()
            .filter(|(_, name)| review::plain(name).to_lowercase().contains(&wanted))
            .collect()
    }

    /// Posts the request; the window closes once Slack accepts it, and the
    /// main window hears which PR it was for.
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(reviewer), Some(channel)) = (
            self.reviewer
                .and_then(|index| self.config.reviewers.get(index)),
            self.config.channels.get(self.channel),
        ) else {
            self.error = Some("Elige a quién pedirle el review.".to_string());
            cx.notify();
            return;
        };
        let request = Request {
            reviewer: reviewer.id.clone(),
            channel: channel.id.clone(),
            urgency: URGENCIES[self.urgency].1.to_string(),
            comment: self.comment.trim().to_string(),
            pr_url: self.url.clone(),
        };
        let config = self.config.clone();
        self.sending = true;
        self.error = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { review::send(&config, &request) })
                .await;

            this.update_in(cx, |this, window, cx| match result {
                Ok(()) => {
                    let url = this.url.clone();
                    let channel = this
                        .config
                        .channels
                        .get(this.channel)
                        .map(|c| c.id.clone())
                        .unwrap_or_default();
                    this.root
                        .update(cx, |root, cx| root.review_asked(&url, &channel, cx))
                        .ok();
                    window.remove_window();
                }
                Err(error) => {
                    this.sending = false;
                    this.error = Some(error);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn name(&self, field: Field) -> String {
        match field {
            Field::Reviewer => self
                .reviewer
                .and_then(|index| self.config.reviewers.get(index))
                .map(|c| c.name.clone())
                .unwrap_or_else(|| "Elige…".to_string()),
            Field::Channel => self
                .config
                .channels
                .get(self.channel)
                .map(|c| format!("#{}", c.name))
                .unwrap_or_default(),
            Field::Urgency => URGENCIES[self.urgency].0.to_string(),
        }
    }

    fn options(&self, field: Field) -> Vec<String> {
        match field {
            Field::Reviewer => self
                .config
                .reviewers
                .iter()
                .map(|c| c.name.clone())
                .collect(),
            Field::Channel => self
                .config
                .channels
                .iter()
                .map(|c| format!("#{}", c.name))
                .collect(),
            Field::Urgency => URGENCIES.iter().map(|(word, _)| word.to_string()).collect(),
        }
    }

    fn pick(&mut self, field: Field, index: usize) {
        match field {
            Field::Reviewer => self.reviewer = Some(index),
            Field::Urgency => self.urgency = index,
            Field::Channel => self.channel = index,
        }
    }

    /// A labeled choice. Clicking it opens the options right under the
    /// field; typing then filters them, and the field shows what is typed.
    fn choice(
        &self,
        label: &'static str,
        field: Field,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.menu == Some(field);
        let shown = if open {
            format!("{}▏", self.query)
        } else {
            self.name(field)
        };

        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .w(px(80.))
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(label),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .child(
                        div()
                            .id(SharedString::from(format!("choice-{label}")))
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if open { theme::ACCENT } else { theme::SURFACE }))
                            .text_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(theme::SURFACE)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _event: &MouseDownEvent, _window, cx| {
                                    if this.menu == Some(field) {
                                        this.close_menu();
                                    } else {
                                        this.open_menu(field);
                                    }
                                    cx.notify();
                                }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .when(open && self.query.is_empty(), |this| {
                                        this.text_color(rgb(theme::MUTED))
                                    })
                                    .child(if open && self.query.is_empty() {
                                        "Escribe para filtrar▏".to_string()
                                    } else {
                                        shown
                                    }),
                            )
                            .child(div().text_xs().text_color(rgb(theme::MUTED)).child("▾")),
                    )
                    .when(open, |this| this.child(self.options_list(field, cx))),
            )
    }

    /// The open choice's options that match what is typed, hanging under
    /// its field and painted over whatever follows.
    fn options_list(&self, field: Field, cx: &mut Context<Self>) -> impl IntoElement {
        let matches = self.matches(field);
        let items = if matches.is_empty() {
            vec![
                div()
                    .px_2()
                    .py_1()
                    .text_color(rgb(theme::MUTED))
                    .child("Sin coincidencias")
                    .into_any_element(),
            ]
        } else {
            matches
                .into_iter()
                .map(|(index, name)| {
                    ui::menu_item(
                        SharedString::from(format!("option-{index}")),
                        name,
                        cx.listener(move |this, _event, _window, cx| {
                            this.pick(field, index);
                            this.close_menu();
                            cx.notify();
                        }),
                    )
                })
                .collect()
        };

        deferred(
            div()
                .id("options")
                .occlude()
                .absolute()
                .top(px(34.))
                .left_0()
                .w_full()
                .flex()
                .flex_col()
                .p_1()
                .max_h(px(220.))
                .overflow_y_scroll()
                .rounded_lg()
                .border_1()
                .border_color(rgb(theme::BORDER))
                .bg(rgb(theme::SURFACE))
                .shadow_lg()
                .text_sm()
                .on_mouse_down_out(cx.listener(|this, _event, _window, cx| {
                    this.close_menu();
                    cx.notify();
                }))
                .children(items),
        )
        .with_priority(1)
    }
}

impl<R: Requester> Focusable for ReviewView<R> {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl<R: Requester> Render for ReviewView<R> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Review")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event, window, cx| this.key(event, window, cx)))
            .flex()
            .flex_col()
            .gap_3()
            .size_full()
            .p_4()
            .bg(rgb(theme::BASE))
            .text_color(rgb(theme::TEXT))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(self.title.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .truncate()
                    .child(self.url.clone()),
            )
            .child(self.choice("Reviewer", Field::Reviewer, cx))
            .child(self.choice("Urgencia", Field::Urgency, cx))
            .child(self.choice("Canal", Field::Channel, cx))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_3()
                    .flex_1()
                    .child(
                        div()
                            .w(px(80.))
                            .pt_1()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child("Comentario"),
                    )
                    .child(
                        // `min_w(0)` keeps the box at the row's width, so long
                        // text wraps instead of widening it.
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .h_full()
                            .min_h(px(56.))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(theme::ACCENT))
                            .bg(rgb(theme::MANTLE))
                            .overflow_hidden()
                            .text_sm()
                            .child(if self.comment.is_empty() {
                                div().text_color(rgb(theme::MUTED)).child("Escribe aquí▏")
                            } else {
                                div().child(format!("{}▏", self.comment))
                            }),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(div().text_xs().text_color(rgb(theme::RED)).child(error))
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(ui::button(
                        "review-cancel",
                        "Cancelar",
                        !self.sending,
                        cx.listener(|_this, _event, window, _cx| window.remove_window()),
                    ))
                    .child(ui::button(
                        "review-send",
                        if self.sending {
                            "Enviando…"
                        } else {
                            "Enviar a Slack"
                        },
                        !self.sending,
                        cx.listener(|this, _event, window, cx| this.send(window, cx)),
                    )),
            )
    }
}
