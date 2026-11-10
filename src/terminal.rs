//! A terminal inside the app, for a group's agent: a login shell running a
//! command in a pseudo-terminal, read by Alacritty's emulator and drawn here
//! cell by cell. Typing, pasting, copying a selection, opening links with
//! cmd-click and scrolling back work as in macOS terminals.

use std::borrow::Cow;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty::{self, Shell};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};
use gpui::{
    App, Bounds, ClipboardItem, Context, FocusHandle, Focusable, Font, FontStyle, FontWeight, Hsla,
    KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, Render, ScrollDelta, ScrollWheelEvent, Size, TextRun, UnderlineStyle, Window, canvas,
    div, fill, font, point, prelude::*, px, rgb, size,
};

use crate::theme;

const FONT: &str = "Menlo";
const FONT_SIZE: f32 = 13.;
const LINE_HEIGHT: f32 = 17.;
/// The terminal's own background, a step under the app's.
const BACKGROUND: u32 = theme::MANTLE;
/// The first 16 colors, Catppuccin Mocha's, as the rest of the app.
const ANSI: [u32; 16] = [
    0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de, 0x585b70,
    0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
];

pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    /// Where the grid was last drawn and its size, to place the mouse and
    /// notice a resize.
    layout: Rc<std::cell::Cell<Layout>>,
    /// What the program running asked the window to be called.
    pub title: Option<String>,
    /// Whether the shell is gone; nothing more can be typed.
    pub exited: bool,
    /// The text selected with the mouse. Kept here rather than in the
    /// emulator, which drops it whenever the lines under it are rewritten,
    /// and Claude Code rewrites its own all the time.
    selection: Rc<RefCell<Option<Selection>>>,
    /// Whether the mouse went down on the grid and is selecting.
    selecting: bool,
    /// Whether cmd is held over a link, which then takes the hand cursor.
    over_link: bool,
    /// The cell a press was reported in to a program that asked for the
    /// mouse, while the button is down, and the last cell a move was.
    reported: Option<(usize, usize)>,
    last_motion: Option<(usize, usize)>,
    /// Wheel scroll left over, under a line.
    scroll_rest: f32,
    focus_handle: FocusHandle,
}

#[derive(Copy, Clone, Debug, Default, PartialEq)]
struct Layout {
    origin: Point<Pixels>,
    cell: Size<Pixels>,
    columns: usize,
    lines: usize,
}

impl Dimensions for Layout {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

impl Layout {
    fn window_size(&self) -> WindowSize {
        WindowSize {
            num_lines: self.lines as u16,
            num_cols: self.columns as u16,
            cell_width: f32::from(self.cell.width) as u16,
            cell_height: f32::from(self.cell.height) as u16,
        }
    }
}

/// Hands the emulator's events to the view, from the reader's thread.
#[derive(Clone)]
struct Listener(mpsc::Sender<Event>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        self.0.send(event).ok();
    }
}

/// What a group's agent is doing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AgentState {
    Working,
    /// Done with what it was asked, or not asked yet.
    Waiting,
    /// The shell is gone.
    Ended,
}

/// A shell started for a [`Terminal`], before the view takes it.
pub struct Started {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    receiver: Receiver<Event>,
    layout: Layout,
}

/// Starts `command` in a login shell in `dir`. When the command ends the
/// shell stays, so the terminal isn't left dead.
pub fn start(dir: &Path, command: &str) -> Result<Started, String> {
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let mut env = std::collections::HashMap::from([
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ]);
        // An app opened from the Finder gets no locale, and the agent's
        // accents would come out garbled.
        if std::env::var("LANG").is_err() {
            env.insert("LANG".to_string(), "en_US.UTF-8".to_string());
        }
        let options = tty::Options {
            shell: Some(Shell::new(
                shell.clone(),
                vec![
                    "-l".to_string(),
                    "-i".to_string(),
                    "-c".to_string(),
                    format!("{}{command}; exec {shell} -l", unset_claude()),
                ],
            )),
            working_directory: Some(dir.to_path_buf()),
            drain_on_exit: true,
            env,
        };
        let layout = Layout {
            origin: Point::default(),
            cell: size(px(8.), px(LINE_HEIGHT)),
            columns: 80,
            lines: 24,
        };
        let pty = tty::new(&options, layout.window_size(), 0)
            .map_err(|error| format!("No se pudo abrir la terminal: {error}"))?;
        let (events, receiver) = mpsc::channel();
        let listener = Listener(events);
        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &layout,
            listener.clone(),
        )));
        let event_loop = EventLoop::new(term.clone(), listener, pty, true, false)
            .map_err(|error| format!("No se pudo abrir la terminal: {error}"))?;
        let sender = event_loop.channel();
        event_loop.spawn();
        Ok(Started {
            term,
            sender,
            receiver,
            layout,
        })
    }
}

