//! sptable — interactive spreadsheet IOC. Each row is one dynamically-added PV.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};

use spvirit_tools::spvirit_server::pv::AnyPv;
use spvirit_tools::spvirit_server::pva_server::{PvaServer, RunningServer};
use spvirit_types::{NtPayload, ScalarArrayValue, ScalarValue};

mod parse;
use parse::{WireType, coerce_scalar, format_array, format_scalar, parse_array, parse_scalar};

mod pattern;

mod anim;
use anim::{AnimSpec, AnimState, Generator, is_enum_only, sample};

enum PvSpec {
    Scalar(WireType),
    Array(WireType),
    Enum { choices: Vec<String> },
    Table { columns: Vec<(String, WireType)> },
}

impl PvSpec {
    fn kind_label(&self) -> &'static str {
        match self {
            PvSpec::Scalar(_) => "scalar",
            PvSpec::Array(_) => "array",
            PvSpec::Enum { .. } => "enum",
            PvSpec::Table { .. } => "table",
        }
    }
    fn type_label(&self) -> String {
        match self {
            PvSpec::Scalar(t) | PvSpec::Array(t) => t.label().to_string(),
            PvSpec::Enum { .. } => "enum".to_string(),
            PvSpec::Table { .. } => "table".to_string(),
        }
    }
}

struct PvRow {
    name: String,
    writable: bool,
    display: String,
    spec: PvSpec,
}

/// How a sampled value is applied to a PV.
enum Target {
    Scalar(WireType),
    Enum(Vec<String>),
}

/// A running animation: spec + mutable state + wall-clock origin + target.
struct Live {
    spec: AnimSpec,
    state: AnimState,
    start: Instant,
    target: Target,
}

type Animators = Arc<Mutex<HashMap<String, Live>>>;

/// Shared, live-tunable tick rate in Hz, encoded as `f64::to_bits`.
type RateHz = Arc<AtomicU64>;

#[derive(Copy, Clone)]
enum AddKind {
    Scalar,
    Array,
    Enum,
}

/// Modal input state for the multi-step "add row" flow and inline edit.
enum Mode {
    Browse,
    AddName { buf: String },
    AddKind { name: String },
    AddType { name: String, kind: AddKind, idx: usize },
    AddChoices { name: String, buf: String },              // enum wizard
    AddIndex { name: String, choices: Vec<String>, buf: String },
    AddAccess { name: String, spec_kind: AddKind, ty: WireType, choices: Vec<String>, index: i32, writable: bool },
    AddValue { name: String, ty: WireType, is_array: bool, writable: bool, buf: String },
    Edit { row: usize, buf: String },
    Command { buf: String },
    Help,
}

struct App {
    rows: Vec<PvRow>,
    table: TableState,
    mode: Mode,
    status: String,
    tcp_port: u16,
    udp_port: u16,
    animators: Animators,
    rate: RateHz,
}

/// Owns the runtime + running server; all async store calls go through here.
struct ServerHandle {
    rt: tokio::runtime::Runtime,
    server: RunningServer,
    animators: Animators,
    rate: RateHz,
}

impl ServerHandle {
    fn start(tcp: u16, udp: u16, rate_hz: f64) -> Result<Self, Box<dyn std::error::Error>> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let server = rt.block_on(async {
            PvaServer::serve(Vec::<AnyPv>::new())
                .port(tcp)
                .udp_port(udp)
                .listen_ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
                .start()
                .await
        });
        let animators: Animators = Arc::new(Mutex::new(HashMap::new()));
        let rate: RateHz = Arc::new(AtomicU64::new(rate_hz.to_bits()));

