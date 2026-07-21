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
use parse::{
    Command, SpecInput, WireType, coerce_scalar, format_array, format_scalar, parse_array,
    parse_command, parse_scalar,
};

mod pattern;
use pattern::expand_pattern;

mod anim;
use anim::{AnimSpec, AnimState, build_anim, is_enum_only, sample};

enum PvSpec {
    Scalar(WireType),
    Array(WireType),
    Enum { choices: Vec<String> },
    Table {
        #[allow(dead_code)] // reserved for future table :set / column-aware editing
        columns: Vec<(String, WireType)>,
    },
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
                    // Manual :set/:del/stop removes the animator; skip a stale write so manual control wins.
                    if !anim_map.lock().unwrap().contains_key(&name) {
                        continue;
                    }
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

/// Parse a `col:type=v1,v2 ...` table value into typed columns + a WireType map
/// (for the row's PvSpec). Returns (columns, column-types).
fn parse_table_value(
    val: &str,
) -> Result<(Vec<(String, ScalarArrayValue)>, Vec<(String, WireType)>), String> {
    let mut columns = Vec::new();
    let mut types = Vec::new();
    for col in val.split_whitespace() {
        let (name, rest) = col.split_once(':').ok_or_else(|| format!("bad column {col:?} (want name:type=v,v)"))?;
        let (tytok, vals) = rest.split_once('=').ok_or_else(|| format!("bad column {col:?} (want name:type=v,v)"))?;
        let ty = WireType::from_token(tytok).ok_or_else(|| format!("unknown column type {tytok:?}"))?;
        let arr = parse_array(ty, vals)?;
        columns.push((name.to_string(), arr));
        types.push((name.to_string(), ty));
    }
    if columns.is_empty() {
        return Err("table needs at least one column".into());
    }
    Ok((columns, types))
}

/// Split an enum value field `A,B,C [index]` into choices + index.
fn parse_enum_value(val: &str) -> Result<(Vec<String>, i32), String> {
    let val = val.trim();
    if val.is_empty() {
        return Err("enum needs choices, e.g. OFF,ON,TRIP 1".into());
    }
    // Optional trailing integer index separated by whitespace.
    let (choices_part, index) = match val.rsplit_once(char::is_whitespace) {
        Some((head, tail)) if tail.parse::<i32>().is_ok() => (head, tail.parse::<i32>().unwrap()),
        _ => (val, 0),
    };
    let choices: Vec<String> = choices_part.split(',').map(|c| c.trim().to_string()).collect();
    if choices.iter().any(|c| c.is_empty()) {
        return Err("empty enum choice".into());
    }
    Ok((choices, index))
}

fn row_index(app: &App, name: &str) -> Option<usize> {
    app.rows.iter().position(|r| r.name == name)
}

fn select_row(app: &mut App, name: &str) {
    if let Some(i) = row_index(app, name) {
        app.table.select(Some(i));
    }
}

/// Remove an animator for `name`, if any. Returns true if one was removed.
fn stop_anim(app: &App, name: &str) -> bool {
    app.animators.lock().unwrap().remove(name).is_some()
}

fn exec_command(app: &mut App, srv: &ServerHandle, line: &str) {
    let cmd = match parse_command(line) {
        Ok(c) => c,
        Err(e) => { app.status = e; return; }
    };
    match cmd {
        Command::Quit => { /* handled by caller via a sentinel below */ }
        Command::Help => { app.mode = Mode::Help; }
        Command::Rate { hz } => {
            app.rate.store(hz.to_bits(), Ordering::Relaxed);
            app.status = format!("tick rate -> {hz} Hz");
        }
        Command::Add { pattern, spec, writable, value } => exec_add(app, srv, &pattern, spec, writable, &value),
        Command::Set { pattern, value } => exec_set(app, srv, &pattern, &value),
        Command::Del { pattern } => exec_del(app, srv, pattern),
        Command::Rename { old, new } => exec_rename(app, srv, &old, &new),
        Command::Access { pattern, writable } => exec_access(app, srv, &pattern, writable),
        Command::Anim { pattern, generator, params } => exec_anim(app, srv, &pattern, &generator, &params),
        Command::Stop { pattern } => exec_stop(app, srv, pattern),
        Command::Source { path } => exec_source(app, srv, &path),
    }
}

fn exec_add(app: &mut App, srv: &ServerHandle, pattern: &str, spec: SpecInput, writable: bool, value: &str) {
    let names = match expand_pattern(pattern) {
        Ok(n) => n,
        Err(e) => { app.status = e; return; }
    };
    let (mut added, mut skipped, mut last) = (0usize, 0usize, None);
    let mut errs: Vec<String> = Vec::new();
    for name in names {
        if row_index(app, &name).is_some() || srv.exists(&name) {
            skipped += 1;
            continue;
        }
        let result = match &spec {
            SpecInput::Scalar(ty) => match parse_scalar(*ty, value) {
                Ok(v) => { srv.add_scalar(&name, v.clone(), writable); Ok((PvSpec::Scalar(*ty), format_scalar(&v))) }
                Err(e) => Err(e),
            },
            SpecInput::Array(ty) => match parse_array(*ty, value) {
                Ok(v) => { srv.add_array(&name, v.clone(), writable); Ok((PvSpec::Array(*ty), format_array(&v))) }
                Err(e) => Err(e),
            },
            SpecInput::Enum => match parse_enum_value(value) {
                Ok((choices, index)) => {
                    srv.add_enum(&name, choices.clone(), index, writable);
                    let disp = choices.get(index.max(0) as usize).cloned().unwrap_or_else(|| "?".into());
                    Ok((PvSpec::Enum { choices }, format!("{index} ({disp})")))
                }
                Err(e) => Err(e),
            },
            SpecInput::Table => match parse_table_value(value) {
                Ok((columns, types)) => {
                    let ncols = columns.len();
                    let nrows = columns.first().map(|(_, a)| a.len()).unwrap_or(0);
                    srv.add_table(&name, columns);
                    Ok((PvSpec::Table { columns: types }, format!("{ncols} cols × {nrows} rows")))
                }
                Err(e) => Err(e),
            },
        };
        let (pvspec, display) = match result {
            Ok(v) => v,
            Err(e) => { errs.push(format!("{name}: {e}")); continue; }
        };
        // Tables are always RW at the store layer; reflect that in the row.
        let row_writable = if matches!(spec, SpecInput::Table) { true } else { writable };
        app.rows.push(PvRow { name: name.clone(), writable: row_writable, display, spec: pvspec });
        added += 1;
        last = Some(name);
    }
    if let Some(name) = last { select_row(app, &name); }
    app.status = format!("added {added}, skipped {skipped} (exist)");
    if !errs.is_empty() {
        app.status = format!("{}, {} errors: {}", app.status, errs.len(), errs.join("; "));
    }
}

fn exec_set(app: &mut App, srv: &ServerHandle, pattern: &str, value: &str) {
    let names = match expand_pattern(pattern) {
        Ok(n) => n,
        Err(e) => { app.status = e; return; }
    };
    let mut set = 0usize;
    let mut errs: Vec<String> = Vec::new();
    for name in names {
        let Some(i) = row_index(app, &name) else { continue; };
        stop_anim(app, &name);
        // Reborrow row fields we need, cloning to avoid overlapping borrows.
        let spec_kind = app.rows[i].spec.kind_label();
        match spec_kind {
            "scalar" => {
                if let PvSpec::Scalar(ty) = app.rows[i].spec {
                    match parse_scalar(ty, value) {
                        Ok(v) => { srv.set_scalar(&name, v.clone()); app.rows[i].display = format_scalar(&v); set += 1; }
                        Err(e) => { errs.push(format!("{name}: {e}")); continue; }
                    }
                }
            }
            "array" => {
                if let PvSpec::Array(ty) = app.rows[i].spec {
                    match parse_array(ty, value) {
                        Ok(v) => { srv.set_array(&name, v.clone()); app.rows[i].display = format_array(&v); set += 1; }
                        Err(e) => { errs.push(format!("{name}: {e}")); continue; }
                    }
                }
            }
            "enum" => {
                if let PvSpec::Enum { choices } = &app.rows[i].spec {
                    let choices = choices.clone();
                    // value may be a choice name or an integer index
                    let index = match choices.iter().position(|c| c == value.trim()) {
                        Some(p) => p as i32,
                        None => match value.trim().parse::<i32>() {
                            Ok(n) => n,
                            Err(_) => { errs.push(format!("{name}: {value:?} is not a choice or index")); continue; }
                        },
                    };
                    srv.set_enum(&name, index, choices.clone());
                    let disp = choices.get(index.max(0) as usize).cloned().unwrap_or_else(|| "?".into());
                    app.rows[i].display = format!("{index} ({disp})");
                    set += 1;
                }
            }
            _ => { errs.push(format!("{name}: cannot :set a table (recreate with :add)")); continue; }
        }
    }
    app.status = format!("set {set}");
    if !errs.is_empty() {
        app.status = format!("{}, {} errors: {}", app.status, errs.len(), errs.join("; "));
    }
}

fn exec_del(app: &mut App, srv: &ServerHandle, pattern: Option<String>) {
    let names: Vec<String> = match pattern {
        Some(p) => match expand_pattern(&p) { Ok(n) => n, Err(e) => { app.status = e; return; } },
        None => match app.table.selected().map(|i| app.rows[i].name.clone()) {
            Some(n) => vec![n],
            None => { app.status = "nothing selected".into(); return; }
        },
    };
    let mut removed = 0usize;
    for name in names {
        stop_anim(app, &name);
        srv.remove(&name);
        if let Some(i) = row_index(app, &name) {
            app.rows.remove(i);
            removed += 1;
        }
    }
    if app.rows.is_empty() { app.table.select(None); }
    else {
        let i = app.table.selected().unwrap_or(0).min(app.rows.len() - 1);
        app.table.select(Some(i));
    }
    app.status = format!("removed {removed}");
}

fn exec_rename(app: &mut App, srv: &ServerHandle, old: &str, new: &str) {
    let Some(i) = row_index(app, old) else { app.status = format!("{old}: no such PV"); return; };
    if row_index(app, new).is_some() || srv.exists(new) {
        app.status = format!("{new}: already exists"); return;
    }
    // Recreate under the new name from the current spec + value, then drop old.
    stop_anim(app, old);
    let writable = app.rows[i].writable;
    match &app.rows[i].spec {
        PvSpec::Scalar(ty) => {
            let ty = *ty;
            if let Some(cur) = srv.read_scalar(old) {
                if let Ok(v) = parse_scalar(ty, &cur) { srv.add_scalar(new, v, writable); }
            }
        }
        PvSpec::Enum { choices } => {
            let choices = choices.clone();
            let index = srv.read_enum(old)
                .and_then(|d| d.split_whitespace().next().map(|s| s.to_string()))
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0);
            srv.add_enum(new, choices, index, writable);
        }
        PvSpec::Array(_) | PvSpec::Table { .. } => {
            app.status = "rename supports scalar/enum only (recreate arrays/tables with :add)".into();
            return;
        }
    }
    srv.remove(old);
    let mut row = app.rows.remove(i);
    row.name = new.to_string();
    app.rows.push(row);
    select_row(app, new);
    app.status = format!("renamed {old} -> {new}");
}