/// Drops the variables a Claude Code session leaves to its children, so an
/// app started from one doesn't hand its agent a session that isn't its own.
fn unset_claude() -> String {
    let inherited: Vec<String> = std::env::vars()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("CLAUDE"))
        .collect();
    if inherited.is_empty() {
        String::new()
    } else {
        format!("unset {}; ", inherited.join(" "))
    }
}

impl Terminal {
    pub fn new(started: Started, cx: &mut Context<Self>) -> Self {
        let Started {
            term,
            sender,
            receiver,
            layout,
        } = started;
        Self::pump(receiver, cx);
        Self {
            term,
            sender,
            layout: Rc::new(std::cell::Cell::new(layout)),
            title: None,
            exited: false,
            selection: Rc::new(RefCell::new(None)),
            selecting: false,
            over_link: false,
            reported: None,
            last_motion: None,
            scroll_rest: 0.,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Takes the emulator's events as they come, a burst at a time.
    fn pump(receiver: Receiver<Event>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut receiver = receiver;
            loop {
                let (back, events) = cx
                    .background_executor()
                    .spawn(async move {
                        let first = receiver.recv().ok();
                        let mut events: Vec<_> = first.into_iter().collect();
                        events.extend(receiver.try_iter());
                        (receiver, events)
                    })
                    .await;
                receiver = back;
                if events.is_empty() {
                    break;
                }
                // No pause between bursts: the window already draws once per
                // frame however many arrive, and a pause delays the echo.
                let alive = this.update(cx, |this, cx| this.handle(events, cx)).is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn handle(&mut self, events: Vec<Event>, cx: &mut Context<Self>) {
        for event in events {
            match event {
                Event::Title(title) => self.title = Some(title),
                Event::ResetTitle => self.title = None,
                Event::PtyWrite(text) => self.write(text.into_bytes()),
                Event::ClipboardStore(_, text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text))
                }
                Event::ColorRequest(index, format) => {
                    let colors = *self.term.lock().colors();
                    self.write(format(color_at(index, &colors)).into_bytes());
                }
                Event::TextAreaSizeRequest(format) => {
                    self.write(format(self.layout.get().window_size()).into_bytes())
                }
                Event::ChildExit(_) | Event::Exit => self.exited = true,
                _ => {}
            }
        }
        cx.notify();
    }

    /// Sends `bytes` to the program as if typed.
    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.sender.send(Msg::Input(bytes.into())).ok();
    }

    /// Ends the terminal: its shell, and the program in it, get a hangup.
    pub fn close(&self) {
        self.sender.send(Msg::Shutdown).ok();
    }

    /// What the agent in the terminal is doing, from its screen: Claude Code
    /// offers "esc to interrupt" only while it works.
    pub fn state(&self) -> AgentState {
        if self.exited {
            AgentState::Ended
        } else if self
            .screen()
            .iter()
            .any(|line| line.contains("esc to interrupt"))
        {
            AgentState::Working
        } else {
            AgentState::Waiting
        }
    }

    /// The text on screen, one string per line.
    pub fn screen(&self) -> Vec<String> {
        let term = self.term.lock();
        let grid = term.grid();
        (0..grid.screen_lines() as i32)
            .map(|line| {
                let row = &grid[Line(line)];
                (0..grid.columns())
                    .map(|column| row[Column(column)].c)
                    .collect::<String>()
            })
            .collect()
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform {
            match keystroke.key.as_str() {
                "c" => self.copy(cx),
                "v" => self.paste(cx),
                "a" => self.select_all(cx),
                "left" => self.typed(b"\x01".to_vec(), cx),
                "right" => self.typed(b"\x05".to_vec(), cx),
                "backspace" => self.typed(b"\x15".to_vec(), cx),
                // The app's own shortcuts.
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        let mode = *self.term.lock().mode();
        if let Some(bytes) = key_bytes(keystroke, mode) {
            self.typed(bytes, cx);
            cx.stop_propagation();
        }
    }

    /// Typing goes back to the bottom and drops the selection, as terminals do.
    fn typed(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if self.exited {
            return;
        }
        self.selection.replace(None);
        self.term.lock().scroll_display(Scroll::Bottom);
        self.write(bytes);
        cx.notify();
    }

    /// Copies the selection, lent to the emulator just to read its text.
    fn copy(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.borrow().clone() else {
            return;
        };
        let mut term = self.term.lock();
        term.selection = Some(selection);
        let text = term.selection_to_string();
        term.selection = None;
        drop(term);
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Pastes the clipboard's text, marked as a paste when the program asked
    /// for that. Without text, an image say, it sends ctrl-v, which Claude
    /// Code answers by reading the clipboard itself.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|item| item.text());
        let mode = *self.term.lock().mode();
        let bytes = match text {
            Some(text) => paste_bytes(&text, mode.contains(TermMode::BRACKETED_PASTE)),
            None => b"\x16".to_vec(),
        };
        self.typed(bytes, cx);
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let term = self.term.lock();
        let start = GridPoint::new(term.topmost_line(), Column(0));
        let end = GridPoint::new(term.bottommost_line(), term.last_column());
        drop(term);
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        self.selection.replace(Some(selection));
        cx.notify();
    }

    /// The cell under `position`, and which half of it.
    fn cell_at(&self, position: Point<Pixels>) -> (GridPoint, Side) {
        let layout = self.layout.get();
        let offset = self.term.lock().grid().display_offset() as i32;
        let x = f32::from(position.x - layout.origin.x).max(0.);
        let y = f32::from(position.y - layout.origin.y).max(0.);
        let width = f32::from(layout.cell.width).max(1.);
        let column = ((x / width) as usize).min(layout.columns.saturating_sub(1));
        let line =
            ((y / f32::from(layout.cell.height)) as usize).min(layout.lines.saturating_sub(1));
        let side = if x % width > width / 2. {
            Side::Right
        } else {
            Side::Left
        };
        (
            GridPoint::new(Line(line as i32 - offset), Column(column)),
            side,
        )
    }

    /// The cell under `position` on screen, column and row from the top
    /// left, as a program that asked for the mouse counts them.
    fn screen_cell(&self, position: Point<Pixels>) -> (usize, usize) {
        let (point, _) = self.cell_at(position);
        let offset = self.term.lock().grid().display_offset() as i32;
        (point.column.0, (point.line.0 + offset).max(0) as usize)
    }

    /// Whether the program asked for the mouse and this click is its: shift
    /// or option keep it for selecting, as macOS terminals do.
    fn mouse_for_program(&self, modifiers: &gpui::Modifiers) -> Option<TermMode> {
        let mode = *self.term.lock().mode();
        (mode.intersects(TermMode::MOUSE_MODE) && !modifiers.shift && !modifiers.alt)
            .then_some(mode)
    }

    fn report(&self, report: MouseReport, mode: TermMode) {
        self.write(mouse_bytes(report, mode.contains(TermMode::SGR_MOUSE)));
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        let (point, side) = self.cell_at(event.position);
        if event.modifiers.platform {
            if let Some(url) = self.link_at(point) {
                cx.open_url(&url);
            }
            return;
        }
        if let Some(mode) = self.mouse_for_program(&event.modifiers) {
            let cell = self.screen_cell(event.position);
            self.report(
                MouseReport {
                    button: 0,
                    cell,
                    pressed: true,
                    motion: false,
                    modifiers: event.modifiers,
                },
                mode,
            );
            self.reported = Some(cell);
            self.last_motion = Some(cell);
            self.selection.replace(None);
            cx.notify();
            return;
        }
        let kind = match event.click_count {
            2 => SelectionType::Semantic,
            3.. => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.selection
            .replace(Some(Selection::new(kind, point, side)));
        self.selecting = true;
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let (point, side) = self.cell_at(event.position);
        let over_link = event.modifiers.platform && self.link_at(point).is_some();
        if over_link != self.over_link {
            self.over_link = over_link;
            cx.notify();
        }
        let mode = *self.term.lock().mode();
        let dragging = self.reported.is_some() && event.pressed_button == Some(MouseButton::Left);
        let wanted = (dragging && mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION))
            || (event.pressed_button.is_none() && mode.contains(TermMode::MOUSE_MOTION));
        if wanted {
            let cell = self.screen_cell(event.position);
            if self.last_motion != Some(cell) {
                self.last_motion = Some(cell);
                self.report(
                    MouseReport {
                        // With no button down, a move reports button 3.
                        button: if dragging { 0 } else { 3 },
                        cell,
                        pressed: true,
                        motion: true,
                        modifiers: event.modifiers,
                    },
                    mode,
                );
            }
            return;
        }
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        if let Some(selection) = self.selection.borrow_mut().as_mut() {
            selection.update(point, side);
        }
        cx.notify();
    }

    fn mouse_up(
        &mut self,
        position: Point<Pixels>,
        modifiers: gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        if self.reported.take().is_some() {
            let mode = *self.term.lock().mode();
            if mode.intersects(TermMode::MOUSE_MODE) {
                self.report(
                    MouseReport {
                        button: 0,
                        cell: self.screen_cell(position),
                        pressed: false,
                        motion: false,
                        modifiers,
                    },
                    mode,
                );
            }
            return;
        }
        self.selecting = false;
        let mut selection = self.selection.borrow_mut();
        if selection.as_ref().is_some_and(Selection::is_empty) {
            *selection = None;
        }
        drop(selection);
        cx.notify();
    }

    /// Scrolls the history; a full-screen program gets arrow keys instead,
    /// as macOS terminals do.
    fn scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let line = f32::from(self.layout.get().cell.height);
        let whole = match event.delta {
            // A trackpad's pixels add up until they make a line.
            ScrollDelta::Pixels(delta) => {
                let lines = f32::from(delta.y) / line + self.scroll_rest;
                let whole = lines.trunc();
                self.scroll_rest = lines - whole;
                whole
            }
            // Each notch of a mouse wheel moves at least a line, however
            // slowly it turns.
            ScrollDelta::Lines(delta) => wheel_lines(delta.y),
        };
        if whole == 0. {
            return;
        }
        if let Some(mode) = self.mouse_for_program(&event.modifiers) {
            let cell = self.screen_cell(event.position);
            for _ in 0..whole.abs() as usize {
                self.report(
                    MouseReport {
                        button: if whole > 0. { 64 } else { 65 },
                        cell,
                        pressed: true,
                        motion: false,
                        modifiers: event.modifiers,
                    },
                    mode,
                );
            }
            return;
        }
        let mut term = self.term.lock();
        let mode = *term.mode();
        if mode.contains(TermMode::ALT_SCREEN) {
            drop(term);
            let key = if whole > 0. { b"\x1b[A" } else { b"\x1b[B" };
            for _ in 0..whole.abs() as usize {
                self.write(key.to_vec());
            }
        } else {
            term.scroll_display(Scroll::Delta(whole as i32));
        }
        cx.notify();
    }

    /// The link at `point`: the one the program attached to the cell, or a
    /// URL written there, followed onto the next lines when it was cut at
    /// the right edge.
    fn link_at(&self, point: GridPoint) -> Option<String> {
        let term = self.term.lock();
        let grid = term.grid();
        if let Some(link) = grid[point].hyperlink() {
            return Some(link.uri().to_string());
        }
        let first = (point.line.0 - LINK_REACH).max(term.topmost_line().0);
        let last = (point.line.0 + LINK_REACH).min(term.bottommost_line().0);
        let rows: Vec<Vec<char>> = (first..=last)
            .map(|line| {
                let row = &grid[Line(line)];
                (0..grid.columns())
                    .map(|column| row[Column(column)].c)
                    .collect()
            })
            .collect();
        joined_url(&rows, (point.line.0 - first) as usize, point.column.0)
    }

    /// Lays the grid out in `bounds`, resizing the terminal when it no longer
    /// fits, and collects what to draw.
    fn frame(
        term: &FairMutex<Term<Listener>>,
        sender: &EventLoopSender,
        layout: &std::cell::Cell<Layout>,
        selection: &RefCell<Option<Selection>>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
    ) -> Frame {
        let font_id = window.text_system().resolve_font(&font(FONT));
        let width = window
            .text_system()
            .advance(font_id, px(FONT_SIZE), 'm')
            .map(|advance| advance.width)
            .unwrap_or(px(8.));
        let cell = size(width, px(LINE_HEIGHT));
        let next = Layout {
            origin: bounds.origin,
            cell,
            columns: ((f32::from(bounds.size.width) / f32::from(width)) as usize).max(2),
            lines: ((f32::from(bounds.size.height) / LINE_HEIGHT) as usize).max(1),
        };
        let last = layout.get();
        let mut term = term.lock();
        if (next.columns, next.lines) != (last.columns, last.lines) {
            term.resize(next);
            sender.send(Msg::Resize(next.window_size())).ok();
        }
        layout.set(next);

        let selected = selection
            .borrow()
            .as_ref()
            .and_then(|selection| selection.to_range(&term));
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let mut rows: Vec<Vec<(char, Style)>> = vec![Vec::with_capacity(next.columns); next.lines];
        for indexed in content.display_iter {
            let Some(row) = rows.get_mut((indexed.point.line.0 + offset) as usize) else {
                continue;
            };
            let selected = selected.is_some_and(|range| range.contains(indexed.point));
            row.push(cell_glyph(indexed.cell, content.colors, selected));
        }
        let cursor = (content.cursor.shape != CursorShape::Hidden).then(|| {
            point(
                cell.width * content.cursor.point.column.0 as f32,
                cell.height * (content.cursor.point.line.0 + offset) as f32,
            )
        });
        Frame {
            rows: rows.into_iter().map(|row| text_row(&row)).collect(),
            cursor,
            cell,
        }
    }
}

/// What a paint draws: each line's text with its runs, and the cursor.
struct Frame {
    rows: Vec<(String, Vec<TextRun>)>,
    cursor: Option<Point<Pixels>>,
    cell: Size<Pixels>,
}

#[derive(Copy, Clone, PartialEq)]
struct Style {
    color: Hsla,
    background: Option<Hsla>,
    bold: bool,
    italic: bool,
    underline: bool,
}

fn cell_glyph(cell: &Cell, colors: &Colors, selected: bool) -> (char, Style) {
    let flags = cell.flags;
    let mut color = rgb_of(cell.fg, colors);
    let mut background = match cell.bg {
        Color::Named(NamedColor::Background) => None,
        bg => Some(rgb_of(bg, colors)),
    };
    if flags.contains(Flags::INVERSE) {
        let behind = background.unwrap_or(hex(BACKGROUND));
        background = Some(color);
        color = behind;
    }
    let mut color = hsla(color);
    if flags.contains(Flags::DIM) {
        color.a = 0.6;
    }
    let glyph = if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::HIDDEN) {
        ' '
    } else {
        cell.c
    };
    let style = Style {
        color,
        background: if selected {
            Some(hsla(hex(theme::SURFACE_ACTIVE)))
        } else {
            background.map(hsla)
        },
        bold: flags.contains(Flags::BOLD),
        italic: flags.contains(Flags::ITALIC),
        underline: flags.intersects(Flags::ALL_UNDERLINES),
    };
    (glyph, style)
}

/// A line's text and its runs, one run per stretch of the same style.
fn text_row(cells: &[(char, Style)]) -> (String, Vec<TextRun>) {
    let mut text = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut last: Option<Style> = None;
    for (glyph, style) in cells {
        let len = glyph.len_utf8();
        text.push(*glyph);
        match runs.last_mut() {
            Some(run) if last == Some(*style) => run.len += len,
            _ => runs.push(text_run(len, style)),
        }
        last = Some(*style);
    }
    (text, runs)
}

fn text_run(len: usize, style: &Style) -> TextRun {
    TextRun {
        len,
        font: Font {
            weight: if style.bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            style: if style.italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            },
            ..font(FONT)
        },
        color: style.color,
        background_color: style.background,
        underline: style.underline.then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(style.color),
            wavy: false,
        }),
        strikethrough: None,
    }
}

