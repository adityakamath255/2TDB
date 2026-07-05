use std::collections::BTreeMap;
use std::env;

use jiff::civil;
use jiff::tz::TimeZone;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use time_travel_db_rs::{Db, Error, ReadOnly, Seq, Snapshot, Timestamp, Value, diff, inspect};

const FOOTER: &str = " tx: h/l ±1 · H/L ±10 · g/G first/latest · :event-or-date jump · n/N selected key's events
 valid: [/] changepoint hop · @date pin · v follow events · u unbounded
 ui: j/k select · / filter · ⏎ history · m diff anchor · d diff-only · esc dismiss · q quit";
const PROMPT_HELP: &str = " enter apply · esc cancel";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args().nth(1).ok_or("usage: scrub <database>")?;
    let db = inspect(path)?;
    let screen = Screen::open(&db)?;
    let mut app = App { db, screen };
    let mut terminal = ratatui::init();
    let outcome = run(&mut terminal, &mut app);
    ratatui::restore();
    outcome
}

#[derive(Clone, Copy, PartialEq)]
enum ValidMode {
    Track,
    Pinned(Timestamp),
    Unbounded,
}

#[derive(Clone, Copy, PartialEq)]
struct Cursor {
    len: Seq,
    seq: Seq,
    valid: ValidMode,
}

impl Cursor {
    fn step(self, delta: Seq) -> Self {
        self.jump(self.seq.saturating_add(delta))
    }

    fn jump(self, seq: Seq) -> Self {
        Cursor {
            seq: seq.clamp(0, self.len - 1),
            ..self
        }
    }

    fn with_valid(self, valid: ValidMode) -> Self {
        Cursor { valid, ..self }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Anchor {
    at: Cursor,
    diff_only: bool,
}

#[derive(Clone, Copy)]
enum PromptKind {
    Tx,
    Valid,
    Filter,
}

struct Prompt {
    kind: PromptKind,
    buffer: String,
}

enum Command {
    Jump(Seq),
    JumpTo(Timestamp),
    Pin(Timestamp),
    Filter(Option<String>),
}

struct App {
    db: Db<ReadOnly>,
    screen: Screen,
}

enum Screen {
    Empty,
    Loaded(Loaded),
}

struct Loaded {
    cursor: Cursor,
    anchor: Option<Anchor>,
    selected: Option<String>,
    history: Option<String>,
    prompt: Option<Prompt>,
    filter: Option<String>,
}

impl Screen {
    fn open(db: &Db<ReadOnly>) -> Result<Self, Error> {
        let len = db.len()?;
        Ok(if len == 0 {
            Screen::Empty
        } else {
            Screen::Loaded(Loaded {
                cursor: Cursor {
                    len,
                    seq: len - 1,
                    valid: ValidMode::Track,
                },
                anchor: None,
                selected: None,
                history: None,
                prompt: None,
                filter: None,
            })
        })
    }
}

fn snapshot<'db>(db: &'db Db<ReadOnly>, cursor: Cursor) -> Result<Snapshot<'db>, Error> {
    let snap = db.at(cursor.seq)?;
    Ok(match cursor.valid {
        ValidMode::Track => snap,
        ValidMode::Pinned(t) => snap.valid_at(t),
        ValidMode::Unbounded => snap.valid_unbounded(),
    })
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let view = build(&app.db, &app.screen)?;
        terminal.draw(|frame| draw(frame, &view))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match &mut app.screen {
            Screen::Empty => match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('G') => app.screen = Screen::open(&app.db)?,
                _ => {}
            },
            Screen::Loaded(st) => {
                if st.prompt.is_some() {
                    st.prompt_key(&app.db, key.code)?;
                } else if key.code == KeyCode::Char('q') {
                    return Ok(());
                } else {
                    st.key(&app.db, key.code, &view.rows)?;
                }
            }
        }
    }
}

