// SPDX-FileCopyrightText: 2026 Mohamed Hammad & Spacecraft Software
// SPDX-License-Identifier: GPL-3.0-or-later
//! `--format explore`: a read-only terminal browser over a command's output
//! (CLI Standard, `references/tui-explore.md`).
//!
//! It presents the same envelope `--format json` prints and invents no
//! fields. The records are those `--fields` would filter: the objects in an
//! array `data`, or in the arrays inside a `data` object, tagged with a
//! `section` column naming the array. A plain object becomes one row per key.
//!
//! Rendering goes to the alternate screen on **stderr**, so stdout carries
//! only what the user chooses to take away: Enter on a record's detail view
//! prints that record as a JSON envelope, and quitting prints nothing. Exit
//! is 0 either way. When the TUI may not start (stdout is not a terminal, an
//! agent is detected, `TERM=dumb`, `CI`, accessible mode), `main` falls back
//! before anything here runs; see [`blocker`].
//!
//! The browser is a pure state machine ([`App::handle`]) drawn by
//! [`draw`]; only [`run`] touches the terminal. Tests drive the first two
//! without one.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::path::Path;

/// Why the TUI may not start, or `None` when it may. Checked by `main`
/// before raw mode is ever entered (Standard §5: never enter raw mode on a
/// non-TTY, in CI, or under an agent).
///
/// stderr is checked as well as stdout because stderr is where the browser
/// draws: with `2>log` it would write control sequences into the file while
/// waiting for keys nobody can see (Codex on #35).
pub fn blocker(
    env: impl Fn(&str) -> Option<String>,
    stdout_is_tty: bool,
    stderr_is_tty: bool,
    accessible: bool,
) -> Option<&'static str> {
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    if set("AI_AGENT") || set("AGENT") {
        Some("an agent environment is detected (AI_AGENT or AGENT is set)")
    } else if env("CI")
        .is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
    {
        Some("CI is set")
    } else if !stdout_is_tty {
        Some("stdout is not a terminal")
    } else if !stderr_is_tty {
        Some("stderr is not a terminal, and the browser draws there")
    } else if env("TERM").is_some_and(|t| t == "dumb") {
        Some("TERM is dumb")
    } else if accessible {
        Some("accessible mode is on; its output is linear text")
    } else {
        None
    }
}

// --- Palette ----------------------------------------------------------------
//
// Steelbore Modern tokens (Standard §11), values from the
// steelbore-color-palette bundle's `steelbore.toml`. Roles follow
// tui-explore.md §3. Under NO_COLOR every style below degrades to plain text
// with bold/reverse/underline carrying the emphasis instead.

const VOID_NAVY: Color = Color::Rgb(0x00, 0x00, 0x27);
const PLATINUM_MIST: Color = Color::Rgb(0xD9, 0xDE, 0xE5);
const PULSE_VIOLET: Color = Color::Rgb(0x8A, 0x6C, 0xFF);
const ACID_LIME: Color = Color::Rgb(0xB4, 0xFF, 0x00);
const PLASMA_MAGENTA: Color = Color::Rgb(0xE4, 0x45, 0xFF);
const MARS_RED: Color = Color::Rgb(0xFF, 0x3B, 0x3B);

#[derive(Debug, Clone, Copy)]
struct Theme {
    color: bool,
}

impl Theme {
    fn base(self) -> Style {
        if self.color {
            Style::new().fg(PLATINUM_MIST).bg(VOID_NAVY)
        } else {
            Style::new()
        }
    }
    fn chrome(self) -> Style {
        if self.color {
            Style::new().fg(PULSE_VIOLET).bg(VOID_NAVY)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        }
    }
    fn selected(self) -> Style {
        if self.color {
            Style::new()
                .fg(VOID_NAVY)
                .bg(ACID_LIME)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::REVERSED)
        }
    }
    fn focused_header(self) -> Style {
        if self.color {
            Style::new()
                .fg(ACID_LIME)
                .bg(VOID_NAVY)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        }
    }
    fn dim(self) -> Style {
        if self.color {
            Style::new()
                .fg(PLATINUM_MIST)
                .bg(VOID_NAVY)
                .add_modifier(Modifier::DIM)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }
    fn warn(self) -> Style {
        if self.color {
            Style::new().fg(PLASMA_MAGENTA).bg(VOID_NAVY)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        }
    }
    fn error(self) -> Style {
        if self.color {
            Style::new()
                .fg(MARS_RED)
                .bg(VOID_NAVY)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        }
    }
}

// --- Model ------------------------------------------------------------------

/// The records to browse, derived from a serialized `Response`.
fn records(envelope: &Value) -> Vec<Map<String, Value>> {
    match envelope.get("data") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Object(map) => map.clone(),
                other => single("value", other.clone()),
            })
            .collect(),
        // The same container rule `--fields` uses, so both agree on what a
        // record is: an empty `context` browses as zero records, not as its
        // own fields.
        Some(Value::Object(map)) if crate::output::fields::is_container(map) => {
            let mut rows = Vec::new();
            for (section, child) in map {
                if let Value::Array(items) = child {
                    for item in items.iter().filter_map(Value::as_object) {
                        let mut row = single(SECTION_KEY, Value::String(section.clone()));
                        row.extend(item.clone());
                        rows.push(row);
                    }
                }
            }
            rows
        }
        // A plain record: one row per key, so long values can be opened in
        // the detail view one at a time.
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| {
                let mut row = single("key", Value::String(k.clone()));
                row.insert("value".to_owned(), v.clone());
                row
            })
            .collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![single("value", other.clone())],
    }
}

/// Key of the browser's own `section` column on container rows. The unit
/// separator in front keeps it apart from a record field that is itself
/// called `section`: that one stays a column of its own and survives
/// selection, where a plain `"section"` key would have been overwritten and
/// then stripped (CodeRabbit on #35). Shown as `section`.
const SECTION_KEY: &str = "\u{1f}section";

