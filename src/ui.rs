//! Shared widgets. Plain functions returning elements, which is all a
//! "component" needs to be in GPUI.

use gpui::{App, ClickEvent, ElementId, SharedString, Window, div, prelude::*, rgb};

use crate::theme;

/// A compact outlined button for actions inside a row, so it stands out from
/// the row's own surface.
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
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::MANTLE))
        .text_xs()
        .whitespace_nowrap()
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
