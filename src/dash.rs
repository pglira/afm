//! Terminal dashboard that shows all jobs of the job store, in the Dracula color theme.

use crate::job::{self, list_jobs, log_path, remove_job, send_control, Job, Status};
use crate::util::{eta_clock, fmt_duration, human_bytes, short_bytes, tail_lines};
use anyhow::{bail, Result};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, HighlightSpacing, Padding, Paragraph, Row, Table, TableState,
};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RELOAD: Duration = Duration::from_millis(500);
const BAR_WIDTH: usize = 14;
/// Speed samples that are kept per job; one sample fills one column of the speed graph.
const SPEED_SAMPLES: usize = 600;
/// Width of the y axis of the speed graph: label, space and axis line.
const AXIS_WIDTH: u16 = 9;
/// Smallest height of the speed graph: statistics line, three plot rows and time axis.
const GRAPH_MIN_HEIGHT: u16 = 5;

/// Dracula palette (https://draculatheme.com/contribute).
mod theme {
    use ratatui::style::Color;
    pub const BG: Color = Color::Rgb(0x28, 0x2a, 0x36);
    pub const BG_DARK: Color = Color::Rgb(0x21, 0x22, 0x2c);
    pub const LINE: Color = Color::Rgb(0x44, 0x47, 0x5a);
    pub const FG: Color = Color::Rgb(0xf8, 0xf8, 0xf2);
    pub const COMMENT: Color = Color::Rgb(0x62, 0x72, 0xa4);
    pub const CYAN: Color = Color::Rgb(0x8b, 0xe9, 0xfd);
    pub const GREEN: Color = Color::Rgb(0x50, 0xfa, 0x7b);
    pub const ORANGE: Color = Color::Rgb(0xff, 0xb8, 0x6c);
    pub const PINK: Color = Color::Rgb(0xff, 0x79, 0xc6);
    pub const PURPLE: Color = Color::Rgb(0xbd, 0x93, 0xf9);
    pub const RED: Color = Color::Rgb(0xff, 0x55, 0x55);
    pub const YELLOW: Color = Color::Rgb(0xf1, 0xfa, 0x8c);
}
use theme::*;

/// Braille spinner frames; braille glyphs have the width of one cell in monospace fonts.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn run() -> Result<()> {
    // A clipboard tool that exits early must not kill the dashboard with SIGPIPE.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let result = App::new().run(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Copy text to the system clipboard with wl-copy, xclip or xsel; fall back to the OSC 52
/// terminal escape sequence. Returns the name of the method that worked.
fn copy_to_clipboard(text: &str) -> Result<&'static str> {
    let mut tools: Vec<(&'static str, &[&str])> = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        tools.push(("wl-copy", &[]));
    }
    if std::env::var_os("DISPLAY").is_some() {
        tools.push(("xclip", &["-selection", "clipboard"]));
        tools.push(("xsel", &["--clipboard", "--input"]));
    }
    for (cmd, args) in tools {
        let mut command = Command::new(cmd);
        command.args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        // Run the tool in its own session, so that the clipboard owner outlives the terminal.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let Ok(mut child) = command.spawn() else {
            continue;
        };
        let written = child.stdin.take().map(|mut s| s.write_all(text.as_bytes()).is_ok()).unwrap_or(false);
        if written && child.wait().map(|s| s.success()).unwrap_or(false) {
            return Ok(cmd);
        }
    }
    let mut out = std::io::stdout();
    if write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes())).and_then(|_| out.flush()).is_ok() {
        return Ok("OSC 52");
    }
    bail!("no clipboard tool found")
}

fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

enum Mode {
    Normal,
    ConfirmCancel(String),
    Log(LogView),
}

struct LogView {
    id: String,
    lines: Vec<String>,
    /// Lines above the bottom of the log.
    scroll: usize,
    height: usize,
}

struct App {
    jobs: Vec<Job>,
    table: TableState,
    selected: Option<String>,
    mode: Mode,
    message: Option<(String, Color, Instant)>,
    last_reload: Option<Instant>,
    /// Recent speed samples per active job, in bytes per second.
    speeds: HashMap<String, VecDeque<u64>>,
    started: Instant,
    /// Screen area of the job table, for mouse clicks.
    table_area: Rect,
}