/// A column key as shown on screen: the synthetic section key loses its
/// separator.
fn display_name(key: &str) -> &str {
    key.strip_prefix('\u{1f}').unwrap_or(key)
}

fn single(key: &str, value: Value) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(key.to_owned(), value);
    map
}

/// Column names: the union of record keys in first-seen order.
fn columns(rows: &[Map<String, Value>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for row in rows {
        for key in row.keys() {
            if !out.contains(key) {
                out.push(key.clone());
            }
        }
    }
    out
}

/// One-line text for a table cell: strings bare, line breaks shown as ⏎.
fn cell_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.replace("\r\n", " ⏎ ").replace('\n', " ⏎ "),
        Some(other) => other.to_string(),
    }
}

/// Orders two cells: numbers first, compared numerically; then other text,
/// compared as text; empty cells last (in ascending order).
///
/// Ranking by type first makes this a total order. Comparing numbers
/// numerically and everything else as text, pair by pair, is not one on a
/// mixed column: 9 < 10, "10" < "5" and "5" < "9" form a cycle, and
/// `sort_by` may panic on that. A plain record's `value` column mixes
/// numbers, strings and booleans routinely (CodeRabbit on #35).
fn compare(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    fn rank(value: Option<&Value>, text: &str) -> u8 {
        if text.is_empty() {
            2
        } else if value.is_some_and(Value::is_number) {
            0
        } else {
            1
        }
    }
    let (x, y) = (cell_text(a), cell_text(b));
    rank(a, &x).cmp(&rank(b, &y)).then_with(|| {
        match (a.and_then(Value::as_f64), b.and_then(Value::as_f64)) {
            // JSON numbers are never NaN, so partial_cmp always answers.
            (Some(p), Some(q)) if a.is_some_and(Value::is_number) => {
                p.partial_cmp(&q).unwrap_or(Ordering::Equal)
            }
            _ => x.cmp(&y),
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortDir {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Screen {
    Table,
    Detail { scroll: u16 },
    Filter { input: String },
    Export { input: String },
    Help,
}

/// What the event loop should do after a key.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Continue,
    /// Leave the TUI, printing nothing.
    Quit,
    /// Leave the TUI and print this envelope to stdout.
    Select(Value),
    /// Copy this text to the clipboard and stay; the caller reports the
    /// result through [`App::copied`].
    Copy(String),
}

/// Browser state. Everything the screen shows is derived from this.
#[derive(Debug)]
pub struct App {
    metadata: Value,
    rows: Vec<Map<String, Value>>,
    columns: Vec<String>,
    /// Indices into `rows`, after the filter and sort.
    view: Vec<usize>,
    selected: usize,
    focused_column: usize,
    sort: Option<(usize, SortDir)>,
    filter: String,
    screen: Screen,
    /// First key of a two-key Vim command (`g` for `gg`, `:` for `:q`).
    pending: Option<char>,
    status: Option<(String, bool)>,
    page: usize,
    /// Kept across frames so the scroll offset persists: a fresh state per
    /// frame made Up from deep in a long list scroll the content under a
    /// highlight pinned to the bottom row (CodeRabbit on #35).
    table: TableState,
}

impl App {
    pub fn new(envelope: &Value) -> Self {
        let rows = records(envelope);
        let columns = columns(&rows);
        let mut app = Self {
            metadata: envelope.get("metadata").cloned().unwrap_or(Value::Null),
            view: Vec::new(),
            rows,
            columns,
            selected: 0,
            focused_column: 0,
            sort: None,
            filter: String::new(),
            screen: Screen::Table,
            pending: None,
            status: None,
            page: 10,
            table: TableState::new(),
        };
        app.refresh();
        app
    }

    /// Recomputes `view` from the filter and sort, keeping the selection in
    /// range.
    fn refresh(&mut self) {
        let needle = self.filter.to_lowercase();
        let mut view: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                needle.is_empty()
                    || self.columns.iter().any(|c| {
                        cell_text(self.rows[i].get(c))
                            .to_lowercase()
                            .contains(&needle)
                    })
            })
            .collect();
        if let Some((col, dir)) = self.sort {
            let key = &self.columns[col];
            view.sort_by(|&a, &b| {
                let ord = compare(self.rows[a].get(key), self.rows[b].get(key));
                match dir {
                    SortDir::Ascending => ord,
                    SortDir::Descending => ord.reverse(),
                }
            });
        }
        self.view = view;
        self.selected = self.selected.min(self.view.len().saturating_sub(1));
    }

    fn current(&self) -> Option<&Map<String, Value>> {
        self.view.get(self.selected).map(|&i| &self.rows[i])
    }

    fn move_by(&mut self, delta: isize) {
        let last = self.view.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    /// The current view as a JSON envelope: the original metadata plus the
    /// visible rows, so an export or a pipe gets the same shape as `--json`.
    fn view_envelope(&self, rows: Vec<Value>) -> Value {
        serde_json::json!({ "metadata": self.metadata, "data": rows })
    }

    /// Applies one key press.
    pub fn handle(&mut self, key: KeyEvent) -> Outcome {
        if key.kind != KeyEventKind::Press {
            return Outcome::Continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Outcome::Quit;
        }
        self.status = None;
        match self.screen.clone() {
            Screen::Filter { input } => self.handle_input(key, input, true),
            Screen::Export { input } => self.handle_input(key, input, false),
            Screen::Help => {
                self.screen = Screen::Table;
                Outcome::Continue
            }
            Screen::Detail { scroll } => self.handle_detail(key, scroll),
            Screen::Table => self.handle_table(key, ctrl),
        }
    }

    fn handle_input(&mut self, key: KeyEvent, mut input: String, filter: bool) -> Outcome {
        match key.code {
            KeyCode::Esc => {
                if filter {
                    self.filter.clear();
                    self.refresh();
                }
                self.screen = Screen::Table;
            }
            KeyCode::Enter => {
                self.screen = Screen::Table;
                if filter {
                    self.filter = input;
                    self.selected = 0;
                    self.refresh();
                } else {
                    self.export(input.trim());
                }
            }
            KeyCode::Backspace => {
                input.pop();
                self.screen = if filter {
                    Screen::Filter { input }
                } else {
                    Screen::Export { input }
                };
            }
            KeyCode::Char(c) => {
                input.push(c);
                self.screen = if filter {
                    Screen::Filter { input }
                } else {
                    Screen::Export { input }
                };
            }
            _ => {}
        }
        Outcome::Continue
    }

    fn handle_detail(&mut self, key: KeyEvent, scroll: u16) -> Outcome {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left | KeyCode::Char('h') => {
                self.screen = Screen::Table;
            }
            KeyCode::Enter => {
                if let Some(row) = self.current() {
                    let mut record = row.clone();
                    // The section label is the browser's, not the data's.
                    record.remove(SECTION_KEY);
                    return Outcome::Select(serde_json::json!({
                        "metadata": self.metadata,
                        "data": Value::Object(record),
                    }));
                }
            }
            KeyCode::Char('y') => return self.copy_current(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.screen = Screen::Detail {
                    scroll: scroll.saturating_add(1),
                };
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.screen = Screen::Detail {
                    scroll: scroll.saturating_sub(1),
                };
            }
            KeyCode::PageDown => {
                self.screen = Screen::Detail {
                    scroll: scroll.saturating_add(10),
                };
            }
            KeyCode::PageUp => {
                self.screen = Screen::Detail {
                    scroll: scroll.saturating_sub(10),
                };
            }
            _ => {}
        }
        Outcome::Continue
    }

    fn handle_table(&mut self, key: KeyEvent, ctrl: bool) -> Outcome {
        let pending = self.pending.take();
        let page = isize::try_from(self.page).unwrap_or(10);
        match key.code {
            KeyCode::Char('q') if pending == Some(':') => return Outcome::Quit,
            KeyCode::Char('q') | KeyCode::Esc => {
                if key.code == KeyCode::Esc && !self.filter.is_empty() {
                    self.filter.clear();
                    self.refresh();
                } else {
                    return Outcome::Quit;
                }
            }
            KeyCode::Char(':') => self.pending = Some(':'),
            KeyCode::Char('g') if pending == Some('g') => self.selected = 0,
            KeyCode::Char('g') => self.pending = Some('g'),
            KeyCode::Home => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.view.len().saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('n') | KeyCode::F(3)
                if !key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.move_by(1);
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('N') | KeyCode::F(3) => {
                self.move_by(-1)
            }
            KeyCode::PageDown => self.move_by(page),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::Char('f') if ctrl => self.move_by(page),
            KeyCode::Char('b') if ctrl => self.move_by(-page),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab | KeyCode::Char('w') => {
                if !self.columns.is_empty() {
                    self.focused_column = (self.focused_column + 1) % self.columns.len();
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                if !self.columns.is_empty() {
                    self.focused_column =
                        (self.focused_column + self.columns.len() - 1) % self.columns.len();
                }
            }
            KeyCode::Char('s') if !self.columns.is_empty() => {
                let col = self.focused_column;
                self.sort = match self.sort {
                    Some((c, SortDir::Ascending)) if c == col => Some((col, SortDir::Descending)),
                    Some((c, SortDir::Descending)) if c == col => None,
                    _ => Some((col, SortDir::Ascending)),
                };
                self.refresh();
            }
            KeyCode::Char('/') => {
                self.screen = Screen::Filter {
                    input: self.filter.clone(),
                };
            }
            KeyCode::Char('e') => {
                self.screen = Screen::Export {
                    input: String::new(),
                }
            }
            KeyCode::Char('?') | KeyCode::F(1) => self.screen = Screen::Help,
            KeyCode::Char('y') => return self.copy_current(),
            KeyCode::Enter if self.current().is_some() => {
                self.screen = Screen::Detail { scroll: 0 };
            }
            _ => {}
        }
        Outcome::Continue
    }

    /// The highlighted record as pretty JSON, the form a person pastes
    /// somewhere, with the browser's own section key made plain as in an
    /// export.
    fn copy_current(&mut self) -> Outcome {
        match self.current() {
            Some(row) => Outcome::Copy(
                serde_json::to_string_pretty(&Value::Object(exported(row)))
                    .expect("a JSON value serializes"),
            ),
            None => {
                self.status = Some(("nothing to copy: no record is highlighted".to_owned(), true));
                Outcome::Continue
            }
        }
    }

    /// Reports a copy attempt in the status line. OSC 52 has no reply, so
    /// success means "sent"; the message says how to get the record another
    /// way when the terminal ignored it (skill clipboard rule 2).
    pub fn copied(&mut self, result: Result<usize, String>) {
        self.status = Some(match result {
            Ok(chars) => (
                format!(
                    "sent {chars} characters to the clipboard (OSC 52); \
                     if nothing arrived, press e to export instead"
                ),
                false,
            ),
            Err(reason) => (
                format!("not copied: {reason}; press e to export instead"),
                true,
            ),
        });
    }

    /// Writes the current view to `name`. Refuses an existing file rather
    /// than overwriting it, and says what happened in the status line.
    fn export(&mut self, name: &str) {
        if name.is_empty() {
            self.status = Some(("export cancelled: no file name".to_owned(), true));
            return;
        }
        let rows: Vec<Value> = self
            .view
            .iter()
            .map(|&i| Value::Object(exported(&self.rows[i])))
            .collect();
        let count = rows.len();
        let body = serde_json::to_string_pretty(&self.view_envelope(rows))
            .expect("a JSON value serializes");
        self.status = Some(match write_new(Path::new(name), &body) {
            Ok(()) => (format!("exported {count} records to {name}"), false),
            Err(e) => (format!("export failed: {e}"), true),
        });
    }
}

/// A row as it goes into an export file: the browser's internal section key
/// becomes a plain `section` field, unless the record has its own
/// `section`, which then wins and keeps its value. An export must never
/// contain the `\u{1f}` key (CodeRabbit on #35).
fn exported(row: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in row {
        if key == SECTION_KEY {
            if !row.contains_key("section") {
                out.insert("section".to_owned(), value.clone());
            }
        } else {
            out.insert(key.clone(), value.clone());
        }
    }
    out
}

fn write_new(path: &Path, body: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(body.as_bytes())?;
    file.write_all(b"\n")
}

// --- Rendering --------------------------------------------------------------

const HELP: &[(&str, &str)] = &[
    ("↑ ↓  k j", "move"),
    ("PgUp PgDn  Ctrl+B Ctrl+F", "page"),
    ("Home End  gg G", "first / last"),
    ("← →  h l  Tab w", "focus column"),
    ("s", "sort focused column: ascending, descending, off"),
    ("/", "filter (Enter applies, Esc clears)"),
    ("n N  F3 Shift+F3", "next / previous row"),
    ("Enter", "open record; Enter again prints it and exits"),
    ("y", "copy the record to the clipboard (OSC 52)"),
    ("e", "export the current view as JSON"),
    ("q  Esc  :q  Ctrl+C", "quit (prints nothing)"),
    ("?  F1", "this help"),
];

fn draw(frame: &mut Frame, app: &mut App, theme: Theme) {
    let [main, status] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());
    frame.render_widget(Block::new().style(theme.base()), frame.area());
    app.page = usize::from(main.height.saturating_sub(3)).max(1);

    match &app.screen {
        Screen::Detail { scroll } => draw_detail(frame, app, theme, main, *scroll),
        _ => draw_table(frame, app, theme, main),
    }
    draw_status(frame, app, theme, status);
    if app.screen == Screen::Help {
        draw_help(frame, theme, main);
    }
}

fn draw_table(frame: &mut Frame, app: &mut App, theme: Theme, area: Rect) {
    let header = Row::new(app.columns.iter().enumerate().map(|(i, name)| {
        let marker = match app.sort {
            Some((c, SortDir::Ascending)) if c == i => " ▲",
            Some((c, SortDir::Descending)) if c == i => " ▼",
            _ => "",
        };
        let style = if i == app.focused_column {
            theme.focused_header()
        } else {
            theme.chrome()
        };
        Cell::from(format!("{}{marker}", display_name(name))).style(style)
    }));
    let rows = app.view.iter().map(|&i| {
        Row::new(
            app.columns
                .iter()
                .map(|c| Cell::from(cell_text(app.rows[i].get(c)))),
        )
    });
    let widths = column_widths(app, area.width.saturating_sub(2));
    let title = format!(
        " engram explore · {} of {} records{} ",
        app.view.len(),
        app.rows.len(),
        if app.filter.is_empty() {
            String::new()
        } else {
            format!(" · filter: {}", app.filter)
        }
    );
    let table = Table::new(rows, widths)
        .header(header)
        .style(theme.base())
        .row_highlight_style(theme.selected())
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(theme.chrome())
                .title(title),
        );
    app.table.select(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut app.table);
}

/// Widest a column may be before it shares the leftover space instead.
/// Fits a UUID (36) with a little room; content columns are far wider.
const MAX_COLUMN: usize = 38;

/// Preferred minimum for a short column when the table can afford it:
/// enough to recognize `user`, a UUID's first block, or a date's year.
const MIN_COLUMN: usize = 4;

/// Each column gets its natural width (header or widest cell, among the rows
/// in view), capped at [`MAX_COLUMN`]. Columns past the cap, typically
/// `content`, share what is left, but never less than half the table.
///
/// The widths are computed here as exact lengths rather than left to
/// ratatui's constraint solver. With `Fill` for content, a UUID, two
/// timestamps and a few short columns filled a 110-column terminal on their
/// own and content got zero width; with `Min`, the solver met it by
/// squeezing `id`, `agent`, `scope` and `role` to nothing while both
/// timestamps kept their room. Both were found by driving a real terminal.
/// Here, when space is short, every short column shrinks in proportion and
/// keeps at least [`MIN_COLUMN`] cells when the budget permits. Otherwise
/// short columns shrink further so wide columns keep their reserved space.
/// The detail view shows values whole.
fn column_widths(app: &App, area_width: u16) -> Vec<Constraint> {
    let n = app.columns.len();
    if n == 0 {
        return Vec::new();
    }
    let natural: Vec<usize> = app.columns.iter().map(|c| natural_width(app, c)).collect();
    let wide: Vec<bool> = natural.iter().map(|&w| w > MAX_COLUMN).collect();
    let wide_count = wide.iter().filter(|&&w| w).count();
    // Ratatui puts one cell between adjacent columns.
    let available = usize::from(area_width).saturating_sub(n - 1);
    let wide_floor = if wide_count == 0 { 0 } else { available / 2 };
    let short_budget = available.saturating_sub(wide_floor);
    let short_total: usize = natural
        .iter()
        .zip(&wide)
        .filter(|(_, w)| !**w)
        .map(|(n, _)| *n)
        .sum();
    let short_floor: usize = natural
        .iter()
        .zip(&wide)
        .filter(|(_, w)| !**w)
        .map(|(n, _)| (*n).min(MIN_COLUMN))
        .sum();
    let mut lengths: Vec<usize> = natural
        .iter()
        .zip(&wide)
        .map(|(&w, &is_wide)| {
            if is_wide {
                0
            } else if short_total <= short_budget {
                w
            } else if short_floor > short_budget {
                w * short_budget / short_total
            } else {
                let floor = MIN_COLUMN.min(w);
                // Reserve the minimums first, then divide only the extra
                // cells proportionally. Rounding cannot exceed the budget.
                floor + (w - floor) * (short_budget - short_floor) / (short_total - short_floor)
            }
        })
        .collect();
    let used: usize = lengths.iter().sum();
    let left = available.saturating_sub(used);
    if let Some(share) = left.checked_div(wide_count) {
        for (len, _) in lengths.iter_mut().zip(&wide).filter(|(_, w)| **w) {
            *len = share;
        }
    } else if let Some(last) = lengths.last_mut() {
        // No long column: the last one absorbs the slack.
        *last += left;
    }
    lengths
        .into_iter()
        .map(|w| Constraint::Length(u16::try_from(w).unwrap_or(u16::MAX)))
        .collect()
}

/// The header's width (plus room for a sort marker) or the widest cell in
/// view, whichever is larger.
fn natural_width(app: &App, column: &str) -> usize {
    app.view
        .iter()
        .map(|&i| cell_text(app.rows[i].get(column)).chars().count())
        .chain(std::iter::once(display_name(column).chars().count() + 2))
        .max()
        .unwrap_or(0)
}

fn draw_detail(frame: &mut Frame, app: &App, theme: Theme, area: Rect, scroll: u16) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(row) = app.current() {
        for (key, value) in row {
            lines.push(Line::from(Span::styled(
                display_name(key).to_owned(),
                theme.chrome(),
            )));
            let text = match value {
                Value::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).expect("a JSON value serializes"),
            };
            for line in text.lines() {
                lines.push(Line::from(format!("  {line}")));
            }
            lines.push(Line::default());
        }
    }
    let paragraph = Paragraph::new(lines)
        .style(theme.base())
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
        .block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(theme.chrome())
                .title(" record · Enter prints it and exits · Esc returns "),
        );
    frame.render_widget(paragraph, area);
}