fn hex(color: u32) -> Rgb {
    Rgb {
        r: (color >> 16) as u8,
        g: (color >> 8) as u8,
        b: color as u8,
    }
}

fn hsla(color: Rgb) -> Hsla {
    rgb(((color.r as u32) << 16) | ((color.g as u32) << 8) | color.b as u32).into()
}

fn rgb_of(color: Color, colors: &Colors) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb,
        Color::Named(named) => color_at(named as usize, colors),
        Color::Indexed(index) => color_at(index as usize, colors),
    }
}

/// Color `index` of the terminal's table: what the program set, or else the
/// app's palette and xterm's 256 colors.
fn color_at(index: usize, colors: &Colors) -> Rgb {
    colors[index].unwrap_or_else(|| default_color(index))
}

fn default_color(index: usize) -> Rgb {
    const FOREGROUND: usize = NamedColor::Foreground as usize;
    const BACKGROUND_INDEX: usize = NamedColor::Background as usize;
    const CURSOR: usize = NamedColor::Cursor as usize;
    const DIM_BLACK: usize = NamedColor::DimBlack as usize;
    const DIM_WHITE: usize = NamedColor::DimWhite as usize;
    const DIM_FOREGROUND: usize = NamedColor::DimForeground as usize;
    match index {
        0..16 => hex(ANSI[index]),
        16..232 => {
            let cube = index - 16;
            let level = |step: usize| if step == 0 { 0 } else { (55 + step * 40) as u8 };
            Rgb {
                r: level(cube / 36),
                g: level(cube / 6 % 6),
                b: level(cube % 6),
            }
        }
        232..256 => {
            let gray = (8 + (index - 232) * 10) as u8;
            Rgb {
                r: gray,
                g: gray,
                b: gray,
            }
        }
        BACKGROUND_INDEX => hex(BACKGROUND),
        DIM_BLACK..=DIM_WHITE => hex(ANSI[index - DIM_BLACK]),
        DIM_FOREGROUND => hex(theme::SUBTEXT),
        FOREGROUND | CURSOR => hex(theme::TEXT),
        _ => hex(theme::TEXT),
    }
}

