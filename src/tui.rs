use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row as TRow, Table, TableState, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::inventory::{merge_removals, Kind, Removal, Source};
use crate::report::{self, date, Analysis, Status};

pub fn run(db_path: &Path, days: u32) -> Result<()> {
    let analysis = report::analyze(db_path, days)?;
    ratatui::run(|term| App::new(analysis).run(term))
}

struct App {
    a: Analysis,
    /// 0 is "all"; 1.. index `Source::ALL`.
    tab: usize,
    show_all: bool,
    filter: String,
    editing_filter: bool,
    /// Indices into `a.rows`.
    selected: BTreeSet<usize>,
    table: TableState,
    message: Option<(String, Color)>,
}

fn removal(kind: &Kind) -> Option<&Removal> {
    match kind {
        Kind::Removable(r) | Kind::Leftover(r) => Some(r),
        Kind::Depended | Kind::NoExec => None,
    }
}

fn status_color(s: Status) -> Color {
    match s {
        Status::Unused => Color::Red,
        Status::Leftover => Color::Magenta,
        Status::Used => Color::Green,
        Status::Depended => Color::Blue,
        Status::NoExec => Color::DarkGray,
    }
}

impl App {
    fn new(a: Analysis) -> Self {
        let message = a.warnings.first().map(|w| (w.clone(), Color::Yellow));
        App {
            a,
            tab: 0,
            show_all: false,
            filter: String::new(),
            editing_filter: false,
            selected: BTreeSet::new(),
            table: TableState::default().with_selected(0),
            message,
        }
    }

    fn source(&self) -> Option<Source> {
        self.tab.checked_sub(1).map(|i| Source::ALL[i])
    }

    /// Rows shown under the current tab, status toggle and filter.
    fn visible(&self) -> Vec<usize> {
        let src = self.source();
        let needle = self.filter.to_lowercase();
        (0..self.a.rows.len())
            .filter(|&i| {
                let r = &self.a.rows[i];
                src.is_none_or(|s| r.tool.source == s)
                    && (self.show_all || matches!(r.status, Status::Unused | Status::Leftover))
                    && (needle.is_empty() || r.tool.name.to_lowercase().contains(&needle))
            })
            .collect()
    }

    fn current(&self) -> Option<usize> {
        self.visible().get(self.table.selected()?).copied()
    }

    fn run(mut self, term: &mut DefaultTerminal) -> Result<()> {
        loop {
            term.draw(|f| self.draw(f))?;
            let Event::Key(k) = event::read()? else { continue };
            if k.kind != KeyEventKind::Press {
                continue;
            }
            if self.editing_filter {
                self.filter_key(k);
            } else if !self.key(k) {
                return Ok(());
            }
            let n = self.visible().len();
            match self.table.selected() {
                _ if n == 0 => self.table.select(None),
                None => self.table.select(Some(0)),
                Some(i) if i >= n => self.table.select(Some(n - 1)),
                Some(_) => {}
            }
        }
    }

