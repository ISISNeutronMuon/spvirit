//! sptable — interactive spreadsheet IOC. Each row is one dynamically-added PV.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};

use spvirit_tools::spvirit_server::pv::AnyPv;
use spvirit_tools::spvirit_server::pva_server::{PvaServer, RunningServer};
use spvirit_types::{ScalarArrayValue, ScalarValue};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum WireType {
    Bool, I8, I16, I32, I64, U8, U16, U32, U64, F32, F64, Str,
}

impl WireType {
    const ALL: [WireType; 12] = [
        WireType::F64, WireType::F32, WireType::I64, WireType::I32,
        WireType::I16, WireType::I8, WireType::U64, WireType::U32,
        WireType::U16, WireType::U8, WireType::Bool, WireType::Str,
    ];

    fn label(self) -> &'static str {
        match self {
            WireType::Bool => "bool", WireType::I8 => "int8",
            WireType::I16 => "int16", WireType::I32 => "int32",
            WireType::I64 => "int64", WireType::U8 => "uint8",
            WireType::U16 => "uint16", WireType::U32 => "uint32",
            WireType::U64 => "uint64", WireType::F32 => "float",
            WireType::F64 => "double", WireType::Str => "string",
        }
    }

    // Only exercised by the round-trip unit test below; not needed by the TUI,
    // which selects types by index via `WireType::ALL`.
    #[allow(dead_code)]
    fn from_label(s: &str) -> Option<WireType> {
        WireType::ALL.into_iter().find(|t| t.label() == s)
    }
}

fn parse_scalar(ty: WireType, s: &str) -> Result<ScalarValue, String> {
    let s = s.trim();
    let num = |e: std::num::ParseIntError| format!("invalid {}: {e}", ty.label());
    let numf = |e: std::num::ParseFloatError| format!("invalid {}: {e}", ty.label());
    Ok(match ty {
        WireType::Bool => match s {
            "true" | "1" | "on" | "True" => ScalarValue::Bool(true),
            "false" | "0" | "off" | "False" => ScalarValue::Bool(false),
            _ => return Err(format!("invalid bool: {s:?} (use true/false)")),
        },
        WireType::I8 => ScalarValue::I8(s.parse().map_err(num)?),
        WireType::I16 => ScalarValue::I16(s.parse().map_err(num)?),
        WireType::I32 => ScalarValue::I32(s.parse().map_err(num)?),
        WireType::I64 => ScalarValue::I64(s.parse().map_err(num)?),
        WireType::U8 => ScalarValue::U8(s.parse().map_err(num)?),
        WireType::U16 => ScalarValue::U16(s.parse().map_err(num)?),
        WireType::U32 => ScalarValue::U32(s.parse().map_err(num)?),
        WireType::U64 => ScalarValue::U64(s.parse().map_err(num)?),
        WireType::F32 => ScalarValue::F32(s.parse().map_err(numf)?),
        WireType::F64 => ScalarValue::F64(s.parse().map_err(numf)?),
        WireType::Str => ScalarValue::Str(s.to_string()),
    })
}

fn parse_array(ty: WireType, s: &str) -> Result<ScalarArrayValue, String> {
    let toks: Vec<&str> = if s.trim().is_empty() {
        Vec::new()
    } else {
        s.split(',').map(|t| t.trim()).collect()
    };
    macro_rules! collect {
        ($variant:ident) => {{
            let mut out = Vec::with_capacity(toks.len());
            for t in &toks {
                match parse_scalar(ty, t)? {
                    ScalarValue::$variant(v) => out.push(v),
                    _ => unreachable!(),
                }
            }
            ScalarArrayValue::$variant(out)
        }};
    }
    Ok(match ty {
        WireType::Bool => collect!(Bool),
        WireType::I8 => collect!(I8),
        WireType::I16 => collect!(I16),
        WireType::I32 => collect!(I32),
        WireType::I64 => collect!(I64),
        WireType::U8 => collect!(U8),
        WireType::U16 => collect!(U16),
        WireType::U32 => collect!(U32),
        WireType::U64 => collect!(U64),
        WireType::F32 => collect!(F32),
        WireType::F64 => collect!(F64),
        WireType::Str => collect!(Str),
    })
}

fn format_scalar(v: &ScalarValue) -> String {
    match v {
        ScalarValue::Bool(b) => b.to_string(),
        ScalarValue::I8(n) => n.to_string(),
        ScalarValue::I16(n) => n.to_string(),
        ScalarValue::I32(n) => n.to_string(),
        ScalarValue::I64(n) => n.to_string(),
        ScalarValue::U8(n) => n.to_string(),
        ScalarValue::U16(n) => n.to_string(),
        ScalarValue::U32(n) => n.to_string(),
        ScalarValue::U64(n) => n.to_string(),
        ScalarValue::F32(n) => n.to_string(),
        ScalarValue::F64(n) => n.to_string(),
        ScalarValue::Str(s) => s.clone(),
    }
}