/// What a key sends to the program, or `None` for a key the terminal leaves
/// to the app.
fn key_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let modifiers = &keystroke.modifiers;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let arrow = |letter: char| {
        let code = 1
            + u8::from(modifiers.shift)
            + 2 * u8::from(modifiers.alt)
            + 4 * u8::from(modifiers.control);
        if code > 1 {
            format!("\x1b[1;{code}{letter}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let bytes = match keystroke.key.as_str() {
        // Option-enter is Claude Code's new line; shift-enter does the same.
        "enter" if modifiers.shift || modifiers.alt => b"\x1b\r".to_vec(),
        "enter" => b"\r".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "tab" if modifiers.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "backspace" if modifiers.alt => b"\x1b\x7f".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "delete" => b"\x1b[3~".to_vec(),
        // Option-arrows move by word, as in macOS terminals.
        "left" if modifiers.alt => b"\x1bb".to_vec(),
        "right" if modifiers.alt => b"\x1bf".to_vec(),
        "up" => arrow('A'),
        "down" => arrow('B'),
        "right" => arrow('C'),
        "left" => arrow('D'),
        "home" => arrow('H'),
        "end" => arrow('F'),
        "pageup" => b"\x1b[5~".to_vec(),
        "pagedown" => b"\x1b[6~".to_vec(),
        "space" if modifiers.control => vec![0],
        "space" => b" ".to_vec(),
        key if modifiers.control => {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return None;
            };
            match c {
                'a'..='z' => vec![c as u8 - b'a' + 1],
                '[' => vec![0x1b],
                '\\' => vec![0x1c],
                ']' => vec![0x1d],
                _ => return None,
            }
        }
        key if modifiers.alt => {
            let mut bytes = b"\x1b".to_vec();
            let typed = if modifiers.shift {
                key.to_uppercase()
            } else {
                key.to_string()
            };
            bytes.extend(typed.into_bytes());
            bytes
        }
        _ => keystroke.key_char.clone()?.into_bytes(),
    };
    Some(bytes)
}