        // Background tick task: sample all animators and write to the store.
        // Re-reads the shared rate each iteration so `:rate` retunes live.
        let store = server.store().clone();
        let anim_map = animators.clone();
        let rate_read = rate.clone();
        rt.spawn(async move {
            loop {
                let hz = f64::from_bits(rate_read.load(Ordering::Relaxed)).max(0.1);
                tokio::time::sleep(Duration::from_secs_f64(1.0 / hz)).await;
                // Compute under the lock; do NOT hold it across awaits.
                let updates: Vec<(String, ScalarValue, Option<Vec<String>>, i32)> = {
                    let mut map = anim_map.lock().unwrap();
                    map.iter_mut()
                        .map(|(name, live)| {
                            let t = live.start.elapsed().as_secs_f64();
                            let raw = sample(&live.spec, &mut live.state, t);
                            match &live.target {
                                Target::Scalar(ty) => {
                                    (name.clone(), coerce_scalar(raw, *ty), None, 0)
                                }
                                Target::Enum(choices) => {
                                    let n = choices.len().max(1) as i64;
                                    let idx = (raw as i64).rem_euclid(n) as i32;
                                    (name.clone(), ScalarValue::I32(idx), Some(choices.clone()), idx)
                                }
                            }
                        })
                        .collect()
                };
                for (name, sval, choices, idx) in updates {
                    match choices {
                        None => {
                            store.set_value(&name, sval).await;
                        }
                        Some(choices) => {
                            let nt = NtPayload::Enum(spvirit_types::NtEnum::new(idx, choices));
                            store.put_nt(&name, nt).await;
                        }
                    }
                }
            }
        });

        Ok(Self { rt, server, animators, rate })
    }

    fn add_scalar(&self, name: &str, v: ScalarValue, writable: bool) {
        self.rt.block_on(self.server.add_scalar(name, v, writable));
    }
    fn add_array(&self, name: &str, v: ScalarArrayValue, writable: bool) {
        self.rt.block_on(self.server.add_array(name, v, writable));
    }
    fn add_enum(&self, name: &str, choices: Vec<String>, index: i32, writable: bool) {
        self.rt.block_on(self.server.add_enum(name, choices, index, writable));
    }
    fn add_table(&self, name: &str, columns: Vec<(String, ScalarArrayValue)>) {
        self.rt.block_on(self.server.add_table(name, columns));
    }
    fn set_scalar(&self, name: &str, v: ScalarValue) {
        self.rt.block_on(self.server.store().set_value(name, v));
    }
    fn set_array(&self, name: &str, v: ScalarArrayValue) {
        self.rt.block_on(self.server.store().set_array_value(name, v));
    }
    fn set_enum(&self, name: &str, index: i32, choices: Vec<String>) {
        let nt = NtPayload::Enum(spvirit_types::NtEnum::new(index, choices));
        self.rt.block_on(self.server.store().put_nt(name, nt));
    }
    fn remove(&self, name: &str) -> bool {
        self.rt.block_on(self.server.store().remove(name))
    }
    fn exists(&self, name: &str) -> bool {
        self.rt.block_on(self.server.store().get_value(name)).is_some()
    }
    /// Read the current formatted value for a scalar row (for refresh).
    fn read_scalar(&self, name: &str) -> Option<String> {
        self.rt
            .block_on(self.server.store().get_value(name))
            .map(|v| format_scalar(&v))
    }
    /// Read an enum row's current display: `index (choice)`.
    fn read_enum(&self, name: &str) -> Option<String> {
        match self.rt.block_on(self.server.store().get_nt(name)) {
            Some(NtPayload::Enum(e)) => {
                let choice = e.selected().unwrap_or("?");
                Some(format!("{} ({})", e.index, choice))
            }
            _ => None,
        }
    }
    fn animators(&self) -> &Animators {
        &self.animators
    }
    fn rate(&self) -> &RateHz {
        &self.rate
    }
    fn abort(&self) {
        self.server.abort();
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(frame.area());

    let header = Row::new(["Name", "Kind", "Type", "R/W", "Value"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let body = app.rows.iter().map(|r| {
        Row::new([
            Cell::from(r.name.clone()),
            Cell::from(r.spec.kind_label()),
            Cell::from(r.spec.type_label()),
            Cell::from(if r.writable { "RW" } else { "RO" }),
            Cell::from(r.display.clone()),
        ])
    });
    let widths = [
        Constraint::Percentage(34),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Percentage(34),
    ];
    let table = Table::new(body, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL).title(format!(
            " sptable — {} PVs @ 127.0.0.1:{} (udp {}) (a add · e edit · d del · q quit) ",
            app.rows.len(),
            app.tcp_port,
            app.udp_port
        )));
    let mut ts = app.table.clone();
    frame.render_stateful_widget(table, chunks[0], &mut ts);

    let status = Paragraph::new(app.status.clone())
        .block(Block::default().borders(Borders::ALL).title(" status "));
    frame.render_widget(status, chunks[1]);

    // Modal prompt overlay for add/edit.
    if let Some((title, content)) = prompt_text(&app.mode) {
        let area = centered(60, 20, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(content).block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }
}

fn prompt_text(mode: &Mode) -> Option<(&'static str, String)> {
    match mode {
        Mode::Browse => None,
        Mode::AddName { buf } => Some((" new PV name (Enter) ", buf.clone())),
        Mode::AddKind { .. } => Some((" kind: [s]calar / [a]rray ", String::new())),
        Mode::AddType { idx, .. } => {
            Some((" type (←/→, Enter) ", WireType::ALL[*idx].label().to_string()))
        }
        Mode::AddChoices { buf, .. } => {
            Some((" enum choices, comma-separated (Enter) ", buf.clone()))
        }
        Mode::AddIndex { buf, .. } => Some((" initial index (Enter) ", buf.clone())),
        Mode::AddAccess { writable, .. } => Some((
            " access: [r]ead-only / [w]ritable (Enter) ",
            if *writable { "writable" } else { "read-only" }.to_string(),
        )),
        Mode::AddValue { buf, ty, is_array, .. } => Some((
            if *is_array { " values, comma-separated (Enter) " } else { " initial value (Enter) " },
            format!("[{}] {}", ty.label(), buf),
        )),
        Mode::Edit { buf, .. } => Some((" new value (Enter) ", buf.clone())),
        Mode::Command { buf } => Some((" command (Enter) ", buf.clone())),
        Mode::Help => Some((" help ", "press any key to close".to_string())),
    }
}

fn centered(pct_w: u16, pct_h: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_h) / 2),
            Constraint::Percentage(pct_h),
            Constraint::Percentage((100 - pct_h) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_w) / 2),
            Constraint::Percentage(pct_w),
            Constraint::Percentage((100 - pct_w) / 2),
        ])
        .split(v[1])[1]
}