fn exec_access(app: &mut App, srv: &ServerHandle, pattern: &str, writable: bool) {
    let names = match expand_pattern(pattern) { Ok(n) => n, Err(e) => { app.status = e; return; } };
    let mut changed = 0usize;
    for name in names {
        let Some(i) = row_index(app, &name) else { continue; };
        // Only scalar/enum have a meaningful access recreate.
        match app.rows[i].spec {
            PvSpec::Scalar(ty) => {
                if let Some(cur) = srv.read_scalar(&name) {
                    if let Ok(v) = parse_scalar(ty, &cur) {
                        srv.add_scalar(&name, v, writable);
                        app.rows[i].writable = writable;
                        changed += 1;
                    }
                }
            }
            PvSpec::Enum { .. } => {
                if let PvSpec::Enum { choices } = &app.rows[i].spec {
                    let choices = choices.clone();
                    let index = srv.read_enum(&name)
                        .and_then(|d| d.split_whitespace().next().map(str::to_string))
                        .and_then(|s| s.parse::<i32>().ok()).unwrap_or(0);
                    srv.add_enum(&name, choices, index, writable);
                    app.rows[i].writable = writable;
                    changed += 1;
                }
            }
            _ => {}
        }
    }
    app.status = format!("access changed on {changed}");
}

