//! Shared widgets. Plain functions returning elements, which is all a
//! "component" needs to be in GPUI.

use gpui::{App, ClickEvent, ElementId, SharedString, Window, div, prelude::*, px, rgb, rgba};

use crate::theme;

/// A quiet outlined button: no fill until hovered.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .h_6()
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(theme::SURFACE))
        .text_xs()
        .whitespace_nowrap()
        .text_color(rgb(if enabled {
            theme::SUBTEXT
        } else {
            theme::MUTED
        }))
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|style| style.bg(rgb(theme::SURFACE)).text_color(rgb(theme::TEXT)))
                .active(|style| style.bg(rgb(theme::SURFACE_ACTIVE)))
                .on_click(on_click)
        })
        .child(label.into())
}

/// One segment of the tab switch; the switch itself is the dark strip the
/// caller puts the segments in.
pub fn toggle_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h_7()
        .px_3()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .when(selected, |this| this.bg(rgb(theme::SURFACE)))
        .text_color(rgb(if selected {
            theme::TEXT
        } else {
            theme::SUBTEXT
        }))
        .when(!selected, |this| {
            this.hover(|style| style.text_color(rgb(theme::TEXT)))
        })
        .child(label.into())
        .on_click(on_click)
}

/// The one colored label a PR gets: its next action, tinted with its color.
pub fn pill(label: impl Into<SharedString>, color: u32) -> impl IntoElement {
    div()
        .min_w(px(0.))
        .max_w_full()
        .px_2()
        .py_0p5()
        .rounded_full()
        .bg(rgba((color << 8) | 0x29))
        .text_xs()
        .truncate()
        .text_color(rgb(color))
        .child(label.into())
}

/// A small colored status label, for the pipeline line.
pub fn badge(label: impl Into<SharedString>, color: u32) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px_2()
        .py_0p5()
        .rounded_md()
        .bg(rgb(theme::MANTLE))
        .text_xs()
        .text_color(rgb(color))
        .child(label.into())
}