    fn filter_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => self.editing_filter = false,
            KeyCode::Esc => {
                self.editing_filter = false;
                self.filter.clear();
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => {}
        }
        self.table.select(Some(0));
    }

    /// Returns false to quit.
    fn key(&mut self, k: KeyEvent) -> bool {
        let tabs = Source::ALL.len() + 1;
        let page = 10;
        match k.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Esc if !self.filter.is_empty() => self.filter.clear(),
            KeyCode::Esc => return false,
            KeyCode::Down | KeyCode::Char('j') => self.table.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.table.select_previous(),
            KeyCode::PageDown => self.table.scroll_down_by(page),
            KeyCode::PageUp => self.table.scroll_up_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.table.select_first(),
            KeyCode::End | KeyCode::Char('G') => self.table.select_last(),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.switch_tab((self.tab + 1) % tabs),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.switch_tab((self.tab + tabs - 1) % tabs),
            KeyCode::Char('u') => {
                self.show_all = !self.show_all;
                self.table.select(Some(0));
            }
            KeyCode::Char('/') => self.editing_filter = true,
            KeyCode::Char(' ') => {
                if let Some(i) = self.current() {
                    if removal(&self.a.rows[i].tool.kind).is_some() && !self.selected.remove(&i) {
                        self.selected.insert(i);
                    }
                    self.table.select_next();
                }
            }
            KeyCode::Char('a') => {
                let rows = &self.a.rows;
                let pick: Vec<usize> = self
                    .visible()
                    .into_iter()
                    .filter(|&i| matches!(rows[i].status, Status::Unused | Status::Leftover))
                    .collect();
                self.selected.extend(pick);
            }
            KeyCode::Char('x') => self.selected.clear(),
            KeyCode::Char('y') => self.copy(),
            _ => {}
        }
        true
    }

    fn switch_tab(&mut self, t: usize) {
        self.tab = t;
        self.table.select(Some(0));
    }

    /// Copies removal commands for the selection, or for the current row when nothing is
    /// selected. Nothing is run: the user reviews and pastes.
    fn copy(&mut self) {
        let idx: Vec<usize> = if self.selected.is_empty() { self.current().into_iter().collect() } else { self.selected.iter().copied().collect() };
        let rs: Vec<&Removal> = idx.iter().filter_map(|&i| removal(&self.a.rows[i].tool.kind)).collect();
        if rs.is_empty() {
            self.message = Some(("nothing removable selected".into(), Color::Yellow));
            return;
        }
        let line = merge_removals(rs.iter().copied());
        self.message = Some(match clipboard(&line) {
            Ok(()) => {
                let body = format!("{} tool(s). Review before running.", rs.len());
                let _ = Command::new("notify-send")
                    .args(["-a", "lastrun", "Removal command copied", &body])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                (format!("copied: {line}"), Color::Green)
            }
            Err(e) => (format!("clipboard failed ({e}); command: {line}"), Color::Red),
        });
    }

    fn draw(&mut self, f: &mut Frame) {
        let cov = self.a.coverage();
        let [head, tabs, body, detail, foot] = Layout::vertical([
            Constraint::Length(u16::try_from(cov.len()).unwrap_or(4)),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Max(8),
            Constraint::Length(2),
        ])
        .areas(f.area());

        let cov: Vec<Line> = cov
            .into_iter()
            .map(|l| if l.starts_with("warning") { Line::from(l).yellow() } else { Line::from(l).dim() })
            .collect();
        f.render_widget(Paragraph::new(cov).wrap(Wrap { trim: true }), head);

        let count = |s: Option<Source>| {
            self.a.rows.iter().filter(|r| r.status == Status::Unused && s.is_none_or(|s| r.tool.source == s)).count()
        };
        let titles = std::iter::once(format!("all {}", count(None)))
            .chain(Source::ALL.iter().map(|s| format!("{} {}", s.label(), count(Some(*s)))));
        f.render_widget(Tabs::new(titles).select(self.tab).highlight_style(Style::new().reversed()), tabs);

        self.draw_table(f, body);
        self.draw_detail(f, detail);
        self.draw_footer(f, foot);
    }

    fn draw_table(&mut self, f: &mut Frame, area: Rect) {
        let vis = self.visible();
        let rows = vis.iter().map(|&i| {
            let r = &self.a.rows[i];
            let mark = if self.selected.contains(&i) { "[x]" } else { "[ ]" };
            let (last, count) = r.hit.map_or(("never".to_owned(), String::new()), |h| (date(h.last), h.count.to_string()));
            let cmd = removal(&r.tool.kind).map(ToString::to_string).unwrap_or_default();
            TRow::new([
                Cell::from(mark),
                Cell::from(r.tool.name.as_str()),
                Cell::from(r.tool.source.label()),
                Cell::from(r.status.label()).fg(status_color(r.status)),
                Cell::from(last),
                Cell::from(count),
                Cell::from(cmd).dim(),
            ])
        });
        let title = format!(
            " {} of {} shown, {} selected{} ",
            vis.len(),
            self.a.rows.len(),
            self.selected.len(),
            if self.show_all { ", all statuses" } else { "" }
        );
        let table = Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Fill(2),
                Constraint::Length(9),
                Constraint::Length(8),
                Constraint::Length(10),
                Constraint::Length(6),
                Constraint::Fill(3),
            ],
        )
        .header(TRow::new(["", "tool", "source", "status", "last run", "runs", "removal"]).bold())
        .block(Block::bordered().title(title))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_detail(&self, f: &mut Frame, area: Rect) {
        let Some(i) = self.current() else {
            f.render_widget(Block::bordered().title(" details "), area);
            return;
        };
        let r = &self.a.rows[i];
        let mut lines = vec![Line::from(vec![
            Span::from(r.tool.name.as_str()).bold(),
            Span::from(format!("  {}  ", r.tool.source.label())),
            Span::from(r.status.label()).fg(status_color(r.status)),
            Span::from(r.tool.note.as_deref().map(|n| format!("  {n}")).unwrap_or_default()).dim(),
        ])];
        let paths = report::matching_paths(&r.tool, &self.a.usage);
        if paths.is_empty() {
            lines.push(Line::from("no recorded runs").dim());
        }
        let room = usize::from(area.height.saturating_sub(3));
        for (p, h) in paths.iter().take(room) {
            lines.push(Line::from(format!("{}  x{:<6} {p}", date(h.last), h.count)));
        }
        if paths.len() > room {
            lines.push(Line::from(format!("... {} more paths", paths.len() - room)).dim());
        }
        f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" details ")), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let keys = if self.editing_filter {
            Line::from(format!("filter: {}_   enter keep  esc clear", self.filter)).yellow()
        } else {
            let filter = if self.filter.is_empty() { String::new() } else { format!("  [filter: {}]", self.filter) };
            Line::from(format!(
                "j/k move  h/l source  space select  a select unused  x clear  y copy removal  u {}  / filter  q quit{filter}",
                if self.show_all { "unused only" } else { "show all" }
            ))
            .dim()
        };
        let msg = self.message.as_ref().map_or_else(Line::default, |(m, c)| Line::from(m.as_str()).fg(*c));
        f.render_widget(Paragraph::new(vec![keys, msg]), area);
    }
}

fn clipboard(text: &str) -> std::io::Result<()> {
    // wl-copy forks a background process to serve the selection, so waiting returns at once.
    let mut child = Command::new("wl-copy").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    child.stdin.take().expect("piped stdin").write_all(text.as_bytes())?;
    let st = child.wait()?;
    if st.success() { Ok(()) } else { Err(std::io::Error::other(format!("wl-copy exited {st}"))) }
}