fn status_color(s: Status) -> Color {
    match s {
        Status::Starting | Status::Running => CYAN,
        Status::Paused => ORANGE,
        Status::Done => GREEN,
        Status::Failed => RED,
        Status::Cancelled => COMMENT,
        Status::Lost => PINK,
    }
}

fn panel(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(LINE))
        .title(title)
        .padding(Padding::horizontal(1))
}

/// Replace the home directory with `~`.
fn tilde(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && (p == h || p.starts_with(&format!("{h}/"))) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

/// Fit `s` into `max` characters; replace the start with `…` when it is too long.
fn tail_fit(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max == 0 {
        return s.to_string();
    }
    let mut out = String::from("…");
    out.extend(s.chars().skip(n - max + 1));
    out
}

/// Thin progress bar with a half-cell resolution.
fn bar(percent: u8, color: Color) -> Line<'static> {
    let halves = (percent.min(100) as usize * BAR_WIDTH * 2 + 50) / 100;
    let full = halves / 2;
    let half = halves % 2 == 1;
    let rest = BAR_WIDTH - full - half as usize;
    let mut spans = vec![Span::styled("━".repeat(full), Style::new().fg(color))];
    if half {
        spans.push(Span::styled("╸", Style::new().fg(color)));
    }
    spans.push(Span::styled("━".repeat(rest), Style::new().fg(LINE)));
    spans.push(Span::styled(format!(" {percent:>3}%"), Style::new().fg(FG)));
    Line::from(spans)
}

fn key_hint(k: &str, desc: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!(" {k} "), Style::new().fg(BG).bg(PURPLE).bold()),
        Span::styled(format!(" {desc}  "), Style::new().fg(COMMENT)),
    ]
}

/// Colors of the speed graph, from the bottom row to the top row.
fn graph_gradient(s: Status) -> (Color, Color) {
    match s {
        Status::Starting | Status::Running => (PURPLE, CYAN),
        _ => (blend(status_color(s), BG, 0.6), status_color(s)),
    }
}

/// Mix two RGB colors; `t` = 0 gives `a`, `t` = 1 gives `b`.
fn blend(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
            Color::Rgb(mix(ar, br), mix(ag, bg), mix(ab, bb))
        }
        _ => a,
    }
}

/// Smallest value of the series 1, 2, 5, 10, 20, 50, … (per binary unit) that is not below `v`.
fn nice_ceil(v: u64) -> u64 {
    let mut unit = 1u64;
    while v / unit >= 1024 && unit < 1 << 50 {
        unit *= 1024;
    }
    [1, 2, 5, 10, 20, 50, 100, 200, 500, 1024]
        .iter()
        .map(|m| m * unit)
        .find(|&n| n >= v)
        .unwrap_or(v)
}