fn format_array(v: &ScalarArrayValue) -> String {
    macro_rules! join {
        ($vec:expr) => {
            $vec.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")
        };
    }
    match v {
        ScalarArrayValue::Bool(a) => join!(a),
        ScalarArrayValue::I8(a) => join!(a),
        ScalarArrayValue::I16(a) => join!(a),
        ScalarArrayValue::I32(a) => join!(a),
        ScalarArrayValue::I64(a) => join!(a),
        ScalarArrayValue::U8(a) => join!(a),
        ScalarArrayValue::U16(a) => join!(a),
        ScalarArrayValue::U32(a) => join!(a),
        ScalarArrayValue::U64(a) => join!(a),
        ScalarArrayValue::F32(a) => join!(a),
        ScalarArrayValue::F64(a) => join!(a),
        ScalarArrayValue::Str(a) => a.join(", "),
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Kind {
    Scalar,
    Array,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Scalar => "scalar",
            Kind::Array => "array",
        }
    }
}

struct PvRow {
    name: String,
    kind: Kind,
    ty: WireType,
    writable: bool,
    display: String, // last known value, formatted
}

/// Modal input state for the multi-step "add row" flow and inline edit.
enum Mode {
    Browse,
    AddName { buf: String },
    AddKind { name: String },
    AddType { name: String, kind: Kind, idx: usize },
    AddAccess { name: String, kind: Kind, ty: WireType, writable: bool },
    AddValue { name: String, kind: Kind, ty: WireType, writable: bool, buf: String },
    Edit { row: usize, buf: String },
}

struct App {
    rows: Vec<PvRow>,
    table: TableState,
    mode: Mode,
    status: String,
    tcp_port: u16,
    udp_port: u16,
}

/// Owns the runtime + running server; all async store calls go through here.
struct ServerHandle {
    rt: tokio::runtime::Runtime,
    server: RunningServer,
}

impl ServerHandle {
    fn start(tcp: u16, udp: u16) -> Result<Self, Box<dyn std::error::Error>> {
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
        Ok(Self { rt, server })
    }

    fn add_scalar(&self, name: &str, v: ScalarValue, writable: bool) {
        self.rt.block_on(self.server.add_scalar(name, v, writable));
    }
    fn add_array(&self, name: &str, v: ScalarArrayValue, writable: bool) {
        self.rt.block_on(self.server.add_array(name, v, writable));
    }
    fn set_scalar(&self, name: &str, v: ScalarValue) {
        self.rt.block_on(self.server.store().set_value(name, v));
    }
    fn set_array(&self, name: &str, v: ScalarArrayValue) {
        self.rt.block_on(self.server.store().set_array_value(name, v));
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
            Cell::from(r.kind.label()),
            Cell::from(r.ty.label()),
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
        Mode::AddAccess { writable, .. } => Some((
            " access: [r]ead-only / [w]ritable (Enter) ",
            if *writable { "writable" } else { "read-only" }.to_string(),
        )),
        Mode::AddValue { buf, ty, kind, .. } => Some((
            match kind {
                Kind::Array => " values, comma-separated (Enter) ",
                Kind::Scalar => " initial value (Enter) ",
            },
            format!("[{}] {}", ty.label(), buf),
        )),
        Mode::Edit { buf, .. } => Some((" new value (Enter) ", buf.clone())),
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
    name: String,
    kind: Kind,
    ty: WireType,
    writable: bool,
    val: &str,
) {
    if app.rows.iter().any(|r| r.name == name) || srv.exists(&name) {
        app.status = format!("name {name:?} already exists");
        return;
    }
    let display;
    match kind {
        Kind::Scalar => match parse_scalar(ty, val) {
            Ok(v) => {
                display = format_scalar(&v);
                srv.add_scalar(&name, v, writable);
            }
            Err(e) => {
                app.status = e;
                return;
            }
        },
        Kind::Array => match parse_array(ty, val) {
            Ok(v) => {
                display = format_array(&v);
                srv.add_array(&name, v, writable);
            }
            Err(e) => {
                app.status = e;
                return;
            }
        },
    }
    app.status = format!("added {name}");
    app.rows.push(PvRow { name, kind, ty, writable, display });
    app.table.select(Some(app.rows.len() - 1));
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
            KeyCode::Char('s') => app.mode = Mode::AddType { name, kind: Kind::Scalar, idx: 0 },
            KeyCode::Char('a') => app.mode = Mode::AddType { name, kind: Kind::Array, idx: 0 },
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
                app.mode = Mode::AddAccess { name, kind, ty: WireType::ALL[idx], writable: true }
            }
            _ => app.mode = Mode::AddType { name, kind, idx },
        },
        Mode::AddAccess { name, kind, ty, mut writable } => match code {
            KeyCode::Esc => {}
            KeyCode::Char('r') => {
                writable = false;
                app.mode = Mode::AddAccess { name, kind, ty, writable };
            }
            KeyCode::Char('w') => {
                writable = true;
                app.mode = Mode::AddAccess { name, kind, ty, writable };
            }
            KeyCode::Enter => {
                app.mode = Mode::AddValue { name, kind, ty, writable, buf: String::new() }
            }
            _ => app.mode = Mode::AddAccess { name, kind, ty, writable },
        },
        Mode::AddValue { name, kind, ty, writable, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => commit_add(app, srv, name, kind, ty, writable, &buf),
            KeyCode::Char(c) => {
                buf.push(c);
                app.mode = Mode::AddValue { name, kind, ty, writable, buf };
            }
            KeyCode::Backspace => {
                buf.pop();
                app.mode = Mode::AddValue { name, kind, ty, writable, buf };
            }
            _ => app.mode = Mode::AddValue { name, kind, ty, writable, buf },
        },
        Mode::Edit { row, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let r = &app.rows[row];
                let name = r.name.clone();
                match r.kind {
                    Kind::Scalar => match parse_scalar(r.ty, &buf) {
                        Ok(v) => {
                            srv.set_scalar(&name, v.clone());
                            app.rows[row].display = format_scalar(&v);
                            app.status = format!("set {name}");
                        }
                        Err(e) => app.status = e,
                    },
                    Kind::Array => match parse_array(r.ty, &buf) {
                        Ok(v) => {
                            srv.set_array(&name, v.clone());
                            app.rows[row].display = format_array(&v);
                            app.status = format!("set {name}");
                        }
                        Err(e) => app.status = e,
                    },
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
    }
    false
}