fn exec_anim(app: &mut App, _srv: &ServerHandle, pattern: &str, generator: &str, params: &[(String, String)]) {
    let spec = match build_anim(generator, params) { Ok(s) => s, Err(e) => { app.status = e; return; } };
    let names = match expand_pattern(pattern) { Ok(n) => n, Err(e) => { app.status = e; return; } };
    let mut on = 0usize;
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    let mut errs: Vec<String> = Vec::new();
    for name in names {
        let Some(i) = row_index(app, &name) else { continue; };
        let target = match &app.rows[i].spec {
            PvSpec::Scalar(ty) if *ty != WireType::Str => Target::Scalar(*ty),
            PvSpec::Enum { choices } if is_enum_only(&spec.generator) => Target::Enum(choices.clone()),
            PvSpec::Enum { .. } => { errs.push(format!("{name}: enum takes only the 'cycle' generator")); continue; }
            _ => { errs.push(format!("{name}: only numeric scalars (and enum+cycle) are animatable")); continue; }
        };
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(i as u64 + 1);
        let live = Live { spec, state: AnimState::new(seed), start: Instant::now(), target };
        app.animators.lock().unwrap().insert(name, live);
        on += 1;
    }
    app.status = format!("animating {on} ({generator})");
    if !errs.is_empty() {
        app.status = format!("{}, {} skipped: {}", app.status, errs.len(), errs.join("; "));
    }
}