fn draw_status(frame: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let line = match (&app.screen, &app.status) {
        (Screen::Filter { input }, _) => Line::from(vec![
            Span::styled("filter: ", theme.warn()),
            Span::styled(format!("{input}▏"), theme.base()),
        ]),
        (Screen::Export { input }, _) => Line::from(vec![
            Span::styled("export to file: ", theme.warn()),
            Span::styled(format!("{input}▏"), theme.base()),
        ]),
        (_, Some((message, true))) => {
            Line::from(Span::styled(format!("[ERROR] {message}"), theme.error()))
        }
        (_, Some((message, false))) => Line::from(Span::styled(
            format!("[OK] {message}"),
            theme.focused_header(),
        )),
        _ => Line::from(Span::styled(
            "? help · / filter · s sort · Enter open · y copy · e export · q quit",
            theme.dim(),
        )),
    };
    frame.render_widget(Paragraph::new(line).style(theme.base()), area);
}

fn draw_help(frame: &mut Frame, theme: Theme, area: Rect) {
    let width = 64.min(area.width);
    let height = u16::try_from(HELP.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(keys, action)| {
            Line::from(vec![
                Span::styled(format!("{keys:<26}"), theme.focused_header()),
                Span::styled(*action, theme.base()),
            ])
        })
        .collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).style(theme.base()).block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(theme.chrome())
                .title(" keys · any key closes "),
        ),
        popup,
    );
}