fn commit_add(
    app: &mut App,
    srv: &ServerHandle,
    name: &str,
    is_array: bool,
    ty: WireType,
    writable: bool,
    val: &str,
) -> bool {
    if app.rows.iter().any(|r| r.name == name) || srv.exists(name) {
        app.status = format!("name {name:?} already exists");
        return false;
    }
    let display;
    let spec;
    if is_array {
        match parse_array(ty, val) {
            Ok(v) => {
                display = format_array(&v);
                srv.add_array(name, v, writable);
                spec = PvSpec::Array(ty);
            }
            Err(e) => {
                app.status = e;
                return false;
            }
        }
    } else {
        match parse_scalar(ty, val) {
            Ok(v) => {
                display = format_scalar(&v);
                srv.add_scalar(name, v, writable);
                spec = PvSpec::Scalar(ty);
            }
            Err(e) => {
                app.status = e;
                return false;
            }
        }
    }
    app.status = format!("added {name}");
    app.rows.push(PvRow { name: name.to_string(), writable, display, spec });
    app.table.select(Some(app.rows.len() - 1));
    true
}

fn on_key(app: &mut App, srv: &ServerHandle, code: KeyCode) -> bool {
    // returns true to quit
    match std::mem::replace(&mut app.mode, Mode::Browse) {
        Mode::Browse => match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('a') => app.mode = Mode::AddName { buf: String::new() },
            KeyCode::Char('e') | KeyCode::Enter => {
                if let Some(i) = app.table.selected() {
                    app.mode = Mode::Edit { row: i, buf: String::new() };
                }
            }
            KeyCode::Char('d') => {
                if let Some(i) = app.table.selected() {
                    let name = app.rows[i].name.clone();
                    srv.remove(&name);
                    app.rows.remove(i);
                    app.status = format!("removed {name}");
                    if app.rows.is_empty() {
                        app.table.select(None);
                    } else {
                        app.table.select(Some(i.min(app.rows.len() - 1)));
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let n = app.rows.len();
                if n > 0 {
                    let i = app.table.selected().map_or(0, |i| (i + 1) % n);
                    app.table.select(Some(i));
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let n = app.rows.len();
                if n > 0 {
                    let i = app.table.selected().map_or(0, |i| (i + n - 1) % n);
                    app.table.select(Some(i));
                }
            }
            _ => {}
        },
        Mode::AddName { mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter if !buf.trim().is_empty() => {
                app.mode = Mode::AddKind { name: buf.trim().to_string() }
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.mode = Mode::AddName { buf };
            }
            KeyCode::Backspace => {
                buf.pop();
                app.mode = Mode::AddName { buf };
            }
            _ => app.mode = Mode::AddName { buf },
        },
        Mode::AddKind { name } => match code {
            KeyCode::Esc => {}
            KeyCode::Char('s') => {
                app.mode = Mode::AddType { name, kind: AddKind::Scalar, idx: 0 }
            }
            KeyCode::Char('a') => {
                app.mode = Mode::AddType { name, kind: AddKind::Array, idx: 0 }
            }
            _ => app.mode = Mode::AddKind { name },
        },
        Mode::AddType { name, kind, mut idx } => match code {
            KeyCode::Esc => {}
            KeyCode::Left => {
                idx = (idx + WireType::ALL.len() - 1) % WireType::ALL.len();
                app.mode = Mode::AddType { name, kind, idx };
            }
            KeyCode::Right => {
                idx = (idx + 1) % WireType::ALL.len();
                app.mode = Mode::AddType { name, kind, idx };
            }
            KeyCode::Enter => {
                app.mode = Mode::AddAccess {
                    name,
                    spec_kind: kind,
                    ty: WireType::ALL[idx],
                    choices: Vec::new(),
                    index: 0,
                    writable: true,
                }
            }
            _ => app.mode = Mode::AddType { name, kind, idx },
        },
        Mode::AddAccess { name, spec_kind, ty, choices, index, mut writable } => match code {
            KeyCode::Esc => {}
            KeyCode::Char('r') => {
                writable = false;
                app.mode = Mode::AddAccess { name, spec_kind, ty, choices, index, writable };
            }
            KeyCode::Char('w') => {
                writable = true;
                app.mode = Mode::AddAccess { name, spec_kind, ty, choices, index, writable };
            }
            KeyCode::Enter => {
                let is_array = matches!(spec_kind, AddKind::Array);
                app.mode = Mode::AddValue { name, ty, is_array, writable, buf: String::new() }
            }
            _ => app.mode = Mode::AddAccess { name, spec_kind, ty, choices, index, writable },
        },
        Mode::AddValue { name, ty, is_array, writable, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                if !commit_add(app, srv, &name, is_array, ty, writable, &buf) {
                    app.mode = Mode::AddValue { name, ty, is_array, writable, buf };
                }
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.mode = Mode::AddValue { name, ty, is_array, writable, buf };
            }
            KeyCode::Backspace => {
                buf.pop();
                app.mode = Mode::AddValue { name, ty, is_array, writable, buf };
            }
            _ => app.mode = Mode::AddValue { name, ty, is_array, writable, buf },
        },
        Mode::AddChoices { name, buf } => match code {
            KeyCode::Esc => {}
            _ => app.mode = Mode::AddChoices { name, buf },
        },
        Mode::AddIndex { name, choices, buf } => match code {
            KeyCode::Esc => {}
            _ => app.mode = Mode::AddIndex { name, choices, buf },
        },
        Mode::Edit { row, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let r = &app.rows[row];
                let name = r.name.clone();
                match &r.spec {
                    PvSpec::Scalar(ty) => match parse_scalar(*ty, &buf) {
                        Ok(v) => {
                            srv.set_scalar(&name, v.clone());
                            app.rows[row].display = format_scalar(&v);
                            app.status = format!("set {name}");
                        }
                        Err(e) => {
                            app.status = e;
                            app.mode = Mode::Edit { row, buf };
                        }
                    },
                    PvSpec::Array(ty) => match parse_array(*ty, &buf) {
                        Ok(v) => {
                            srv.set_array(&name, v.clone());
                            app.rows[row].display = format_array(&v);
                            app.status = format!("set {name}");
                        }
                        Err(e) => {
                            app.status = e;
                            app.mode = Mode::Edit { row, buf };
                        }
                    },
                    PvSpec::Enum { .. } | PvSpec::Table { .. } => {
                        app.status = "edit not supported for this kind yet".to_string();
                    }
                }
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.mode = Mode::Edit { row, buf };
            }
            KeyCode::Backspace => {
                buf.pop();
                app.mode = Mode::Edit { row, buf };
            }
            _ => app.mode = Mode::Edit { row, buf },
        },
        Mode::Command { buf } => match code {
            KeyCode::Esc => {}
            _ => app.mode = Mode::Command { buf },
        },
        Mode::Help => {}
    }
    false
}