/// Area chart of the speed samples: statistics line, y axis with grid lines, filled columns
/// with a vertical color gradient, and a time axis. The newest sample is at the right edge.
fn draw_speed_graph(f: &mut Frame, area: Rect, samples: &[u64], status: Status) {
    if area.height < GRAPH_MIN_HEIGHT || area.width < AXIS_WIDTH + 20 {
        return;
    }
    let [stats, body, time] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(area);
    // An even number of plot rows puts the middle grid line at half of the axis maximum.
    let spare = if body.height >= 5 { body.height % 2 } else { 0 };
    let plot = Rect {
        x: body.x + AXIS_WIDTH,
        y: body.y + spare,
        width: body.width - AXIS_WIDTH,
        height: body.height - spare,
    };
    let samples = &samples[samples.len().saturating_sub(plot.width as usize)..];

    let comment = Style::new().fg(COMMENT);
    let peak = samples.iter().copied().max().unwrap_or(0);
    let avg = if samples.is_empty() { 0 } else { (samples.iter().map(|&v| v as u128).sum::<u128>() / samples.len() as u128) as u64 };
    let stat = |label: &str, v: u64, color: Color| {
        vec![
            Span::styled(format!("{label} "), comment),
            Span::styled(format!("{}/s", short_bytes(v)), Style::new().fg(color).bold()),
            Span::raw("    "),
        ]
    };
    let (low, high) = graph_gradient(status);
    let mut spans = vec![Span::styled(format!("{:<8}", "speed"), comment)];
    // A finished job has no current speed; its newest sample is the speed at its end.
    let (newest, right) = if status.is_final() { ("last", " end ") } else { ("now", " now ") };
    spans.extend(stat(newest, samples.last().copied().unwrap_or(0), high));
    spans.extend(stat("avg", avg, FG));
    spans.extend(stat("peak", peak, FG));
    f.render_widget(Paragraph::new(Line::from(spans)), stats);

    // Time axis: the plot width is the time window, one column per sample.
    let window = fmt_duration((plot.width as u128 * RELOAD.as_millis() / 1000) as u64);
    let left = format!(" {window} ago ");
    let fill = (plot.width as usize).saturating_sub(left.len() + right.len() + 1);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("╰─", Style::new().fg(LINE)),
            Span::styled(left, comment),
            Span::styled("─".repeat(fill), Style::new().fg(LINE)),
            Span::styled(right, comment),
        ])),
        Rect { x: plot.x - 1, width: plot.width + 1, ..time },
    );

    const LEVELS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let rows = plot.height as usize;
    let max = nice_ceil(peak.max(1));
    // Grid lines are at the top edge of the top row and, with samples, of the row below the middle.
    let grid_rows = [Some(rows - 1), (rows >= 4 && !samples.is_empty()).then(|| rows / 2 - 1)];
    let offset = plot.width as usize - samples.len();
    let buf = f.buffer_mut();
    for r in 0..rows {
        let y = plot.y + (rows - 1 - r) as u16;
        let is_grid = grid_rows.contains(&Some(r));
        let label = if is_grid && peak > 0 {
            format!("{}/s", short_bytes((max as u128 * (r as u128 + 1) / rows as u128) as u64))
        } else {
            String::new()
        };
        buf.set_string(body.x, y, format!("{label:>w$}", w = AXIS_WIDTH as usize - 2), comment);
        buf.set_string(plot.x - 1, y, if is_grid { "┤" } else { "│" }, Style::new().fg(LINE));
        let t = if rows > 1 { r as f64 / (rows - 1) as f64 } else { 1.0 };
        let color = blend(low, high, t);
        for col in 0..plot.width as usize {
            // Height of the column in eighths of a row; a sample above zero shows at least one eighth.
            let level = col.checked_sub(offset).map_or(0, |i| {
                let v = samples[i];
                ((v as u128 * rows as u128 * 8 + max as u128 / 2) / max as u128).max((v > 0) as u128) as usize
            });
            let fill = level.saturating_sub(r * 8).min(8);
            let x = plot.x + col as u16;
            if fill > 0 {
                buf.set_string(x, y, LEVELS[fill], Style::new().fg(color));
            } else if is_grid {
                buf.set_string(x, y, "▔", Style::new().fg(LINE));
            }
        }
    }
    if samples.is_empty() {
        let msg = Rect { y: plot.y + plot.height / 2, height: 1, ..plot };
        f.render_widget(Paragraph::new(Span::styled(" no samples ", comment)).alignment(Alignment::Center), msg);
    }
}

impl App {
    fn new() -> Self {
        Self {
            jobs: Vec::new(),
            table: TableState::default(),
            selected: None,
            mode: Mode::Normal,
            message: None,
            last_reload: None,
            speeds: HashMap::new(),
            started: Instant::now(),
            table_area: Rect::default(),
        }
    }

    fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            if self.last_reload.is_none_or(|t| t.elapsed() >= RELOAD) {
                self.reload();
            }
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        if self.on_key(k) {
                            return Ok(());
                        }
                    }
                    Event::Mouse(m) => self.on_mouse(m),
                    _ => {}
                }
            }
        }
    }

    fn reload(&mut self) {
        self.jobs = list_jobs();
        let idx = self
            .selected
            .as_ref()
            .and_then(|id| self.jobs.iter().position(|j| &j.id == id))
            .or(if self.jobs.is_empty() { None } else { Some(0) });
        self.table.select(idx);
        self.selected = idx.map(|i| self.jobs[i].id.clone());
        for j in &self.jobs {
            if !j.status.is_final() {
                let s = self.speeds.entry(j.id.clone()).or_default();
                s.push_back(j.speed as u64);
                if s.len() > SPEED_SAMPLES {
                    s.pop_front();
                }
            }
        }
        self.speeds.retain(|id, _| self.jobs.iter().any(|j| &j.id == id));
        if let Mode::Log(v) = &mut self.mode {
            v.lines = tail_lines(&log_path(&v.id), 5000);
        }
        self.last_reload = Some(Instant::now());
    }

    fn current(&self) -> Option<&Job> {
        self.table.selected().and_then(|i| self.jobs.get(i))
    }

    fn select(&mut self, delta: isize) {
        if self.jobs.is_empty() {
            return;
        }
        let i = self.table.selected().unwrap_or(0) as isize + delta;
        let i = i.clamp(0, self.jobs.len() as isize - 1) as usize;
        self.table.select(Some(i));
        self.selected = Some(self.jobs[i].id.clone());
    }

    fn flash(&mut self, text: impl Into<String>, color: Color) {
        self.message = Some((text.into(), color, Instant::now()));
    }

    /// Handle a key. Returns true to quit.
    fn on_key(&mut self, k: KeyEvent) -> bool {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return true;
        }
        match &mut self.mode {
            Mode::Log(v) => {
                let page = v.height.max(1);
                let max = v.lines.len().saturating_sub(page);
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => self.mode = Mode::Normal,
                    KeyCode::Up | KeyCode::Char('k') => v.scroll = (v.scroll + 1).min(max),
                    KeyCode::Down | KeyCode::Char('j') => v.scroll = v.scroll.saturating_sub(1),
                    KeyCode::PageUp => v.scroll = (v.scroll + page).min(max),
                    KeyCode::PageDown => v.scroll = v.scroll.saturating_sub(page),
                    KeyCode::Home | KeyCode::Char('g') => v.scroll = max,
                    KeyCode::End | KeyCode::Char('G') => v.scroll = 0,
                    _ => {}
                }
                false
            }
            Mode::ConfirmCancel(id) => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    let id = id.clone();
                    match send_control(&id, "cancel") {
                        Ok(()) => self.flash(format!("cancel sent to job {id}"), ORANGE),
                        Err(e) => self.flash(format!("error: {e}"), RED),
                    }
                }
                self.mode = Mode::Normal;
                false
            }
            Mode::Normal => {
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return true,
                    KeyCode::Up | KeyCode::Char('k') => self.select(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.select(1),
                    KeyCode::Home | KeyCode::Char('g') => self.select(isize::MIN / 2),
                    KeyCode::End | KeyCode::Char('G') => self.select(isize::MAX / 2),
                    KeyCode::Char('p') | KeyCode::Char(' ') => self.toggle_pause(),
                    KeyCode::Char('i') => match copy_to_clipboard(crate::INSTALL_PROMPT) {
                        Ok(how) => self.flash(format!("skill install prompt copied to the clipboard ({how})"), GREEN),
                        Err(e) => self.flash(format!("error: {e}"), RED),
                    },
                    KeyCode::Char('c') => {
                        if let Some(j) = self.current() {
                            if j.status.is_final() {
                                let text = format!("job {} is already {}", j.id, j.status.as_str());
                                self.flash(text, YELLOW);
                            } else {
                                self.mode = Mode::ConfirmCancel(j.id.clone());
                            }
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Enter => {
                        if let Some(j) = self.current() {
                            let id = j.id.clone();
                            let lines = tail_lines(&log_path(&id), 5000);
                            self.mode = Mode::Log(LogView { id, lines, scroll: 0, height: 0 });
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        if let Some(j) = self.current() {
                            let id = j.id.clone();
                            if j.status.is_final() {
                                match remove_job(&id) {
                                    Ok(()) => self.flash(format!("removed job {id}"), GREEN),
                                    Err(e) => self.flash(format!("error: {e}"), RED),
                                }
                                self.select(1);
                                self.last_reload = None;
                            } else {
                                self.flash("cancel the job before you remove it", YELLOW);
                            }
                        }
                    }
                    KeyCode::Char('x') => {
                        let n = job::clear_finished();
                        self.flash(format!("removed {n} finished job(s)"), GREEN);
                        self.last_reload = None;
                    }
                    _ => {}
                }
                false
            }
        }
    }

    /// Mouse: click a row to select it, use the wheel to move the selection or scroll the log.
    fn on_mouse(&mut self, m: MouseEvent) {
        match (&mut self.mode, m.kind) {
            (Mode::Log(v), MouseEventKind::ScrollUp) => {
                v.scroll = (v.scroll + 3).min(v.lines.len().saturating_sub(v.height.max(1)));
            }
            (Mode::Log(v), MouseEventKind::ScrollDown) => v.scroll = v.scroll.saturating_sub(3),
            (Mode::Normal, MouseEventKind::ScrollUp) => self.select(-1),
            (Mode::Normal, MouseEventKind::ScrollDown) => self.select(1),
            (Mode::Normal, MouseEventKind::Down(MouseButton::Left)) => {
                let a = self.table_area;
                // Rows start below the top border, the header line and the header margin.
                let first_row = a.y + 3;
                let inside = m.column > a.x && m.column + 1 < a.right() && m.row >= first_row && m.row + 1 < a.bottom();
                if inside {
                    let i = self.table.offset() + (m.row - first_row) as usize;
                    if i < self.jobs.len() {
                        self.table.select(Some(i));
                        self.selected = Some(self.jobs[i].id.clone());
                    }
                }
            }
            _ => {}
        }
    }

    fn toggle_pause(&mut self) {
        let Some(j) = self.current() else { return };
        let (id, status) = (j.id.clone(), j.status);
        let cmd = match status {
            Status::Running => "pause",
            Status::Paused => "resume",
            _ => {
                self.flash(format!("job {id} is {}", status.as_str()), YELLOW);
                return;
            }
        };
        match send_control(&id, cmd) {
            Ok(()) => self.flash(format!("{cmd} sent to job {id}"), CYAN),
            Err(e) => self.flash(format!("error: {e}"), RED),
        }
    }

    fn spinner(&self) -> &'static str {
        SPINNER[(self.started.elapsed().as_millis() / 100) as usize % SPINNER.len()]
    }

    fn draw(&mut self, f: &mut Frame) {
        f.render_widget(Block::new().style(Style::new().bg(BG).fg(FG)), f.area());
        let area = f.area().inner(Margin { horizontal: 1, vertical: 0 });
        let [header, table, details, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(if area.height >= 30 { 14 } else { 10 }),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(f, header);
        self.draw_table(f, table);
        self.draw_details(f, details);
        self.draw_footer(f, footer);
        match self.mode {
            Mode::Log(_) => self.draw_log(f, area),
            Mode::ConfirmCancel(_) => self.draw_confirm(f),
            Mode::Normal => {}
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let count = |s: &[Status]| self.jobs.iter().filter(|j| s.contains(&j.status)).count();
        let speed: f64 = self.jobs.iter().filter(|j| !j.status.is_final()).map(|j| j.speed).sum();
        let stat = |n: usize, label: &str, color: Color| {
            let style = if n > 0 { Style::new().fg(color) } else { Style::new().fg(COMMENT) };
            vec![
                Span::styled(n.to_string(), style.bold()),
                Span::styled(format!(" {label}   "), Style::new().fg(COMMENT)),
            ]
        };
        let mut left = vec![
            Span::styled(" afm ", Style::new().fg(BG).bg(PURPLE).bold()),
            Span::styled("  agent file manager     ", Style::new().fg(COMMENT)),
        ];
        left.extend(stat(count(&[Status::Running, Status::Starting]), "running", CYAN));
        left.extend(stat(count(&[Status::Paused]), "paused", ORANGE));
        left.extend(stat(count(&[Status::Done]), "done", GREEN));
        left.extend(stat(count(&[Status::Failed, Status::Lost]), "failed", RED));
        let right = Line::from(vec![
            Span::styled("total ", Style::new().fg(COMMENT)),
            Span::styled(format!("{}/s ", human_bytes(speed as u64)), Style::new().fg(FG).bold()),
        ])
        .alignment(Alignment::Right);
        let row = Rect { height: 1, ..area };
        f.render_widget(Paragraph::new(Line::from(left)), row);
        f.render_widget(Paragraph::new(right), row);
    }

    fn draw_table(&mut self, f: &mut Frame, area: Rect) {
        let header = Row::new(["STATUS", "ID", "OP", "PROGRESS", "SIZE", "SPEED", "ETA / END", "AGENT", "NAME"])
            .style(Style::new().fg(COMMENT).bold())
            .bottom_margin(1);
        let spinner = self.spinner();
        let rows: Vec<Row> = self
            .jobs
            .iter()
            .map(|j| {
                let color = status_color(j.status);
                let active = !j.status.is_final();
                let size = match j.total_estimate() {
                    Some(t) if j.status != Status::Done => format!("{} / {}", short_bytes(j.bytes), short_bytes(t)),
                    _ => short_bytes(j.bytes),
                };
                let speed = if active && j.speed > 0.0 { format!("{}/s", short_bytes(j.speed as u64)) } else { "-".into() };
                let eta = match (active, j.eta) {
                    (true, Some(e)) if j.status == Status::Running => Line::from(vec![
                        Span::styled(fmt_duration(e), Style::new().fg(YELLOW)),
                        Span::styled(format!("  {}", eta_clock(e)), Style::new().fg(COMMENT)),
                    ]),
                    (true, _) => Line::from(Span::styled("-", Style::new().fg(COMMENT))),
                    (false, _) => Line::from(Span::styled(
                        j.elapsed().map(fmt_duration).unwrap_or_default(),
                        Style::new().fg(COMMENT),
                    )),
                };
                let mark = if matches!(j.status, Status::Running | Status::Starting) { spinner } else { " " };
                let status = Line::from(vec![
                    Span::styled(format!("{mark} "), Style::new().fg(color)),
                    Span::styled(j.status.as_str(), Style::new().fg(color).bold()),
                ]);
                let op_color = if j.op == job::Op::Move { PINK } else { PURPLE };
                Row::new([
                    Cell::from(status),
                    Cell::from(Span::styled(j.id.clone(), Style::new().fg(COMMENT))),
                    Cell::from(Span::styled(j.op.as_str(), Style::new().fg(op_color))),
                    Cell::from(bar(j.percent, color)),
                    Cell::from(Span::styled(size, Style::new().fg(FG))),
                    Cell::from(Span::styled(speed, Style::new().fg(if active { CYAN } else { COMMENT }))),
                    Cell::from(eta),
                    Cell::from(Span::styled(j.agent.clone().unwrap_or_default(), Style::new().fg(ORANGE))),
                    Cell::from(Line::from(vec![
                        Span::styled(j.name.clone().unwrap_or_else(|| "-".into()), Style::new().fg(FG).bold()),
                        Span::styled(if j.dry_run { "  dry run" } else { "" }, Style::new().fg(COMMENT).italic()),
                    ])),
                ])
                .style(if active { Style::new() } else { Style::new().add_modifier(Modifier::DIM) })
            })
            .collect();
        let widths = [
            Constraint::Length(11),
            Constraint::Length(6),
            Constraint::Length(4),
            Constraint::Length(BAR_WIDTH as u16 + 5),
            Constraint::Length(13),
            Constraint::Length(10),
            Constraint::Length(17),
            Constraint::Length(10),
            Constraint::Min(20),
        ];
        let title = Line::from(vec![
            Span::styled(" Jobs ", Style::new().fg(PURPLE).bold()),
            Span::styled(format!("{} ", self.jobs.len()), Style::new().fg(COMMENT)),
        ]);
        let table = Table::new(rows, widths)
            .header(header)
            .block(panel(title))
            .row_highlight_style(Style::new().bg(LINE).remove_modifier(Modifier::DIM))
            .highlight_symbol(Text::from(Span::styled("▌", Style::new().fg(PINK))))
            .highlight_spacing(HighlightSpacing::Always)
            .column_spacing(2);
        self.table_area = area;
        f.render_stateful_widget(table, area, &mut self.table);
        if self.jobs.is_empty() {
            let inner = area.inner(Margin { horizontal: 3, vertical: 3 });
            let text = Text::from(vec![
                Line::from(Span::styled("No jobs yet", Style::new().fg(FG).bold())),
            ])
            .alignment(Alignment::Center);
            f.render_widget(Paragraph::new(text), inner);
        }
    }

    fn draw_details(&self, f: &mut Frame, area: Rect) {
        let Some(j) = self.current() else {
            f.render_widget(panel(Span::styled(" Job ", Style::new().fg(PURPLE).bold())), area);
            return;
        };
        let title = Line::from(vec![
            Span::styled(" Job ", Style::new().fg(PURPLE).bold()),
            Span::styled(format!("{} ", j.id), Style::new().fg(FG).bold()),
        ]);
        let block = panel(title);
        let inner = block.inner(area);
        f.render_widget(block, area);

        // The paths use the rows that the smallest graph does not need.
        let max_paths = inner.height.saturating_sub(GRAPH_MIN_HEIGHT).max(2) as usize;
        let key = |k: &str| Span::styled(format!("{k:<8}"), Style::new().fg(COMMENT));
        let room = (inner.width as usize).saturating_sub(8);
        let path = |p: &str| Span::styled(tail_fit(&tilde(p), room), Style::new().fg(FG));
        // One line is for the target; a summary line replaces the sources that do not fit.
        let shown = if j.sources.len() < max_paths { j.sources.len() } else { max_paths - 2 };
        let mut lines: Vec<Line> =
            j.sources.iter().take(shown).map(|s| Line::from(vec![key("source"), path(s)])).collect();
        if shown < j.sources.len() {
            let rest = j.sources.len() - shown;
            let (label, text) =
                if shown == 0 { ("source", format!("{rest} sources")) } else { ("", format!("{rest} more sources")) };
            lines.push(Line::from(vec![key(label), Span::styled(text, Style::new().fg(COMMENT).italic())]));
        }
        lines.push(Line::from(vec![key("target"), path(&j.target)]));

        // An empty row separates the paths from the graph when the panel has room for it.
        let spacer = (inner.height > lines.len() as u16 + GRAPH_MIN_HEIGHT) as u16;
        let [paths, _, graph] = Layout::vertical([
            Constraint::Length(lines.len() as u16),
            Constraint::Length(spacer),
            Constraint::Min(0),
        ])
        .areas(inner);
        f.render_widget(Paragraph::new(lines), paths);
        let samples: Vec<u64> = self.speeds.get(&j.id).map(|s| s.iter().copied().collect()).unwrap_or_default();
        draw_speed_graph(f, graph, &samples, j.status);
    }

    fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
        if self.message.as_ref().is_some_and(|(_, _, t)| t.elapsed() > Duration::from_secs(4)) {
            self.message = None;
        }
        let hints: &[(&str, &str)] = match self.mode {
            Mode::Log(_) => &[("↑↓", "scroll"), ("PgUp/PgDn", "page"), ("g/G", "top/follow"), ("q", "close")],
            _ => &[
                ("↑↓", "select"),
                ("p", "pause/resume"),
                ("c", "cancel"),
                ("l", "log"),
                ("d", "remove"),
                ("x", "clear finished"),
                ("q", "quit"),
            ],
        };
        let spans: Vec<Span> = hints.iter().flat_map(|(k, d)| key_hint(k, d)).collect();
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        if let Some((m, c, _)) = &self.message {
            let msg = Line::from(Span::styled(format!(" {m} "), Style::new().fg(BG).bg(*c).bold()))
                .alignment(Alignment::Right);
            let width = (m.chars().count() as u16 + 2).min(area.width);
            f.render_widget(Clear, Rect { x: area.right() - width, width, ..area });
            f.render_widget(Paragraph::new(msg), area);
        }
    }

    fn draw_confirm(&self, f: &mut Frame) {
        let Mode::ConfirmCancel(id) = &self.mode else { return };
        let name = self.jobs.iter().find(|j| &j.id == id).and_then(|j| j.name.clone());
        let [area] = Layout::horizontal([Constraint::Length(62)]).flex(Flex::Center).areas(f.area());
        let [area] = Layout::vertical([Constraint::Length(8)]).flex(Flex::Center).areas(area);
        let block = Block::bordered()
            .border_type(BorderType::Plain)
            .border_style(Style::new().fg(RED))
            .title(Span::styled(" Cancel job ", Style::new().fg(RED).bold()))
            .padding(Padding::uniform(1))
            .style(Style::new().bg(BG_DARK));
        let mut lines = vec![Line::from(vec![
            Span::styled("Stop job ", Style::new().fg(FG)),
            Span::styled(id.clone(), Style::new().fg(FG).bold()),
            Span::styled("? Transferred files stay at the target.", Style::new().fg(FG)),
        ])];
        if let Some(n) = name {
            lines.push(Line::from(Span::styled(n, Style::new().fg(YELLOW).italic())));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" y ", Style::new().fg(BG).bg(RED).bold()),
            Span::styled(" cancel the job    ", Style::new().fg(COMMENT)),
            Span::styled(" any key ", Style::new().fg(BG).bg(COMMENT).bold()),
            Span::styled(" keep it running", Style::new().fg(COMMENT)),
        ]));
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_log(&mut self, f: &mut Frame, area: Rect) {
        let Mode::Log(v) = &mut self.mode else { return };
        let area = Rect { height: area.height.saturating_sub(1), ..area };
        let height = area.height.saturating_sub(2) as usize;
        v.height = height;
        let max = v.lines.len().saturating_sub(height);
        v.scroll = v.scroll.min(max);
        let start = max - v.scroll;
        let end = (start + height).min(v.lines.len());
        let lines: Vec<Line> = v.lines[start..end]
            .iter()
            .map(|l| {
                let c = if l.starts_with("rsync:") || l.starts_with("rsync error") {
                    RED
                } else if l.starts_with("afm:") {
                    PURPLE
                } else if l.starts_with("$ ") {
                    CYAN
                } else if l.ends_with('/') {
                    COMMENT
                } else {
                    FG
                };
                Line::from(Span::styled(l.clone(), Style::new().fg(c)))
            })
            .collect();
        let title = Line::from(vec![
            Span::styled(" Log ", Style::new().fg(PURPLE).bold()),
            Span::styled(format!("{} ", v.id), Style::new().fg(FG).bold()),
            Span::styled(format!("{} lines ", v.lines.len()), Style::new().fg(COMMENT)),
            if v.scroll == 0 {
                Span::styled("follow ", Style::new().fg(GREEN))
            } else {
                Span::styled(format!("up {} ", v.scroll), Style::new().fg(YELLOW))
            },
        ]);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).block(panel(title).border_style(Style::new().fg(PURPLE))).style(Style::new().bg(BG)),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn nice_ceil_rounds_up_to_the_series() {
        use super::nice_ceil;
        assert_eq!(nice_ceil(1), 1);
        assert_eq!(nice_ceil(3), 5);
        assert_eq!(nice_ceil(600), 1024);
        assert_eq!(nice_ceil(1024), 1024);
        assert_eq!(nice_ceil(12 << 20), 20 << 20);
        assert_eq!(nice_ceil(900 << 20), 1 << 30);
    }

    #[test]
    fn base64_matches_reference() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(super::base64(b"foobar"), "Zm9vYmFy");
    }
}
