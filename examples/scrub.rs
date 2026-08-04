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

use time_travel_db_rs::{
    Assertion, Delta, Error, EventId, Reader, RecordedAssertion, Snapshot, State, Timestamp, Value,
};

const FOOTER: &str =
    " tx: h/l ±1 · H/L ±10 · g/G first/latest · :event-or-date jump · n/N selected key's events
 valid: [/] changepoint hop · @date pin · v follow events · u unbounded
 ui: j/k select · / filter · ⏎ history · m diff anchor · d diff-only · esc dismiss · q quit";
const PROMPT_HELP: &str = " enter apply · esc cancel";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args().nth(1).ok_or("usage: scrub <database>")?;
    let db = Reader::inspect(path)?;
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
struct Position {
    event: EventId,
    valid: ValidMode,
}

impl Position {
    fn step(self, delta: i64, latest_event: EventId) -> Self {
        let event = if delta < 0 {
            self.event.saturating_sub(delta.unsigned_abs())
        } else {
            self.event.saturating_add(delta as u64)
        };
        self.jump(event, latest_event)
    }

    fn jump(self, event: EventId, latest_event: EventId) -> Self {
        Position {
            event: event.clamp(1, latest_event),
            ..self
        }
    }

    fn with_valid(self, valid: ValidMode) -> Self {
        Position { valid, ..self }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Comparison {
    position: Position,
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
    Jump(EventId),
    JumpTo(Timestamp),
    Pin(Timestamp),
    Filter(Option<String>),
}

struct App {
    db: Reader,
    screen: Screen,
}

enum Screen {
    Empty,
    Loaded(Loaded),
}

struct Loaded {
    position: Position,
    comparison: Option<Comparison>,
    selected: Option<String>,
    history: Option<String>,
    prompt: Option<Prompt>,
    filter: Option<String>,
}

impl Screen {
    fn open(db: &Reader) -> Result<Self, Error> {
        let len = db.len()?;
        Ok(if len == 0 {
            Screen::Empty
        } else {
            Screen::Loaded(Loaded {
                position: Position {
                    event: len,
                    valid: ValidMode::Track,
                },
                comparison: None,
                selected: None,
                history: None,
                prompt: None,
                filter: None,
            })
        })
    }
}

fn snapshot<'db>(db: &'db Reader, position: Position) -> Result<Snapshot<'db>, Error> {
    let snap = db.at(position.event)?;
    Ok(match position.valid {
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
    fn key(&mut self, db: &Reader, code: KeyCode, rows: &[RowData]) -> Result<(), Error> {
        match code {
            KeyCode::Char('h') | KeyCode::Left => self.step_event(db, -1)?,
            KeyCode::Char('l') | KeyCode::Right => self.step_event(db, 1)?,
            KeyCode::Char('H') => self.step_event(db, -10)?,
            KeyCode::Char('L') => self.step_event(db, 10)?,
            KeyCode::Char('g') => self.jump_event(db, 1)?,
            KeyCode::Char('G') => {
                let latest_event = db.len()?;
                if latest_event > 0 {
                    self.position = self.position.jump(latest_event, latest_event);
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
                if let Some(comparison) = &mut self.comparison {
                    comparison.diff_only = !comparison.diff_only;
                }
            }
            KeyCode::Char('n') => self.step_key_event(db, 1)?,
            KeyCode::Char('N') => self.step_key_event(db, -1)?,
            KeyCode::Char('[') => self.step_valid(db, -1)?,
            KeyCode::Char(']') => self.step_valid(db, 1)?,
            KeyCode::Char('v') => self.position = self.position.with_valid(ValidMode::Track),
            KeyCode::Char('u') => {
                let mode = match self.position.valid {
                    ValidMode::Unbounded => ValidMode::Track,
                    _ => ValidMode::Unbounded,
                };
                self.position = self.position.with_valid(mode);
            }
            KeyCode::Char('m') => {
                self.comparison = match self.comparison {
                    Some(comparison) if comparison.position == self.position => None,
                    _ => Some(Comparison {
                        position: self.position,
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
                    self.comparison = None;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn prompt_key(&mut self, db: &Reader, code: KeyCode) -> Result<(), Error> {
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

    fn exec(&mut self, db: &Reader, cmd: Command) -> Result<(), Error> {
        match cmd {
            Command::Jump(id) => self.jump_event(db, id)?,
            Command::JumpTo(t) => {
                if let Some(id) = db.known_at(t)?.event_id() {
                    self.jump_event(db, id)?;
                }
            }
            Command::Pin(t) => {
                self.position = self.position.with_valid(ValidMode::Pinned(t));
            }
            Command::Filter(f) => self.filter = f,
        }
        Ok(())
    }

    fn step_event(&mut self, db: &Reader, delta: i64) -> Result<(), Error> {
        self.position = self.position.step(delta, db.len()?);
        Ok(())
    }

    fn jump_event(&mut self, db: &Reader, event: EventId) -> Result<(), Error> {
        self.position = self.position.jump(event, db.len()?);
        Ok(())
    }

    fn step_key_event(&mut self, db: &Reader, dir: i64) -> Result<(), Error> {
        let Some(key) = self.selected.as_deref() else {
            return Ok(());
        };
        let current = self.position;
        let ids: Vec<EventId> = db
            .history(key)?
            .iter()
            .map(|record| record.event_id)
            .collect();
        let target = if dir > 0 {
            ids.iter().find(|&&id| id > current.event)
        } else {
            ids.iter().rev().find(|&&id| id < current.event)
        };
        if let Some(&id) = target {
            self.jump_event(db, id)?;
        }
        Ok(())
    }

    fn step_valid(&mut self, db: &Reader, dir: i64) -> Result<(), Error> {
        let snap = snapshot(db, self.position)?;
        let points = snap.changepoints()?;
        let target = match (dir > 0, snap.valid_through()) {
            (true, Some(t)) => points.iter().find(|&&p| p > t),
            (true, None) => None,
            (false, Some(t)) => points.iter().rev().find(|&&p| p < t),
            (false, None) => points.last(),
        };
        if let Some(&t) = target {
            self.position = self.position.with_valid(ValidMode::Pinned(t));
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

struct LoadedData {
    latest_event: EventId,
    committed_at: Timestamp,
    state: State,
    diff: BTreeMap<String, Delta>,
    context: ContextData,
}

enum ContextData {
    Event(Vec<Assertion>),
    History {
        key: String,
        records: Vec<RecordedAssertion>,
    },
}

fn load(db: &Reader, state: &Loaded) -> Result<LoadedData, Error> {
    let current = snapshot(db, state.position)?;
    let event = db.event(state.position.event)?;
    let diff = match state.comparison {
        Some(comparison) => snapshot(db, comparison.position)?.diff(&current)?,
        None => BTreeMap::new(),
    };
    let context = match &state.history {
        Some(key) => ContextData::History {
            key: key.clone(),
            records: db.history(key)?,
        },
        None => ContextData::Event(event.assertions),
    };

    Ok(LoadedData {
        latest_event: db.len()?,
        committed_at: event.committed_at,
        state: current.state()?,
        diff,
        context,
    })
}

fn build(db: &Reader, screen: &Screen) -> Result<View, Error> {
    match screen {
        Screen::Empty => Ok(View {
            status: vec![StatusSpan::plain(" empty store · G to re-check")],
            rows: vec![],
            selected: None,
            context_title: String::new(),
            context: vec![],
            prompt: None,
        }),
        Screen::Loaded(state) => Ok(present(state, load(db, state)?)),
    }
}

fn present(state: &Loaded, data: LoadedData) -> View {
    let rows = state_rows(state, data.state, data.diff);
    let selected = state
        .selected
        .as_ref()
        .and_then(|key| rows.iter().position(|row| &row.key == key));
    let (context_title, context) = context(state.position.event, data.context);

    View {
        status: status(state, data.latest_event, data.committed_at),
        rows,
        selected,
        context_title,
        context,
        prompt: prompt(state.prompt.as_ref()),
    }
}

fn prompt(prompt: Option<&Prompt>) -> Option<String> {
    prompt.map(|prompt| {
        let prefix = match prompt.kind {
            PromptKind::Tx => ':',
            PromptKind::Valid => '@',
            PromptKind::Filter => '/',
        };
        format!(" {prefix}{}▏", prompt.buffer)
    })
}

fn status(state: &Loaded, latest_event: EventId, committed_at: Timestamp) -> Vec<StatusSpan> {
    let mut spans = vec![StatusSpan::plain(format!(
        " event {}/{} · {} · valid: {}",
        state.position.event,
        latest_event,
        committed_at,
        fmt_valid(state.position.valid)
    ))];
    if let Some(comparison) = state.comparison {
        spans.push(StatusSpan::plain(format!(
            " · vs event {} ({})",
            comparison.position.event,
            fmt_valid(comparison.position.valid)
        )));
    }
    if let Some(filter) = &state.filter {
        spans.push(StatusSpan::plain(format!(" · /{filter}")));
    }
    if let Some(comparison) = state.comparison {
        if comparison.diff_only {
            spans.push(StatusSpan::plain(" · diff-only"));
        }
        spans.push(StatusSpan::colored(" · changed", Color::Yellow));
        spans.push(StatusSpan::colored(" +added", Color::Green));
        spans.push(StatusSpan::colored(" -dropped", Color::Red));
    }
    spans
}

fn state_rows(state: &Loaded, current: State, diff: BTreeMap<String, Delta>) -> Vec<RowData> {
    let mut marks: BTreeMap<String, Mark> = BTreeMap::new();
    let mut dropped: Vec<RowData> = vec![];
    for (key, delta) in diff {
        match delta {
            Delta::Added(_) => {
                marks.insert(key, Mark::Added);
            }
            Delta::Removed(value) => dropped.push(RowData {
                kind: kind_name(&value),
                value: fmt_value(&value),
                key,
                mark: Mark::Dropped,
            }),
            Delta::Changed { .. } => {
                marks.insert(key, Mark::Changed);
            }
        }
    }

    let mut rows: Vec<RowData> = current
        .into_iter()
        .map(|(key, value)| RowData {
            mark: marks.get(&key).copied().unwrap_or(Mark::Same),
            kind: kind_name(&value),
            value: fmt_value(&value),
            key,
        })
        .collect();
    rows.extend(dropped);
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    if let Some(filter) = &state.filter {
        rows.retain(|row| row.key.contains(filter.as_str()));
    }
    if state
        .comparison
        .is_some_and(|comparison| comparison.diff_only)
    {
        rows.retain(|row| row.mark != Mark::Same);
    }
    rows
}

fn context(event: EventId, data: ContextData) -> (String, Vec<ContextLine>) {
    match data {
        ContextData::History { key, records } => {
            let lines = records
                .into_iter()
                .map(|record| {
                    let what = match &record.assertion.value {
                        Some(value) => format!("= {}", fmt_value(value)),
                        None => "deleted".into(),
                    };
                    ContextLine {
                        text: format!(
                            "#{} {} · valid {} · at {}",
                            record.event_id, what, record.assertion.valid_from, record.committed_at
                        ),
                        pending: record.event_id > event,
                    }
                })
                .collect();
            (format!("history · {key}"), lines)
        }
        ContextData::Event(assertions) => {
            let lines = assertions
                .into_iter()
                .map(|assertion| {
                    let what = match &assertion.value {
                        Some(value) => {
                            format!("set {} = {}", assertion.key, fmt_value(value))
                        }
                        None => format!("del {}", assertion.key),
                    };
                    ContextLine {
                        text: format!("{what} · valid {}", assertion.valid_from),
                        pending: false,
                    }
                })
                .collect();
            (format!("event {event}"), lines)
        }
    }
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