fn refresh_values(app: &mut App, srv: &ServerHandle) {
    for r in app.rows.iter_mut() {
        match &r.spec {
            PvSpec::Scalar(_) => {
                if let Some(s) = srv.read_scalar(&r.name) {
                    r.display = s;
                }
            }
            PvSpec::Enum { .. } => {
                if let Some(s) = srv.read_enum(&r.name) {
                    r.display = s;
                }
            }
            _ => {}
        }
    }
}

fn run_ui(
    mut terminal: DefaultTerminal,
    mut app: App,
    srv: &ServerHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        // Reflect external PUTs into scalar/enum rows each tick.
        if matches!(app.mode, Mode::Browse) {
            refresh_values(&mut app, srv);
        }
        terminal.draw(|f| draw(f, &app))?;
        if event::poll(Duration::from_millis(500))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
            && on_key(&mut app, srv, k.code)
        {
            return Ok(());
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use argparse::{ArgumentParser, Store};
    let mut tcp_port: u16 = 5075;
    let mut udp_port: u16 = 5076;
    let mut rate_hz: f64 = 10.0;
    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Interactive spreadsheet IOC — each row is a PV");
        ap.refer(&mut tcp_port).add_option(&["--port"], Store, "TCP port (default 5075)");
        ap.refer(&mut udp_port)
            .add_option(&["--udp-port"], Store, "UDP search port (default 5076)");
        ap.refer(&mut rate_hz).add_option(
            &["--rate"],
            Store,
            "animation tick rate Hz (default 10)",
        );
        ap.parse_args_or_exit();
    }
    if rate_hz <= 0.0 {
        eprintln!("--rate must be positive");
        std::process::exit(2);
    }

    let srv = ServerHandle::start(tcp_port, udp_port, rate_hz)?;

    color_eyre::install()?;
    let terminal = ratatui::init();
    let app = App {
        rows: Vec::new(),
        table: TableState::default(),
        mode: Mode::Browse,
        status: format!("serving on 127.0.0.1:{tcp_port} — 'a' add · ':' cmd · '?' help"),
        tcp_port,
        udp_port,
        animators: srv.animators().clone(),
        rate: srv.rate().clone(),
    };
    let result = run_ui(terminal, app, &srv);
    ratatui::restore();
    srv.abort();
    result
}