impl Loaded {
    fn key(&mut self, db: &Db<ReadOnly>, code: KeyCode, rows: &[RowData]) -> Result<(), Error> {
        match code {
            KeyCode::Char('h') | KeyCode::Left => self.cursor = self.cursor.step(-1),
            KeyCode::Char('l') | KeyCode::Right => self.cursor = self.cursor.step(1),
            KeyCode::Char('H') => self.cursor = self.cursor.step(-10),
            KeyCode::Char('L') => self.cursor = self.cursor.step(10),
            KeyCode::Char('g') => self.cursor = self.cursor.jump(0),
            KeyCode::Char('G') => {
                let len = db.len()?;
                if len > 0 {
                    self.cursor = Cursor {
                        len,
                        seq: len - 1,
                        ..self.cursor
                    };
                }
            }
            KeyCode::Char(':') => {
                self.prompt = Some(Prompt {
                    kind: PromptKind::Tx,
                    buffer: String::new(),
                })
            }
            KeyCode::Char('@') => {
                self.prompt = Some(Prompt {
                    kind: PromptKind::Valid,
                    buffer: String::new(),
                })
            }
            KeyCode::Char('/') => {
                self.prompt = Some(Prompt {
                    kind: PromptKind::Filter,
                    buffer: self.filter.clone().unwrap_or_default(),
                })
            }
            KeyCode::Char('d') => {
                if let Some(anchor) = &mut self.anchor {
                    anchor.diff_only = !anchor.diff_only;
                }
            }
            KeyCode::Char('n') => self.step_key_event(db, 1)?,
            KeyCode::Char('N') => self.step_key_event(db, -1)?,
            KeyCode::Char('[') => self.step_valid(db, -1)?,
            KeyCode::Char(']') => self.step_valid(db, 1)?,
            KeyCode::Char('v') => self.cursor = self.cursor.with_valid(ValidMode::Track),
            KeyCode::Char('u') => {
                let mode = match self.cursor.valid {
                    ValidMode::Unbounded => ValidMode::Track,
                    _ => ValidMode::Unbounded,
                };
                self.cursor = self.cursor.with_valid(mode);
            }
            KeyCode::Char('m') => {
                self.anchor = match self.anchor {
                    Some(a) if a.at == self.cursor => None,
                    _ => Some(Anchor {
                        at: self.cursor,
                        diff_only: false,
                    }),
                };
            }
            KeyCode::Char('j') | KeyCode::Down => self.select(rows, 1),
            KeyCode::Char('k') | KeyCode::Up => self.select(rows, -1),
            KeyCode::Enter => self.history = self.selected.clone(),
            KeyCode::Esc => {
                if self.history.is_some() {
                    self.history = None;
                } else if self.filter.is_some() {
                    self.filter = None;
                } else {
                    self.anchor = None;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn prompt_key(&mut self, db: &Db<ReadOnly>, code: KeyCode) -> Result<(), Error> {
        let Some(prompt) = &mut self.prompt else {
            return Ok(());
        };
        match code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Enter => {
                if let Some(cmd) = parse_command(prompt) {
                    self.prompt = None;
                    self.exec(db, cmd)?;
                }
            }
            KeyCode::Backspace => {
                prompt.buffer.pop();
            }
            KeyCode::Char(c) => prompt.buffer.push(c),
            _ => {}
        }
        Ok(())
    }

    fn exec(&mut self, db: &Db<ReadOnly>, cmd: Command) -> Result<(), Error> {
        match cmd {
            Command::Jump(seq) => self.cursor = self.cursor.jump(seq),
            Command::JumpTo(t) => {
                if let Some(seq) = db.known_at(t)?.seq() {
                    self.cursor = self.cursor.jump(seq);
                }
            }
            Command::Pin(t) => self.cursor = self.cursor.with_valid(ValidMode::Pinned(t)),
            Command::Filter(f) => self.filter = f,
        }
        Ok(())
    }

    fn step_key_event(&mut self, db: &Db<ReadOnly>, dir: Seq) -> Result<(), Error> {
        let Some(key) = self.selected.as_deref() else {
            return Ok(());
        };
        let cur = self.cursor;
        let seqs: Vec<Seq> = db
            .history(key)?
            .iter()
            .map(|a| a.seq)
            .filter(|&s| s < cur.len)
            .collect();
        let target = if dir > 0 {
            seqs.iter().find(|&&s| s > cur.seq)
        } else {
            seqs.iter().rev().find(|&&s| s < cur.seq)
        };
        if let Some(&s) = target {
            self.cursor = cur.jump(s);
        }
        Ok(())
    }

    fn step_valid(&mut self, db: &Db<ReadOnly>, dir: Seq) -> Result<(), Error> {
        let snap = snapshot(db, self.cursor)?;
        let points = snap.changepoints()?;
        let target = match (dir > 0, snap.valid()) {
            (true, Some(t)) => points.iter().find(|&&p| p > t),
            (true, None) => None,
            (false, Some(t)) => points.iter().rev().find(|&&p| p < t),
            (false, None) => points.last(),
        };
        if let Some(&t) = target {
            self.cursor = self.cursor.with_valid(ValidMode::Pinned(t));
        }
        Ok(())
    }

    fn select(&mut self, rows: &[RowData], delta: isize) {
        if rows.is_empty() {
            self.selected = None;
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|key| rows.iter().position(|r| &r.key == key));
        let to = match at {
            Some(i) => i.saturating_add_signed(delta).min(rows.len() - 1),
            None => 0,
        };
        self.selected = Some(rows[to].key.clone());
    }
}

fn parse_command(prompt: &Prompt) -> Option<Command> {
    let input = prompt.buffer.trim();
    match prompt.kind {
        PromptKind::Filter => Some(Command::Filter(
            (!input.is_empty()).then(|| input.to_string()),
        )),
        PromptKind::Tx => input
            .parse()
            .ok()
            .map(Command::Jump)
            .or_else(|| parse_time(input).map(Command::JumpTo)),
        PromptKind::Valid => parse_time(input).map(Command::Pin),
    }
}

fn parse_time(input: &str) -> Option<Timestamp> {
    if let Ok(t) = input.parse::<Timestamp>() {
        return Some(t);
    }
    if let Ok(dt) = input.parse::<civil::DateTime>() {
        return dt.to_zoned(TimeZone::UTC).ok().map(|z| z.timestamp());
    }
    if let Ok(d) = input.parse::<civil::Date>() {
        return d.to_zoned(TimeZone::UTC).ok().map(|z| z.timestamp());
    }
    None
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Same,
    Changed,
    Added,
    Dropped,
}

struct RowData {
    key: String,
    kind: &'static str,
    value: String,
    mark: Mark,
}

struct ContextLine {
    text: String,
    pending: bool,
}

struct StatusSpan {
    text: String,
    color: Option<Color>,
}

impl StatusSpan {
    fn plain(text: impl Into<String>) -> Self {
        StatusSpan {
            text: text.into(),
            color: None,
        }
    }

    fn colored(text: impl Into<String>, color: Color) -> Self {
        StatusSpan {
            text: text.into(),
            color: Some(color),
        }
    }
}

struct View {
    status: Vec<StatusSpan>,
    rows: Vec<RowData>,
    selected: Option<usize>,
    context_title: String,
    context: Vec<ContextLine>,
    prompt: Option<String>,
}

fn fmt_valid(mode: ValidMode) -> String {
    match mode {
        ValidMode::Track => "tracking".to_string(),
        ValidMode::Pinned(t) => t.to_string(),
        ValidMode::Unbounded => "unbounded".to_string(),
    }
}

fn build(db: &Db<ReadOnly>, screen: &Screen) -> Result<View, Error> {
    let Screen::Loaded(st) = screen else {
        return Ok(View {
            status: vec![StatusSpan::plain(" empty store · G to re-check")],
            rows: vec![],
            selected: None,
            context_title: String::new(),
            context: vec![],
            prompt: None,
        });
    };
    let prompt = st.prompt.as_ref().map(|p| {
        let prefix = match p.kind {
            PromptKind::Tx => ':',
            PromptKind::Valid => '@',
            PromptKind::Filter => '/',
        };
        format!(" {prefix}{}▏", p.buffer)
    });
    let snap = snapshot(db, st.cursor)?;
    let ev = db.event(st.cursor.seq)?;

    let mut status = vec![StatusSpan::plain(format!(
        " event {}/{} · {} · valid: {}",
        st.cursor.seq,
        st.cursor.len - 1,
        ev.ts,
        fmt_valid(st.cursor.valid)
    ))];
    if let Some(anchor) = st.anchor {
        status.push(StatusSpan::plain(format!(
            " · vs event {} ({})",
            anchor.at.seq,
            fmt_valid(anchor.at.valid)
        )));
    }
    if let Some(f) = &st.filter {
        status.push(StatusSpan::plain(format!(" · /{f}")));
    }
    if let Some(anchor) = st.anchor {
        if anchor.diff_only {
            status.push(StatusSpan::plain(" · diff-only"));
        }
        status.push(StatusSpan::colored(" · changed", Color::Yellow));
        status.push(StatusSpan::colored(" +added", Color::Green));
        status.push(StatusSpan::colored(" -dropped", Color::Red));
    }

    let mut marks: BTreeMap<String, Mark> = BTreeMap::new();
    let mut dropped: Vec<RowData> = vec![];
    if let Some(anchor) = st.anchor {
        for entry in diff(&snapshot(db, anchor.at)?, &snap)? {
            match (entry.before, entry.after) {
                (None, Some(_)) => {
                    marks.insert(entry.key, Mark::Added);
                }
                (Some(old), None) => dropped.push(RowData {
                    kind: kind_name(&old),
                    value: fmt_value(&old),
                    key: entry.key,
                    mark: Mark::Dropped,
                }),
                _ => {
                    marks.insert(entry.key, Mark::Changed);
                }
            }
        }
    }

    let mut rows: Vec<RowData> = snap
        .entries()?
        .into_iter()
        .map(|e| RowData {
            mark: marks.get(&e.key).copied().unwrap_or(Mark::Same),
            kind: kind_name(&e.value),
            value: fmt_value(&e.value),
            key: e.key,
        })
        .collect();
    rows.extend(dropped);
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    if let Some(f) = &st.filter {
        rows.retain(|r| r.key.contains(f.as_str()));
    }
    if st.anchor.is_some_and(|a| a.diff_only) {
        rows.retain(|r| r.mark != Mark::Same);
    }

    let selected = st
        .selected
        .as_ref()
        .and_then(|key| rows.iter().position(|r| &r.key == key));

    let (context_title, context) = match &st.history {
        Some(key) => {
            let lines = db
                .history(key)?
                .into_iter()
                .map(|a| {
                    let what = match &a.value {
                        Some(v) => format!("= {}", fmt_value(v)),
                        None => "deleted".into(),
                    };
                    ContextLine {
                        text: format!("#{} {} · valid {} · at {}", a.seq, what, a.valid, a.ts),
                        pending: a.seq > st.cursor.seq,
                    }
                })
                .collect();
            (format!("history · {key}"), lines)
        }
        None => {
            let lines = ev
                .changes
                .iter()
                .map(|c| {
                    let what = match &c.value {
                        Some(v) => format!("set {} = {}", c.key, fmt_value(v)),
                        None => format!("del {}", c.key),
                    };
                    ContextLine {
                        text: format!("{what} · valid {}", c.valid),
                        pending: false,
                    }
                })
                .collect();
            (format!("event {}", st.cursor.seq), lines)
        }
    };

    Ok(View {
        status,
        rows,
        selected,
        context_title,
        context,
        prompt,
    })
}

fn draw(frame: &mut Frame, view: &View) {
    let [status, main, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(3),
    ])
    .areas(frame.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(main);

    let spans: Vec<Span> = view
        .status
        .iter()
        .map(|s| match s.color {
            Some(c) => Span::styled(s.text.as_str(), Style::default().fg(c)),
            None => Span::raw(s.text.as_str()),
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().add_modifier(Modifier::REVERSED)),
        status,
    );

    let rows = view.rows.iter().map(|r| {
        let style = match r.mark {
            Mark::Same => Style::default(),
            Mark::Changed => Style::default().fg(Color::Yellow),
            Mark::Added => Style::default().fg(Color::Green),
            Mark::Dropped => Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::CROSSED_OUT),
        };
        Row::new([r.key.clone(), r.kind.to_string(), r.value.clone()]).style(style)
    });
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(35),
            Constraint::Length(6),
            Constraint::Percentage(55),
        ],
    )
    .header(Row::new(["key", "type", "value"]).style(Style::default().add_modifier(Modifier::BOLD)))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .block(Block::bordered().title("state"));
    let mut table_state = TableState::default();
    table_state.select(view.selected);
    frame.render_stateful_widget(table, left, &mut table_state);

    let lines: Vec<Line> = view
        .context
        .iter()
        .map(|line| {
            let text = Line::from(line.text.as_str());
            if line.pending {
                text.style(Style::default().add_modifier(Modifier::DIM))
            } else {
                text
            }
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(view.context_title.as_str())),
        right,
    );

    let footer_text = match &view.prompt {
        Some(p) => format!("{p}\n{PROMPT_HELP}"),
        None => FOOTER.to_string(),
    };
    frame.render_widget(Paragraph::new(footer_text), footer);
}

fn kind_name(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Str(_) => "str",
    }
}

fn fmt_value(value: &Value) -> String {
    match value {
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Str(s) => {
            if s.chars().count() > 40 {
                format!("\"{}…\"", s.chars().take(39).collect::<String>())
            } else {
                format!("\"{s}\"")
            }
        }
    }
}
