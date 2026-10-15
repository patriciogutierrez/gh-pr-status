//! Shared widgets. Plain functions returning elements, which is all a
//! "component" needs to be in GPUI.

use gpui::{App, ClickEvent, ElementId, SharedString, Window, div, prelude::*, rgb};

use crate::theme;

/// A regular pill button. `enabled == false` drops the click handler and the
/// hover/active styling, so it reads as inert.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h_8()
        .px_3()
        .rounded_lg()
        .bg(rgb(if enabled {
            theme::SURFACE
        } else {
            theme::MANTLE
        }))
        .text_sm()
        .text_color(rgb(if enabled { theme::TEXT } else { theme::MUTED }))
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
                .active(|style| style.bg(rgb(theme::SURFACE_ACTIVE)))
                .on_click(on_click)
        })
        .child(label.into())
}

/// One segment of the tab switch.
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
        .h_8()
        .px_3()
        .rounded_lg()
        .border_1()
        .text_sm()
        .cursor_pointer()
        .bg(rgb(if selected {
            theme::SURFACE_ACTIVE
        } else {
            theme::SURFACE
        }))
        .border_color(rgb(if selected {
            theme::ACCENT
        } else {
            theme::BORDER
        }))
        .text_color(rgb(if selected {
            theme::ACCENT
        } else {
            theme::SUBTEXT
        }))
        .hover(|style| style.bg(rgb(theme::SURFACE_HOVER)))
        .child(label.into())
        .on_click(on_click)
}

/// A screen heading: title on top, dimmed caption underneath.
pub fn heading(
    title: impl Into<SharedString>,
    caption: impl Into<SharedString>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .text_lg()
                .text_color(rgb(theme::ACCENT))
                .child(title.into()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::MUTED))
                .child(caption.into()),
        )
}

/// A small colored status label.
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

/// A count with its caption, for the summary row.
pub fn stat(count: usize, caption: impl Into<SharedString>, color: u32) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .gap_0p5()
        .p_3()
        .rounded_lg()
        .bg(rgb(theme::SURFACE))
        .child(
            div()
                .text_xl()
                .text_color(rgb(if count == 0 { theme::MUTED } else { color }))
                .child(count.to_string()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::SUBTEXT))
                .child(caption.into()),
        )
}