fn exec_stop(app: &mut App, _srv: &ServerHandle, pattern: Option<String>) {
    let names: Vec<String> = match pattern {
        Some(p) => match expand_pattern(&p) { Ok(n) => n, Err(e) => { app.status = e; return; } },
        None => match app.table.selected().map(|i| app.rows[i].name.clone()) {
            Some(n) => vec![n],
            None => { app.status = "nothing selected".into(); return; }
        },
    };
    let mut stopped = 0usize;
    for name in names { if stop_anim(app, &name) { stopped += 1; } }
    app.status = format!("stopped {stopped}");
}

fn exec_source(app: &mut App, srv: &ServerHandle, path: &str) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => { app.status = format!("source {path}: {e}"); return; }
    };
    let (mut n, mut errs) = (0usize, Vec::new());
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        let line = line.strip_prefix(':').unwrap_or(line);
        // Reuse exec_command but capture parse errors per line.
        match parse_command(line) {
            Ok(_) => { exec_command(app, srv, line); n += 1; }
            Err(e) => errs.push(format!("line {}: {e}", lineno + 1)),
        }
    }
    app.status = if errs.is_empty() {
        format!("sourced {n} commands")
    } else {
        format!("sourced {n} commands, {} errors: {}", errs.len(), errs.join("; "))
    };
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(frame.area());

    let header = Row::new(["Name", "Kind", "Type", "R/W", "Value"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let animated = app.animators.lock().unwrap();
    let body = app.rows.iter().map(|r| {
        let mark = if animated.contains_key(&r.name) { "~" } else { "" };
        Row::new([
            Cell::from(r.name.clone()),
            Cell::from(r.spec.kind_label()),
            Cell::from(r.spec.type_label()),
            Cell::from(if r.writable { "RW" } else { "RO" }),
            Cell::from(format!("{mark}{}", r.display)),
        ])
    }).collect::<Vec<_>>();
    drop(animated);
    let widths = [
        Constraint::Percentage(34),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Percentage(34),
    ];
    let rate_hz = f64::from_bits(app.rate.load(Ordering::Relaxed));
    let table = Table::new(body, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL).title(format!(
            " sptable — {} PVs @ 127.0.0.1:{} (udp {}) rate {rate_hz}Hz (a add · : cmd · ? help · q quit) ",
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
        let area = if matches!(app.mode, Mode::Help) {
            centered(80, 70, frame.area())
        } else {
            centered(60, 20, frame.area())
        };
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
        Mode::AddKind { .. } => Some((" kind: [s]calar / [a]rray / [e]num (table via :add) ", String::new())),
        Mode::AddType { idx, .. } => {
            Some((" type (←/→, Enter) ", WireType::ALL[*idx].label().to_string()))
        }
        Mode::AddChoices { buf, .. } => Some((" enum choices, comma-separated (Enter) ", buf.clone())),
        Mode::AddIndex { buf, .. } => Some((" initial index (Enter, default 0) ", buf.clone())),
        Mode::AddAccess { writable, .. } => Some((
            " access: [r]ead-only / [w]ritable (Enter) ",
            if *writable { "writable" } else { "read-only" }.to_string(),
        )),
        Mode::AddValue { buf, ty, is_array, .. } => Some((
            if *is_array { " values, comma-separated (Enter) " } else { " initial value (Enter) " },
            format!("[{}] {buf}", ty.label()),
        )),
        Mode::Edit { buf, .. } => Some((" new value (Enter) ", buf.clone())),
        Mode::Command { buf } => Some((" :command (Enter run · Esc cancel) ", format!(":{buf}"))),
        Mode::Help => Some((" help (Esc to close) ", help_text())),
    }
}

fn help_text() -> String {
    "\
Commands (prefix :) — shorthands in ( )
  add|a  <name> <type> [ro|rw] <value>   add PV(s)
  set|s  <name> <value>                  set value (choice name or index for enum)
  del|d  [name]                          delete (blank = selected row)
  rename|mv <old> <new>                  rename (scalar/enum)
  ro|rw  <name>                          set advertised access
  anim   <name> <gen> [k=v ...]          animate
  stop   [name]                          stop animation (blank = selected)
  source|so <file>                       run a file of commands
  rate   <hz>                            (set at startup via --rate)
  help|h    quit|q

Types: bool int8 int16 int32(int) int64(long) uint8 uint16 uint32 uint64
       float(f32) double(f64) string(s) ; arrays: int32[]  ; enum ; table
Patterns: {1..8} {8..1} {0..100..10} {01..12} {A,B,C}  and products S{1..4}:{A,B}
Generators: sine ramp triangle square noise walk count  (enum: cycle)
  e.g. :anim RING:BPM{01..99} noise min=-1 max=1
Value forms: enum -> OFF,ON,TRIP 1   table -> id:i32=1,2,3 x:f64=0.5,1.5"
        .to_string()
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

fn on_key(app: &mut App, srv: &ServerHandle, code: KeyCode) -> bool {
    // returns true to quit
    match std::mem::replace(&mut app.mode, Mode::Browse) {
        Mode::Browse => match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('a') => app.mode = Mode::AddName { buf: String::new() },
            KeyCode::Char(':') => app.mode = Mode::Command { buf: String::new() },
            KeyCode::Char('?') => app.mode = Mode::Help,
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
            KeyCode::Char('e') => app.mode = Mode::AddChoices { name, buf: String::new() },
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
            KeyCode::Enter => match spec_kind {
                AddKind::Enum => {
                    if row_index(app, &name).is_some() || srv.exists(&name) {
                        app.status = format!("name {name:?} already exists");
                    } else {
                        srv.add_enum(&name, choices.clone(), index, writable);
                        let disp = choices.get(index.max(0) as usize).cloned().unwrap_or_else(|| "?".into());
                        app.rows.push(PvRow {
                            name: name.clone(),
                            writable,
                            display: format!("{index} ({disp})"),
                            spec: PvSpec::Enum { choices },
                        });
                        app.table.select(Some(app.rows.len() - 1));
                        app.status = format!("added {name}");
                    }
                }
                AddKind::Scalar | AddKind::Array => {
                    let is_array = matches!(spec_kind, AddKind::Array);
                    app.mode = Mode::AddValue { name, ty, is_array, writable, buf: String::new() };
                }
            },
            _ => app.mode = Mode::AddAccess { name, spec_kind, ty, choices, index, writable },
        },
        Mode::AddValue { name, ty, is_array, writable, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let spec = if is_array { SpecInput::Array(ty) } else { SpecInput::Scalar(ty) };
                // Reuse exec_add's single-name path via a one-shot pattern.
                let before = app.rows.len();
                exec_add(app, srv, &name, spec, writable, &buf);
                if app.rows.len() == before {
                    // add failed (parse error) — keep the value prompt open
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
        Mode::AddChoices { name, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter if !buf.trim().is_empty() => {
                let choices: Vec<String> = buf.split(',').map(|c| c.trim().to_string()).collect();
                app.mode = Mode::AddIndex { name, choices, buf: String::new() };
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.mode = Mode::AddChoices { name, buf };
            }
            KeyCode::Backspace => {
                buf.pop();
                app.mode = Mode::AddChoices { name, buf };
            }
            _ => app.mode = Mode::AddChoices { name, buf },
        },
        Mode::AddIndex { name, choices, mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let index: i32 = buf.trim().parse().unwrap_or(0);
                app.mode = Mode::AddAccess {
                    name,
                    spec_kind: AddKind::Enum,
                    ty: WireType::I32,
                    choices,
                    index,
                    writable: true,
                };
            }
            KeyCode::Char(c) => {
                buf.push(c);
                app.mode = Mode::AddIndex { name, choices, buf };
            }
            KeyCode::Backspace => {
                buf.pop();
                app.mode = Mode::AddIndex { name, choices, buf };
            }
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
        Mode::Command { mut buf } => match code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                if parse_command(buf.trim()) == Ok(Command::Quit) {
                    return true;
                }
                exec_command(app, srv, buf.trim());
            }
            KeyCode::Char(c) => { buf.push(c); app.mode = Mode::Command { buf }; }
            KeyCode::Backspace => { buf.pop(); app.mode = Mode::Command { buf }; }
            _ => app.mode = Mode::Command { buf },
        },
        Mode::Help => match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {}
            _ => app.mode = Mode::Help,
        },
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