/// The lines a mouse wheel's notch scrolls: what macOS says, rounded, but
/// never less than one, which a slow turn would otherwise give.
fn wheel_lines(delta: f32) -> f32 {
    if delta == 0. {
        0.
    } else {
        delta.signum() * delta.abs().round().max(1.)
    }
}

/// A mouse event for a program that asked for the mouse.
#[derive(Copy, Clone)]
struct MouseReport {
    /// 0 the left button, 3 none, 64 and 65 the wheel up and down.
    button: u8,
    /// Column and row on screen, from 0.
    cell: (usize, usize),
    pressed: bool,
    motion: bool,
    modifiers: gpui::Modifiers,
}

/// How a mouse event is told to the program: as SGR (`ESC [ < b ; x ; y M`)
/// when it asked for that, else in the old single-byte form, which can't
/// reach past column 223.
fn mouse_bytes(report: MouseReport, sgr: bool) -> Vec<u8> {
    let modifiers = &report.modifiers;
    let mut button = report.button
        + 4 * u8::from(modifiers.shift)
        + 8 * u8::from(modifiers.alt)
        + 16 * u8::from(modifiers.control);
    if report.motion {
        button += 32;
    }
    let (column, row) = (report.cell.0 + 1, report.cell.1 + 1);
    if sgr {
        let end = if report.pressed { 'M' } else { 'm' };
        return format!("\x1b[<{button};{column};{row}{end}").into_bytes();
    }
    if !report.pressed {
        // The old form has no release per button, only "released".
        button = 3 + (button & !3);
    }
    let byte = |value: usize| (32 + value).min(255) as u8;
    vec![0x1b, b'[', b'M', 32 + button, byte(column), byte(row)]
}

