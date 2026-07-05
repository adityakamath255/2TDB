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

use time_travel_db_rs::{Db, Error, ReadOnly, Snapshot, Timestamp, Value, diff, inspect};

const FOOTER: &str = " tx: h/l ±1 · H/L ±10 · g/G first/latest · :event-or-date jump · n/N selected key's events
 valid: [/] changepoint hop · @date pin · v follow events · u unbounded
 ui: j/k select · / filter · ⏎ history · m diff anchor · d diff-only · esc dismiss · q quit";
const PROMPT_HELP: &str = " enter apply · esc cancel";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args().nth(1).ok_or("usage: scrub <database>")?;
    let db = inspect(path)?;
    let len = db.len()?;
    let mut app = App {
        cursor: (len > 0).then(|| Cursor {
            len,
            seq: len - 1,
            valid: ValidMode::Track,
        }),
        db,
        anchor: None,
        selected: None,
        history: None,
        prompt: None,
        filter: None,
    };
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
    len: usize,
    seq: usize,
    valid: ValidMode,
}

impl Cursor {
    fn step(self, delta: isize) -> Self {
        self.jump(self.seq.saturating_add_signed(delta))
    }

    fn jump(self, seq: usize) -> Self {
        Cursor {
            seq: seq.min(self.len - 1),
            ..self
        }
    }

    fn with_valid(self, valid: ValidMode) -> Self {
        Cursor { valid, ..self }
    }
}

/// A pinned comparison point: which snapshot to diff against, and whether
/// the state table should be narrowed to just the rows that differ from it.
/// `diff_only` only ever exists alongside an `at`, so there's no way to end
/// up with a dangling diff-only mode once the anchor is cleared.
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

struct App {
    db: Db<ReadOnly>,
    cursor: Option<Cursor>,
    anchor: Option<Anchor>,
    selected: Option<String>,
    history: Option<String>,
    prompt: Option<Prompt>,
    filter: Option<String>,
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
        let view = build(app)?;
        terminal.draw(|frame| draw(frame, &view))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if app.prompt.is_some() {
                prompt_key(app, key.code)?;
                continue;
            }
            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('h') | KeyCode::Left => step(app, -1),
                KeyCode::Char('l') | KeyCode::Right => step(app, 1),
                KeyCode::Char('H') => step(app, -10),
                KeyCode::Char('L') => step(app, 10),
                KeyCode::Char('g') => app.cursor = app.cursor.map(|c| c.jump(0)),
                KeyCode::Char('G') => refresh(app)?,
                KeyCode::Char(':') => open_prompt(app, PromptKind::Tx, String::new()),
                KeyCode::Char('@') => open_prompt(app, PromptKind::Valid, String::new()),
                KeyCode::Char('/') => {
                    let initial = app.filter.clone().unwrap_or_default();
                    open_prompt(app, PromptKind::Filter, initial);
                }
                KeyCode::Char('d') => {
                    if let Some(anchor) = &mut app.anchor {
                        anchor.diff_only = !anchor.diff_only;
                    }
                }
                KeyCode::Char('n') => step_key_event(app, 1)?,
                KeyCode::Char('N') => step_key_event(app, -1)?,
                KeyCode::Char('[') => step_valid(app, -1)?,
                KeyCode::Char(']') => step_valid(app, 1)?,
                KeyCode::Char('v') => set_valid(app, ValidMode::Track),
                KeyCode::Char('u') => {
                    let mode = match app.cursor.map(|c| c.valid) {
                        Some(ValidMode::Unbounded) => ValidMode::Track,
                        _ => ValidMode::Unbounded,
                    };
                    set_valid(app, mode);
                }
                KeyCode::Char('m') => {
                    if let Some(c) = app.cursor {
                        app.anchor = if app.anchor.map(|a| a.at) == Some(c) {
                            None
                        } else {
                            Some(Anchor {
                                at: c,
                                diff_only: false,
                            })
                        };
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => select(app, &view.rows, 1),
                KeyCode::Char('k') | KeyCode::Up => select(app, &view.rows, -1),
                KeyCode::Enter => app.history = app.selected.clone(),
                KeyCode::Esc => {
                    if app.history.is_some() {
                        app.history = None;
                    } else if app.filter.is_some() {
                        app.filter = None;
                    } else {
                        app.anchor = None;
                    }
                }
                _ => {}
            }
        }
    }
}

