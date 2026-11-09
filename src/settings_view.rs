//! The settings window: one field per setting, filled with what is saved,
//! a list's entries one per line.
//! A secret's field starts empty and says whether one is saved, so the
//! window never shows it; typing replaces it. Tab moves to the next field,
//! cmd-enter saves and escape closes.

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, FontWeight, KeyDownEvent, Render,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::*, px, rgb,
    size,
};

use crate::settings::{self, ALL, REVIEW_CHANNELS, REVIEWERS, Setting};
use crate::text_input::TextInput;
use crate::{theme, ui};

const WIDTH: f32 = 620.;
const HEIGHT: f32 = 720.;
/// The fields that take a list, which get room for a few lines.
const LISTS: [&str; 2] = [REVIEWERS, REVIEW_CHANNELS];

pub struct SettingsView {
    fields: Vec<(&'static Setting, Entity<TextInput>)>,
    /// What each field started with, to save only what changed.
    initial: Vec<String>,
    saving: bool,
    error: Option<String>,
    /// Runs once the settings are saved, for the main window to reload.
    on_saved: Box<dyn Fn(&mut App)>,
    focus_handle: FocusHandle,
}

/// Opens the settings in a window of its own, centered, or brings the one
/// already open to the front.
pub fn open(on_saved: impl Fn(&mut App) + 'static, cx: &mut App) -> Result<(), String> {
    if let Some(open) = cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<SettingsView>())
    {
        return open
            .update(cx, |_, window, _| window.activate_window())
            .map_err(|error| format!("No se pudo abrir la ventana: {error}"));
    }
    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    cx.open_window(
        WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from("Ajustes")),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |window, cx| {
            let mut initial = Vec::new();
            let fields: Vec<_> = ALL
                .iter()
                .map(|setting| {
                    let saved = settings::get(setting.key);
                    let (text, placeholder) = match (setting.secret, saved) {
                        (true, Some(_)) => (String::new(), "Guardado en el Llavero"),
                        (true, None) => (String::new(), "Sin configurar"),
                        // A list shows one entry per line.
                        (false, Some(saved)) if LISTS.contains(&setting.key) => {
                            (saved.split(',').collect::<Vec<_>>().join("\n"), "")
                        }
                        (false, saved) => (saved.unwrap_or_default(), ""),
                    };
                    initial.push(text.clone());
                    let input = cx.new(|cx| TextInput::with_text(placeholder, text, cx));
                    (setting, input)
                })
                .collect();
            if let Some((_, first)) = fields.first() {
                window.focus(&first.read(cx).focus_handle(cx));
            }
            cx.new(|cx| SettingsView {
                fields,
                initial,
                saving: false,
                error: None,
                on_saved: Box::new(on_saved),
                focus_handle: cx.focus_handle(),
            })
        },
    )
    .map(drop)
    .map_err(|error| format!("No se pudo abrir la ventana: {error}"))
}

impl SettingsView {
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        match keystroke.key.as_str() {
            "enter" if keystroke.modifiers.platform && !self.saving => self.save(window, cx),
            "escape" if !self.saving => window.remove_window(),
            "tab" => self.focus_next(keystroke.modifiers.shift, window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn focus_next(&self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.fields.len();
        let current = self
            .fields
            .iter()
            .position(|(_, input)| input.read(cx).focus_handle(cx).is_focused(window))
            .unwrap_or(0);
        let next = if back {
            (current + count - 1) % count
        } else {
            (current + 1) % count
        };
        window.focus(&self.fields[next].1.read(cx).focus_handle(cx));
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let changes: Vec<_> = self
            .fields
            .iter()
            .zip(&self.initial)
            .filter_map(|((setting, input), initial)| {
                let text = input.read(cx).text();
                (text != initial).then(|| (setting.key, one_line(setting.key, text)))
            })
            .collect();
        self.saving = true;
        self.error = None;
        cx.notify();

        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { settings::save(changes) })
                .await;

            this.update(cx, |this, cx| {
                this.saving = false;
                match result {
                    Ok(()) => {
                        (this.on_saved)(cx);
                        window_handle
                            .update(cx, |_, window, _| window.remove_window())
                            .ok();
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn field(&self, setting: &Setting, input: &Entity<TextInput>) -> impl IntoElement {
        let list = LISTS.contains(&setting.key);
        div()
            .flex()
            .items_start()
            .gap_3()
            .child(
                div()
                    .w(px(170.))
                    .flex_shrink_0()
                    .pt_1()
                    .flex()
                    .flex_col()
                    .child(div().text_sm().child(setting.label))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child(setting.help),
                    ),
            )
            .child(
                // `min_w(0)` keeps the box at the row's width, so long text
                // wraps instead of widening it.
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(if list { 104. } else { 30. }))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::MANTLE))
                    .overflow_hidden()
                    .text_sm()
                    .child(input.clone()),
            )
    }
}

/// What a field's text saves as: a list's lines joined by commas, any other
/// field's on one line.
fn one_line(key: &str, text: &str) -> String {
    if LISTS.contains(&key) {
        text.split(['\n', ','])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(",")
    } else {
        text.lines().map(str::trim).collect()
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Settings")
            .track_focus(&self.focus_handle)
            .capture_key_down(cx.listener(|this, event, window, cx| this.key(event, window, cx)))
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
                    .child("Ajustes"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child("Se guardan en este Mac; el webhook y el token, en el Llavero."),
            )
            .child(
                div()
                    .id("settings-fields")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .children(
                        self.fields
                            .iter()
                            .map(|(setting, input)| self.field(setting, input)),
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
                        "settings-cancel",
                        "Cancelar",
                        !self.saving,
                        cx.listener(|_this, _event, window, _cx| window.remove_window()),
                    ))
                    .child(ui::button(
                        "settings-save",
                        if self.saving {
                            "Guardando…"
                        } else {
                            "Guardar"
                        },
                        !self.saving,
                        cx.listener(|this, _event, window, cx| this.save(window, cx)),
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_typed_on_several_lines_saves_as_one_comma_separated_line() {
        assert_eq!(
            one_line(REVIEWERS, "Matute:U1:nmaturana,\n Nico:U2:nmosnaim\n"),
            "Matute:U1:nmaturana,Nico:U2:nmosnaim"
        );
    }

    #[test]
    fn a_plain_field_loses_its_line_breaks() {
        assert_eq!(one_line(settings::BOT, "toku-bot\n"), "toku-bot");
    }
}