fn refresh_scalars(app: &mut App, srv: &ServerHandle) {
    for r in app.rows.iter_mut() {
        if r.kind == Kind::Scalar
            && let Some(s) = srv.read_scalar(&r.name)
        {
            r.display = s;
        }
    }
}

fn run_ui(
    mut terminal: DefaultTerminal,
    mut app: App,
    srv: &ServerHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        // Reflect external PUTs into scalar rows each tick.
        if matches!(app.mode, Mode::Browse) {
            refresh_scalars(&mut app, srv);
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
    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Interactive spreadsheet IOC — each row is a PV");
        ap.refer(&mut tcp_port).add_option(&["--port"], Store, "TCP port (default 5075)");
        ap.refer(&mut udp_port)
            .add_option(&["--udp-port"], Store, "UDP search port (default 5076)");
        ap.parse_args_or_exit();
    }

    let srv = ServerHandle::start(tcp_port, udp_port)?;

    color_eyre::install()?;
    let terminal = ratatui::init();
    let app = App {
        rows: Vec::new(),
        table: TableState::default(),
        mode: Mode::Browse,
        status: format!("serving on 127.0.0.1:{tcp_port} — press 'a' to add a PV"),
        tcp_port,
        udp_port,
    };
    let result = run_ui(terminal, app, &srv);
    ratatui::restore();
    srv.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_roundtrip_and_errors() {
        assert_eq!(parse_scalar(WireType::I32, "42").unwrap(), ScalarValue::I32(42));
        assert_eq!(parse_scalar(WireType::U8, "255").unwrap(), ScalarValue::U8(255));
        assert!(parse_scalar(WireType::U8, "256").is_err(), "u8 overflow rejected");
        assert!(parse_scalar(WireType::I32, "x").is_err());
        assert_eq!(parse_scalar(WireType::Bool, "on").unwrap(), ScalarValue::Bool(true));
        assert_eq!(
            parse_scalar(WireType::Str, "hi there").unwrap(),
            ScalarValue::Str("hi there".into())
        );
    }

    #[test]
    fn array_parse_and_format() {
        let a = parse_array(WireType::F64, "1.0, 2.5, 3").unwrap();
        assert_eq!(a, ScalarArrayValue::F64(vec![1.0, 2.5, 3.0]));
        assert_eq!(format_array(&a), "1, 2.5, 3");
        assert!(parse_array(WireType::I16, "1, notanint").is_err());
        assert_eq!(parse_array(WireType::I32, "").unwrap(), ScalarArrayValue::I32(vec![]));
    }

    #[test]
    fn wiretype_labels_roundtrip() {
        for t in WireType::ALL {
            assert_eq!(WireType::from_label(t.label()), Some(t));
        }
        assert_eq!(WireType::ALL.len(), 12);
        assert!(WireType::from_label("nope").is_none());
    }
}