fn open_prompt(app: &mut App, kind: PromptKind, buffer: String) {
    if app.cursor.is_some() {
        app.prompt = Some(Prompt { kind, buffer });
    }
}

fn step_key_event(app: &mut App, dir: isize) -> Result<(), Error> {
    let (Some(cur), Some(key)) = (app.cursor, app.selected.as_deref()) else {
        return Ok(());
    };
    let seqs: Vec<usize> = app
        .db
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
        app.cursor = app.cursor.map(|c| c.jump(s));
    }
    Ok(())
}

fn prompt_key(app: &mut App, code: KeyCode) -> Result<(), Error> {
    let Some(mut prompt) = app.prompt.take() else {
        return Ok(());
    };
    match code {
        KeyCode::Esc => {}
        KeyCode::Enter => {
            if !apply_prompt(app, &prompt)? {
                app.prompt = Some(prompt);
            }
        }
        KeyCode::Backspace => {
            prompt.buffer.pop();
            app.prompt = Some(prompt);
        }
        KeyCode::Char(c) => {
            prompt.buffer.push(c);
            app.prompt = Some(prompt);
        }
        _ => app.prompt = Some(prompt),
    }
    Ok(())
}

fn apply_prompt(app: &mut App, prompt: &Prompt) -> Result<bool, Error> {
    let Some(cur) = app.cursor else {
        return Ok(true);
    };
    match prompt.kind {
        PromptKind::Filter => {
            let trimmed = prompt.buffer.trim();
            app.filter = (!trimmed.is_empty()).then(|| trimmed.to_string());
            Ok(true)
        }
        PromptKind::Tx => {
            if let Ok(seq) = prompt.buffer.trim().parse::<usize>() {
                app.cursor = app.cursor.map(|c| c.jump(seq));
                return Ok(true);
            }
            if let Some(t) = parse_time(&prompt.buffer) {
                if let Some(seq) = seq_known_at(&app.db, cur.len, t)? {
                    app.cursor = app.cursor.map(|c| c.jump(seq));
                }
                return Ok(true);
            }
            Ok(false)
        }
        PromptKind::Valid => match parse_time(&prompt.buffer) {
            Some(t) => {
                app.cursor = app.cursor.map(|c| c.with_valid(ValidMode::Pinned(t)));
                Ok(true)
            }
            None => Ok(false),
        },
    }
}