/// Pasted text, its line breaks as the return key and, when the program
/// asked, marked as a paste so it isn't taken as typed.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes()
    } else {
        text.into_bytes()
    }
}

/// How many lines up and down a cut URL is followed.
const LINK_REACH: i32 = 8;

/// The URL at `column` of `rows[row]`. A row whose text reaches the right
/// edge runs on into the next one, its indentation dropped, since Claude Code
/// cuts a long URL into lines of its own.
fn joined_url(rows: &[Vec<char>], row: usize, column: usize) -> Option<String> {
    let full = |chars: &[char]| {
        chars
            .iter()
            .rposition(|c| !c.is_whitespace())
            .is_some_and(|end| end + 2 >= chars.len())
    };
    let mut first = row;
    while first > 0 && full(&rows[first - 1]) {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < rows.len() && full(&rows[last]) {
        last += 1;
    }
    let mut text = Vec::new();
    let mut at = None;
    for (index, chars) in rows.iter().enumerate().take(last + 1).skip(first) {
        let skip = if index == first {
            0
        } else {
            chars.iter().take_while(|c| c.is_whitespace()).count()
        };
        let end = if index == last {
            chars.len()
        } else {
            chars
                .iter()
                .rposition(|c| !c.is_whitespace())
                .map_or(skip, |end| end + 1)
        };
        if index == row && column >= skip && column < end {
            at = Some(text.len() + column - skip);
        }
        text.extend_from_slice(&chars[skip..end.max(skip)]);
    }
    url_at(&text, at?)
}

/// The URL in `text` that covers position `at`, without the punctuation
/// that tends to follow one in prose.
fn url_at(text: &[char], at: usize) -> Option<String> {
    let stops = |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | '│');
    let mut start = at.min(text.len().checked_sub(1)?);
    if stops(text[start]) {
        return None;
    }
    while start > 0 && !stops(text[start - 1]) {
        start -= 1;
    }
    let mut end = at;
    while end < text.len() && !stops(text[end]) {
        end += 1;
    }
    let word: String = text[start..end].iter().collect();
    let from = word.find("https://").or_else(|| word.find("http://"))?;
    let url = word[from..].trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '!', '?']);
    let url_start = start + word[..from].chars().count();
    (at >= url_start && at < url_start + url.chars().count()).then(|| url.to_string())
}