// --- Terminal ---------------------------------------------------------------
//
// Setup order: raw mode → keyboard probe → alternate screen → push kitty
// flags. Restore runs the reverse (pop flags → leave the alternate screen →
// leave raw mode) from three places that share one function: the `Restore`
// guard's `Drop`, the panic hook, and the signal watcher. `ACTIVE` makes
// the second and third runs no-ops (spacecraft-tui-protocols §2.3).

use crate::term_protocols::{self, Protocol};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::Duration;

/// True between setup and restore.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// True while kitty keyboard flags are pushed and must be popped.
static KEYBOARD_PUSHED: AtomicBool = AtomicBool::new(false);

/// `CSI > 1 u`: push "disambiguate escape codes" (flag 1 only; nothing in
/// the browser needs release events or associated text).
const KITTY_PUSH: &[u8] = b"\x1b[>1u";
/// `CSI < u`: pop the entry pushed above. Missing it leaves the user's
/// shell receiving `CSI … u` for every key.
const KITTY_POP: &[u8] = b"\x1b[<u";

fn restore_terminal() {
    if !ACTIVE.swap(false, AtomicOrdering::SeqCst) {
        return;
    }
    let mut err = std::io::stderr();
    if KEYBOARD_PUSHED.swap(false, AtomicOrdering::SeqCst) {
        let _ = err.write_all(KITTY_POP);
    }
    let _ = execute!(err, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

/// Restores the terminal when dropped, on every exit path including errors.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Default deadline for the keyboard probe. A terminal that answers DA1 at
/// all answers within a few milliseconds; the deadline only matters for
/// one that answers nothing.
const PROBE_DEADLINE: Duration = Duration::from_millis(200);

/// The probe deadline, overridable for slow links (ssh across the world)
/// with `ENGRAM_TERM_PROBE_MS`.
fn probe_deadline() -> Duration {
    std::env::var("ENGRAM_TERM_PROBE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(PROBE_DEADLINE, |ms| Duration::from_millis(ms.min(5_000)))
}

/// What a probe reply buffer has shown so far.
#[derive(Debug, Default, PartialEq, Eq)]
struct Replies {
    /// A `CSI ? <flags> u` reply arrived: the kitty keyboard protocol is
    /// spoken.
    kitty_keyboard: bool,
    /// The DA1 reply `CSI ? <params> c` arrived; terminals answer in order,
    /// so anything still unanswered is unsupported.
    da1: bool,
}

/// Scans `bytes` for `CSI ? <params> <final>` replies. Other bytes (a key
/// the user pressed during the probe, an unrelated reply) are skipped.
fn scan_replies(bytes: &[u8]) -> Replies {
    let mut found = Replies::default();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' && bytes[i + 2] == b'?' {
            let mut j = i + 3;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            match bytes.get(j) {
                Some(b'u') => found.kitty_keyboard = true,
                Some(b'c') => found.da1 = true,
                _ => {}
            }
            i = j;
        } else {
            i += 1;
        }
    }
    found
}

/// Asks the terminal whether it speaks the kitty keyboard protocol:
/// `CSI ? u` fenced by DA1 (`CSI c`), read from `/dev/tty` with a deadline,
/// then drained so no late reply leaks into the browser or the shell.
/// Must run in raw mode and before crossterm starts reading events.
///
/// Any failure means "no": a probe must never stop the browser starting.
#[cfg(unix)]
fn probe_kitty_keyboard(deadline: Duration) -> bool {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    use std::io::Read;
    use std::time::Instant;

    fn readable(tty: &std::fs::File, wait: Duration) -> bool {
        let timeout = Timespec {
            tv_sec: i64::try_from(wait.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(wait.subsec_nanos()),
        };
        let mut fds = [PollFd::new(tty, PollFlags::IN)];
        matches!(poll(&mut fds, Some(&timeout)), Ok(n) if n > 0)
    }

    let Ok(mut tty) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    else {
        return false;
    };
    if tty
        .write_all(b"\x1b[?u\x1b[c")
        .and_then(|()| tty.flush())
        .is_err()
    {
        return false;
    }
    let end = Instant::now() + deadline;
    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    let mut replies = Replies::default();
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        if !readable(&tty, left) {
            break;
        }
        match tty.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
        replies = scan_replies(&seen);
        if replies.da1 {
            break;
        }
    }
    // Drain: swallow whatever arrives late, so it is never read as keys.
    while readable(&tty, Duration::from_millis(20)) {
        if !matches!(tty.read(&mut buf), Ok(n) if n > 0) {
            break;
        }
    }
    replies.kitty_keyboard
}

#[cfg(not(unix))]
fn probe_kitty_keyboard(_deadline: Duration) -> bool {
    false
}

/// Whether to push kitty keyboard flags: the override wins, otherwise the
/// probe decides. Both sit behind the §2.1 gate, which `blocker` already
/// enforced for the TTY, agent and CI conditions; `TERM` unset is the one
/// left to check here.
fn want_kitty_keyboard() -> bool {
    if term_protocols::gate(|k| std::env::var(k).ok(), true).is_some() {
        return false;
    }
    match term_protocols::forced(Protocol::Keyboard) {
        Some(on) => on,
        None => probe_kitty_keyboard(probe_deadline()),
    }
}

/// Sends `text` to the clipboard with OSC 52 on stderr, the terminal the
/// browser draws on. Returns the character count for the status line.
fn copy_to_clipboard(text: &str) -> Result<usize, String> {
    if term_protocols::forced(Protocol::Clipboard) == Some(false) {
        return Err("the clipboard protocol is turned off (--term-protocols)".to_owned());
    }
    if term_protocols::gate(|k| std::env::var(k).ok(), true).is_some() {
        return Err("TERM is unset, so no terminal protocol is sent".to_owned());
    }
    let encoded = term_protocols::osc52_len(text);
    if encoded > term_protocols::OSC52_MAX_BASE64 {
        return Err(format!(
            "the record is {encoded} bytes encoded, over the {} terminals accept",
            term_protocols::OSC52_MAX_BASE64
        ));
    }
    let mut err = std::io::stderr();
    err.write_all(term_protocols::osc52(text).as_bytes())
        .and_then(|()| err.flush())
        .map_err(|e| format!("write failed: {e}"))?;
    Ok(text.chars().count())
}

/// Runs the browser on stderr's alternate screen until the user quits or
/// selects. Returns the envelope to print on stdout, if any.
///
/// # Errors
///
/// Any terminal I/O error; the terminal is restored before it is returned.
pub fn run(envelope: &Value, color: bool) -> std::io::Result<Option<Value>> {
    // A panic must not leave the terminal raw, on the alternate screen, or
    // with kitty flags pushed. Restore first so the message prints on a
    // sane terminal.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    terminal::enable_raw_mode()?;
    ACTIVE.store(true, AtomicOrdering::SeqCst);
    let _restore = Restore;
    // SIGTERM/SIGHUP (and a SIGINT sent with `kill`; in raw mode Ctrl+C is a
    // key) restore before the process ends.
    let _signals = term_protocols::SignalGuard::install(restore_terminal);
    let kitty = want_kitty_keyboard();
    execute!(std::io::stderr(), terminal::EnterAlternateScreen)?;
    if kitty {
        // Pushed after entering the alternate screen, whose flag stack is
        // separate from the main screen's.
        KEYBOARD_PUSHED.store(true, AtomicOrdering::SeqCst);
        let mut err = std::io::stderr();
        err.write_all(KITTY_PUSH)?;
        err.flush()?;
    }
    let mut term = Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stderr()))?;
    let theme = Theme { color };
    let mut app = App::new(envelope);
    loop {
        term.draw(|frame| draw(frame, &mut app, theme))?;
        if let Event::Key(key) = event::read()? {
            match app.handle(key) {
                Outcome::Continue => {}
                Outcome::Quit => return Ok(None),
                Outcome::Select(value) => return Ok(Some(value)),
                Outcome::Copy(text) => app.copied(copy_to_clipboard(&text)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn press(app: &mut App, code: KeyCode) -> Outcome {
        app.handle(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn envelope() -> Value {
        json!({
            "metadata": {"tool": "engram", "command": "engram memory recall"},
            "data": [
                {"id": "a", "content": "alpha decision\nsecond line", "n": 10},
                {"id": "b", "content": "beta note", "n": 2},
                {"id": "c", "content": "gamma decision", "n": 33},
            ],
        })
    }

    fn visible_ids(app: &App) -> Vec<String> {
        app.view
            .iter()
            .map(|&i| cell_text(app.rows[i].get("id")))
            .collect()
    }

    #[test]
    fn the_tui_never_starts_where_it_could_trap_or_corrupt() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert!(blocker(env(&[("AI_AGENT", "claude-code_2-1")]), true, true, false).is_some());
        assert!(blocker(env(&[("AGENT", "x")]), true, true, false).is_some());
        assert!(blocker(env(&[("CI", "true")]), true, true, false).is_some());
        assert!(
            blocker(env(&[]), false, true, false).is_some(),
            "stdout piped"
        );
        assert!(
            blocker(env(&[]), true, false, false).is_some(),
            "stderr redirected"
        );
        assert!(blocker(env(&[("TERM", "dumb")]), true, true, false).is_some());
        assert!(blocker(env(&[]), true, true, true).is_some(), "accessible");
        assert_eq!(
            blocker(env(&[("CI", "false"), ("AI_AGENT", "")]), true, true, false),
            None
        );
    }

    #[test]
    fn records_come_from_arrays_and_containers() {
        let app = App::new(&envelope());
        assert_eq!(app.columns, ["id", "content", "n"]);
        assert_eq!(visible_ids(&app), ["a", "b", "c"]);

        let container = App::new(&json!({"metadata": {}, "data": {
            "scope": "x", "rules": [{"rule_id": "r1"}], "memories": [{"id": "m1"}],
        }}));
        assert_eq!(container.columns, [SECTION_KEY, "rule_id", "id"]);
        assert_eq!(cell_text(container.rows[1].get(SECTION_KEY)), "memories");

        let plain = App::new(&json!({"metadata": {}, "data": {"id": "a", "content": "c"}}));
        assert_eq!(plain.columns, ["key", "value"]);
        assert_eq!(plain.rows.len(), 2);
    }

    #[test]
    fn filtering_matches_any_column_case_insensitively_and_esc_clears() {
        let mut app = App::new(&envelope());
        press(&mut app, KeyCode::Char('/'));
        typed(&mut app, "DECISION");
        press(&mut app, KeyCode::Enter);
        assert_eq!(visible_ids(&app), ["a", "c"]);
        press(&mut app, KeyCode::Esc);
        assert_eq!(
            visible_ids(&app),
            ["a", "b", "c"],
            "Esc clears the filter first"
        );
        assert_eq!(press(&mut app, KeyCode::Esc), Outcome::Quit, "then quits");
    }

    #[test]
    fn sorting_cycles_on_the_focused_column_and_compares_numbers_numerically() {
        let mut app = App::new(&envelope());
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Right); // focus "n"
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(
            visible_ids(&app),
            ["b", "a", "c"],
            "2 < 10 < 33, not text order"
        );
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(visible_ids(&app), ["c", "a", "b"]);
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(
            visible_ids(&app),
            ["a", "b", "c"],
            "third press turns sorting off"
        );
    }

    #[test]
    fn both_keybinding_sets_navigate() {
        let mut app = App::new(&envelope());
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.selected, 2);
        typed(&mut app, "gg");
        assert_eq!(app.selected, 0);
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.selected, 0);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected, 2, "movement stops at the last row");
        typed(&mut app, ":");
        assert_eq!(
            press(&mut app, KeyCode::Char('q')),
            Outcome::Quit,
            ":q quits"
        );
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(App::new(&envelope()).handle(ctrl_c), Outcome::Quit);
    }

    #[test]
    fn the_keyboard_probe_reads_kitty_and_da1_replies() {
        // kitty, Ghostty, WezTerm with enable_kitty_keyboard: both replies.
        assert_eq!(
            scan_replies(b"\x1b[?0u\x1b[?62;22c"),
            Replies {
                kitty_keyboard: true,
                da1: true
            }
        );
        // A terminal without the protocol answers DA1 only.
        assert_eq!(
            scan_replies(b"\x1b[?1;2c"),
            Replies {
                kitty_keyboard: false,
                da1: true
            }
        );
        // A key typed during the probe, and a reply split across reads,
        // before the fence arrives.
        assert_eq!(scan_replies(b"j\x1b[?1"), Replies::default());
        assert!(scan_replies(b"j\x1b[?1u\x1b[?6").kitty_keyboard);
        assert!(!scan_replies(b"\x1b[1u\x1b[c").kitty_keyboard);
    }

    #[test]
    fn y_copies_the_highlighted_record_and_reports_it() {
        let mut app = App::new(&envelope());
        press(&mut app, KeyCode::Down);
        let Outcome::Copy(text) = press(&mut app, KeyCode::Char('y')) else {
            panic!("y should ask to copy");
        };
        let copied: Value = serde_json::from_str(&text).unwrap();
        let expected = app.current().cloned().map(Value::Object).unwrap();
        assert_eq!(copied, expected);
        app.copied(Ok(42));
        assert!(matches!(&app.status, Some((m, false)) if m.contains("42 characters")));
        app.copied(Err("too big".to_owned()));
        assert!(matches!(&app.status, Some((m, true)) if m.contains("press e")));

        // From the detail view too, and nothing to copy in an empty view.
        press(&mut app, KeyCode::Enter);
        assert!(matches!(
            press(&mut app, KeyCode::Char('y')),
            Outcome::Copy(_)
        ));
        let mut empty = App::new(&json!({ "metadata": {}, "data": [] }));
        assert_eq!(press(&mut empty, KeyCode::Char('y')), Outcome::Continue);
        assert!(matches!(&empty.status, Some((_, true))));
    }

    #[test]
    fn enter_twice_selects_the_record_with_its_envelope() {
        let mut app = App::new(&envelope());
        press(&mut app, KeyCode::Down);
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Outcome::Continue,
            "first Enter opens"
        );
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Outcome::Select(json!({
                "metadata": {"tool": "engram", "command": "engram memory recall"},
                "data": {"id": "b", "content": "beta note", "n": 2},
            }))
        );
    }

    #[test]
    fn the_scroll_offset_persists_so_up_moves_the_highlight() {
        let rows: Vec<Value> = (0..50).map(|i| json!({"id": format!("r{i:02}")})).collect();
        let mut app = App::new(&json!({"metadata": {}, "data": rows}));
        let mut term = Terminal::new(TestBackend::new(40, 12)).expect("terminal");
        let mut draw_now = |app: &mut App| {
            term.draw(|f| draw(f, app, Theme { color: false }))
                .expect("draw");
        };
        press(&mut app, KeyCode::End);
        draw_now(&mut app);
        let offset = app.table.offset();
        assert!(offset > 0, "the last row scrolled into view");
        press(&mut app, KeyCode::Up);
        draw_now(&mut app);
        assert_eq!(
            app.table.offset(),
            offset,
            "Up moves the highlight, not the content"
        );
        assert_eq!(app.table.selected(), Some(48));
    }

    #[test]
    fn a_records_own_section_field_survives_selection() {
        let mut app = App::new(&json!({"metadata": {}, "data": {
            "memories": [{"id": "m1", "section": "chapter 2"}],
        }}));
        assert_eq!(
            app.columns,
            [SECTION_KEY, "id", "section"],
            "two distinct columns"
        );
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Outcome::Select(json!({
                "metadata": {},
                "data": {"id": "m1", "section": "chapter 2"},
            })),
            "only the browser's label is removed"
        );
    }

    #[test]
    fn sorting_a_mixed_column_is_total_and_does_not_panic() {
        let mut app = App::new(&json!({"metadata": {}, "data": {
            "a": 9, "b": 10, "c": "5", "d": "", "e": true, "f": null, "g": "apple",
        }}));
        press(&mut app, KeyCode::Char('l')); // focus "value"
        press(&mut app, KeyCode::Char('s'));
        let order: Vec<String> = app
            .view
            .iter()
            .map(|&i| cell_text(app.rows[i].get("value")))
            .collect();
        assert_eq!(order, ["9", "10", "5", "apple", "true", "", ""]);
    }

    #[test]
    fn a_container_export_names_the_section_plainly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("container.json");
        let mut app = App::new(&json!({"metadata": {}, "data": {
            "rules": [{"rule_id": "r1"}],
            "memories": [{"id": "m1", "section": "own"}],
        }}));
        press(&mut app, KeyCode::Char('e'));
        typed(&mut app, path.to_str().expect("utf-8 path"));
        press(&mut app, KeyCode::Enter);
        let text = std::fs::read_to_string(&path).expect("exported");
        assert!(
            !text.contains("\\u001f") && !text.contains('\u{1f}'),
            "{text}"
        );
        let written: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(
            written["data"],
            json!([{"section": "rules", "rule_id": "r1"}, {"id": "m1", "section": "own"}]),
            "the label becomes section; a record's own section wins"
        );
    }

    #[test]
    fn export_writes_the_filtered_view_and_never_overwrites() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("view.json");
        let name = path.to_str().expect("utf-8 path");
        let mut app = App::new(&envelope());
        press(&mut app, KeyCode::Char('/'));
        typed(&mut app, "beta");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('e'));
        typed(&mut app, name);
        press(&mut app, KeyCode::Enter);
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("exported")).expect("json");
        assert_eq!(
            written["data"],
            json!([{"id": "b", "content": "beta note", "n": 2}])
        );
        assert_eq!(written["metadata"]["tool"], "engram");
        assert!(matches!(&app.status, Some((m, false)) if m.contains("1 records")));

        press(&mut app, KeyCode::Char('e'));
        typed(&mut app, name);
        press(&mut app, KeyCode::Enter);
        assert!(
            matches!(&app.status, Some((m, true)) if m.contains("export failed")),
            "{:?}",
            app.status
        );
    }

    #[test]
    fn content_stays_visible_beside_ids_and_timestamps() {
        let mut app = App::new(&json!({"metadata": {}, "data": [{
            "id": "dc9e5b42-4324-5b0b-a0ee-44f634698566",
            "agent": "claude-code",
            "scope": "engram",
            "role": "assistant",
            "content": "the content column must never be squeezed out by the short columns beside it",
            "created_at": "2026-10-29T20:40:20.235Z",
            "valid_from": "2026-10-29T20:40:20.235Z",
        }]}));
        for width in [32, 40, 64, 80, 110] {
            let mut term = Terminal::new(TestBackend::new(width, 10)).expect("terminal");
            term.draw(|f| draw(f, &mut app, Theme { color: false }))
                .expect("draw");
            let screen: String = term
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(screen.contains("content"), "width {width}: {screen}");
            assert!(screen.contains("the content"), "width {width}: {screen}");
            if width == 110 {
                assert!(screen.contains("the content column must"), "{screen}");
                assert!(
                    screen.contains("dc9e"),
                    "the id keeps a recognisable start: {screen}"
                );
                assert!(
                    screen.contains("clau") && screen.contains("assi"),
                    "{screen}"
                );
            }
        }
    }

    #[test]
    fn the_screen_renders_rows_help_and_detail() {
        let mut app = App::new(&envelope());
        let mut term = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
        let render = |term: &mut Terminal<TestBackend>, app: &mut App| {
            term.draw(|f| draw(f, app, Theme { color: false }))
                .expect("draw");
            let buffer = term.backend().buffer().clone();
            buffer
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        let table = render(&mut term, &mut app);
        assert!(table.contains("3 of 3 records"), "{table}");
        assert!(table.contains("alpha decision ⏎ second line"), "{table}");

        press(&mut app, KeyCode::Char('?'));
        assert!(render(&mut term, &mut app).contains("focus column"));
        press(&mut app, KeyCode::Char('x')); // any key closes help
        press(&mut app, KeyCode::Enter);
        let detail = render(&mut term, &mut app);
        assert!(
            detail.contains("second line") && detail.contains("record"),
            "{detail}"
        );
    }
}