fn parse_time(input: &str) -> Option<Timestamp> {
    let input = input.trim();
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

/// last event whose ts is at or before t, assuming nondecreasing ts
fn seq_known_at(db: &Db<ReadOnly>, len: usize, t: Timestamp) -> Result<Option<usize>, Error> {
    let (mut lo, mut hi) = (0, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if db.event(mid)?.ts <= t {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo.checked_sub(1))
}

fn step(app: &mut App, delta: isize) {
    app.cursor = app.cursor.map(|c| c.step(delta));
}

fn set_valid(app: &mut App, valid: ValidMode) {
    app.cursor = app.cursor.map(|c| c.with_valid(valid));
}

fn refresh(app: &mut App) -> Result<(), Error> {
    let len = app.db.len()?;
    let valid = app.cursor.map_or(ValidMode::Track, |c| c.valid);
    app.cursor = (len > 0).then(|| Cursor {
        len,
        seq: len - 1,
        valid,
    });
    Ok(())
}

fn step_valid(app: &mut App, dir: isize) -> Result<(), Error> {
    let Some(cur) = app.cursor else {
        return Ok(());
    };
    let points = snapshot(&app.db, cur)?.changepoints()?;
    let at = match cur.valid {
        ValidMode::Track => Some(app.db.event(cur.seq)?.ts),
        ValidMode::Pinned(t) => Some(t),
        ValidMode::Unbounded => None,
    };
    let target = match (dir > 0, at) {
        (true, Some(t)) => points.iter().find(|&&p| p > t),
        (true, None) => None,
        (false, Some(t)) => points.iter().rev().find(|&&p| p < t),
        (false, None) => points.last(),
    };
    if let Some(&t) = target {
        app.cursor = app.cursor.map(|c| c.with_valid(ValidMode::Pinned(t)));
    }
    Ok(())
}

fn select(app: &mut App, rows: &[RowData], delta: isize) {
    if rows.is_empty() {
        app.selected = None;
        return;
    }
    let at = app
        .selected
        .as_ref()
        .and_then(|key| rows.iter().position(|r| &r.key == key));
    let to = match at {
        Some(i) => i.saturating_add_signed(delta).min(rows.len() - 1),
        None => 0,
    };
    app.selected = Some(rows[to].key.clone());
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

/// One piece of the status line: the text, and the color to render it in
/// (`None` for the default/uncolored style). Building this list is the only
/// place that decides what the status line contains — `draw` just renders it.
struct View {
    status: Vec<(String, Option<Color>)>,
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

fn build(app: &App) -> Result<View, Error> {
    let prompt = app.prompt.as_ref().map(|p| {
        let prefix = match p.kind {
            PromptKind::Tx => ':',
            PromptKind::Valid => '@',
            PromptKind::Filter => '/',
        };
        format!(" {prefix}{}▏", p.buffer)
    });
    let Some(cur) = app.cursor else {
        return Ok(View {
            status: vec![(" empty store · G to re-check".to_string(), None)],
            rows: vec![],
            selected: None,
            context_title: String::new(),
            context: vec![],
            prompt,
        });
    };
    let snap = snapshot(&app.db, cur)?;
    let ev = app.db.event(cur.seq)?;

    let mut status = vec![(
        format!(
            " event {}/{} · {} · valid: {}",
            cur.seq,
            cur.len - 1,
            ev.ts,
            fmt_valid(cur.valid)
        ),
        None,
    )];
    if let Some(anchor) = app.anchor {
        status.push((
            format!(" · vs event {} ({})", anchor.at.seq, fmt_valid(anchor.at.valid)),
            None,
        ));
    }
    if let Some(f) = &app.filter {
        status.push((format!(" · /{f}"), None));
    }
    if let Some(anchor) = app.anchor {
        if anchor.diff_only {
            status.push((" · diff-only".to_string(), None));
        }
        status.push((" · changed".to_string(), Some(Color::Yellow)));
        status.push((" +added".to_string(), Some(Color::Green)));
        status.push((" -dropped".to_string(), Some(Color::Red)));
    }

    let mut marks: BTreeMap<String, Mark> = BTreeMap::new();
    let mut dropped: Vec<RowData> = vec![];
    if let Some(anchor) = app.anchor {
        for (key, old, new) in diff(&snapshot(&app.db, anchor.at)?, &snap)? {
            match (old, new) {
                (None, Some(_)) => {
                    marks.insert(key, Mark::Added);
                }
                (Some(old), None) => dropped.push(RowData {
                    kind: kind_name(&old),
                    value: fmt_value(&old),
                    key,
                    mark: Mark::Dropped,
                }),
                _ => {
                    marks.insert(key, Mark::Changed);
                }
            }
        }
    }

    let mut rows: Vec<RowData> = snap
        .entries()?
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
    if let Some(f) = &app.filter {
        rows.retain(|r| r.key.contains(f.as_str()));
    }
    if app.anchor.is_some_and(|a| a.diff_only) {
        rows.retain(|r| r.mark != Mark::Same);
    }

    let selected = app
        .selected
        .as_ref()
        .and_then(|key| rows.iter().position(|r| &r.key == key));

    let (context_title, context) = match &app.history {
        Some(key) => {
            let lines = app
                .db
                .history(key)?
                .into_iter()
                .map(|a| {
                    let what = match &a.value {
                        Some(v) => format!("= {}", fmt_value(v)),
                        None => "deleted".into(),
                    };
                    ContextLine {
                        text: format!("#{} {} · valid {} · at {}", a.seq, what, a.valid, a.ts),
                        pending: a.seq > cur.seq,
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
            (format!("event {}", cur.seq), lines)
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
        .map(|(text, color)| match color {
            Some(c) => Span::styled(text.as_str(), Style::default().fg(*c)),
            None => Span::raw(text.as_str()),
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