impl Focusable for Terminal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Terminal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let term = self.term.clone();
        let sender = self.sender.clone();
        let layout = self.layout.clone();
        let selection = self.selection.clone();
        let focused = self.focus_handle.is_focused(window);

        div()
            .id("terminal")
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.mouse_up(event.position, event.modifiers, cx)
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _window, cx| {
                    this.mouse_up(event.position, event.modifiers, cx)
                }),
            )
            .on_scroll_wheel(cx.listener(Self::scroll))
            .map(|this| {
                if self.over_link {
                    this.cursor_pointer()
                } else {
                    this.cursor_text()
                }
            })
            .size_full()
            .p_2()
            .bg(rgb(BACKGROUND))
            .child(
                canvas(
                    move |bounds, window, _cx| {
                        Self::frame(&term, &sender, &layout, &selection, bounds, window)
                    },
                    move |bounds, frame, window, cx| {
                        let line_height = frame.cell.height;
                        for (index, (text, runs)) in frame.rows.into_iter().enumerate() {
                            let origin = point(
                                bounds.origin.x,
                                bounds.origin.y + line_height * index as f32,
                            );
                            let line = window.text_system().shape_line(
                                text.into(),
                                px(FONT_SIZE),
                                &runs,
                                Some(frame.cell.width),
                            );
                            // Backgrounds (the selection, inverse text) are
                            // painted apart, under the glyphs.
                            line.paint_background(origin, line_height, window, cx).ok();
                            line.paint(origin, line_height, window, cx).ok();
                        }
                        if let Some(at) = frame.cursor {
                            let mut color: Hsla = rgb(theme::TEXT).into();
                            color.a = if focused { 0.5 } else { 0.2 };
                            window.paint_quad(fill(
                                Bounds::new(bounds.origin + at, frame.cell),
                                color,
                            ));
                        }
                    },
                )
                .size_full(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    fn keystroke(text: &str) -> Keystroke {
        Keystroke::parse(text).unwrap()
    }

    #[test]
    fn a_url_is_found_under_the_click_without_the_trailing_period() {
        let text = chars("ver https://github.com/Toku-IA/x/pull/7. listo");

        assert_eq!(
            url_at(&text, 10).as_deref(),
            Some("https://github.com/Toku-IA/x/pull/7")
        );
        assert_eq!(url_at(&text, 1), None);
        assert_eq!(url_at(&text, 42), None);
    }

    #[test]
    fn a_url_cut_at_the_right_edge_is_followed_onto_the_next_line() {
        let rows = [
            chars("  ver https://github.com/To"),
            chars("  ku-IA/x/pull/7 listo     "),
        ];

        assert_eq!(
            joined_url(&rows, 1, 4).as_deref(),
            Some("https://github.com/Toku-IA/x/pull/7")
        );
        assert_eq!(
            joined_url(&rows, 0, 10).as_deref(),
            Some("https://github.com/Toku-IA/x/pull/7")
        );
        assert_eq!(joined_url(&rows, 1, 20), None);
    }

    #[test]
    fn a_click_is_told_to_the_program_as_sgr_or_the_old_form() {
        let click = |pressed| MouseReport {
            button: 0,
            cell: (9, 4),
            pressed,
            motion: false,
            modifiers: gpui::Modifiers::default(),
        };

        assert_eq!(mouse_bytes(click(true), true), b"\x1b[<0;10;5M");
        assert_eq!(mouse_bytes(click(false), true), b"\x1b[<0;10;5m");
        assert_eq!(
            mouse_bytes(click(true), false),
            vec![0x1b, b'[', b'M', 32, 42, 37]
        );
        assert_eq!(
            mouse_bytes(click(false), false),
            vec![0x1b, b'[', b'M', 35, 42, 37]
        );
    }

    #[test]
    fn a_slow_wheel_notch_still_scrolls_a_line() {
        assert_eq!(wheel_lines(0.1), 1.);
        assert_eq!(wheel_lines(-0.3), -1.);
        assert_eq!(wheel_lines(3.4), 3.);
        assert_eq!(wheel_lines(0.), 0.);
    }

    #[test]
    fn a_paste_keeps_its_lines_and_is_marked_when_asked() {
        assert_eq!(paste_bytes("a\nb", false), b"a\rb");
        assert_eq!(paste_bytes("a\r\nb", true), b"\x1b[200~a\rb\x1b[201~");
    }

    #[test]
    fn keys_send_what_a_terminal_sends() {
        let mode = TermMode::empty();

        assert_eq!(key_bytes(&keystroke("enter"), mode), Some(b"\r".to_vec()));
        assert_eq!(
            key_bytes(&keystroke("shift-enter"), mode),
            Some(b"\x1b\r".to_vec())
        );
        assert_eq!(key_bytes(&keystroke("ctrl-c"), mode), Some(vec![3]));
        assert_eq!(key_bytes(&keystroke("up"), mode), Some(b"\x1b[A".to_vec()));
        assert_eq!(
            key_bytes(&keystroke("up"), TermMode::APP_CURSOR),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key_bytes(&keystroke("shift-tab"), mode),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            key_bytes(&keystroke("alt-left"), mode),
            Some(b"\x1bb".to_vec())
        );
    }

    #[test]
    fn the_256_colors_follow_xterm() {
        let colors = Colors::default();

        assert_eq!(color_at(16, &colors), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            color_at(231, &colors),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(
            color_at(244, &colors),
            Rgb {
                r: 128,
                g: 128,
                b: 128
            }
        );
    }
}
