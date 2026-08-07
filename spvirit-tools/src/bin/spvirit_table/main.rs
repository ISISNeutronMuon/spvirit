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
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

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
    help_scroll: u16,
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
                .expect("server start hooks must succeed")
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
    /// Read a table row back as a `:add`-compatible value string:
    /// `col:type=v,v col2:type=v,v` (columns space-separated, values
    /// comma-separated, no spaces inside a column so `:source` re-parses it).
    fn read_table(&self, name: &str) -> Option<String> {
        match self.rt.block_on(self.server.store().get_nt(name)) {
            Some(NtPayload::Table(t)) => {
                let cols: Vec<String> = t
                    .columns
                    .iter()
                    .map(|c| {
                        let (ty, vals) = array_type_and_values(&c.values);
                        format!("{}:{}={}", c.name, ty, vals)
                    })
                    .collect();
                Some(cols.join(" "))
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
        Command::Help => { app.help_scroll = 0; app.mode = Mode::Help; }
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
        Command::Write { path } => exec_write(app, srv, &path),
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

/// A typespec token + comma-joined values (no spaces) for one array/table
/// column, e.g. `("int32", "1,2,3")`. Tokens are the long labels accepted by
/// `WireType::from_token`.
fn array_type_and_values(a: &ScalarArrayValue) -> (&'static str, String) {
    macro_rules! join {
        ($v:expr) => {
            $v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",")
        };
    }
    match a {
        ScalarArrayValue::Bool(v) => ("bool", join!(v)),
        ScalarArrayValue::I8(v) => ("int8", join!(v)),
        ScalarArrayValue::I16(v) => ("int16", join!(v)),
        ScalarArrayValue::I32(v) => ("int32", join!(v)),
        ScalarArrayValue::I64(v) => ("int64", join!(v)),
        ScalarArrayValue::U8(v) => ("uint8", join!(v)),
        ScalarArrayValue::U16(v) => ("uint16", join!(v)),
        ScalarArrayValue::U32(v) => ("uint32", join!(v)),
        ScalarArrayValue::U64(v) => ("uint64", join!(v)),
        ScalarArrayValue::F32(v) => ("float", join!(v)),
        ScalarArrayValue::F64(v) => ("double", join!(v)),
        ScalarArrayValue::Str(v) => ("string", v.join(",")),
    }
}

/// Dump the whole session as a `:source`-compatible script: a `rate` line,
/// one `add` per PV (current value, read back so edits/tables survive), then
/// one `anim` per animated PV — all in row order. Loading it rebuilds state.
fn exec_write(app: &mut App, srv: &ServerHandle, path: &str) {
    let mut out = String::from("# sptable session dump\n");
    let rate_hz = f64::from_bits(app.rate.load(Ordering::Relaxed));
    out.push_str(&format!("rate {rate_hz}\n"));

    for r in &app.rows {
        let access = if r.writable { "rw" } else { "ro" };
        let line = match &r.spec {
            PvSpec::Scalar(ty) => {
                let val = srv.read_scalar(&r.name).unwrap_or_else(|| r.display.clone());
                format!("add {} {} {} {}", r.name, ty.label(), access, val)
            }
            PvSpec::Array(ty) => {
                // row.display is the last-set array, "1, 2, 3" — parse_array
                // trims, so the spaces are harmless in a scalar-position value.
                format!("add {} {}[] {} {}", r.name, ty.label(), access, r.display)
            }
            PvSpec::Enum { choices } => {
                let index = srv
                    .read_enum(&r.name)
                    .and_then(|d| d.split_whitespace().next().map(str::to_string))
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(0);
                format!("add {} enum {} {} {}", r.name, access, choices.join(","), index)
            }
            PvSpec::Table { .. } => match srv.read_table(&r.name) {
                // table is always RW; omit the access token (its value never
                // starts with an exact `ro`/`rw` token).
                Some(cols) => format!("add {} table {}", r.name, cols),
                None => continue,
            },
        };
        out.push_str(&line);
        out.push('\n');
    }

    // Animations, in row order, reconstructed from resolved params.
    {
        let animated = app.animators.lock().unwrap();
        for r in &app.rows {
            if let Some(live) = animated.get(&r.name) {
                out.push_str(&format!(
                    "anim {} {} {}\n",
                    r.name,
                    live.spec.generator.name(),
                    live.spec.dump_params()
                ));
            }
        }
    }

    match std::fs::write(path, out) {
        Ok(()) => app.status = format!("wrote {} PVs to {path}", app.rows.len()),
        Err(e) => app.status = format!("write {path}: {e}"),
    }
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

    // Modal prompt overlay for add/edit/command/help.
    if let Some((title, content)) = prompt_text(&app.mode) {
        let is_help = matches!(app.mode, Mode::Help);
        let area = if is_help {
            centered(90, 90, frame.area())
        } else {
            centered(64, 30, frame.area())
        };
        frame.render_widget(Clear, area);
        let mut para = Paragraph::new(content)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false });
        if is_help {
            para = para.scroll((app.help_scroll, 0));
        }
        frame.render_widget(para, area);
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
        Mode::Command { buf } => {
            let hint = command_hint(buf);
            let content = if hint.is_empty() {
                format!(":{buf}")
            } else {
                format!(":{buf}\n {hint}")
            };
            Some((" :command (Enter run · Esc cancel) ", content))
        }
        Mode::Help => Some((" help (↑/↓ scroll · Esc close) ", help_text())),
    }
}

/// A live usage hint for the verb currently being typed in `:command` mode.
/// For `anim`, once a valid generator name is present it shows *that*
/// generator's params (with defaults) — the args the user actually needs.
fn command_hint(buf: &str) -> String {
    let toks: Vec<&str> = buf.split_whitespace().collect();
    let verb = toks.first().copied().unwrap_or("");
    match verb {
        "" => "verbs: add set del rename ro rw anim stop source write rate help quit".to_string(),
        "add" | "a" => "<name> <type> [ro|rw] <value>   type: i32 f64 bool string i32[] enum table".to_string(),
        "set" | "s" => "<name> <value>   enum: choice name or index".to_string(),
        "del" | "d" => "[name]   blank = selected row".to_string(),
        "rename" | "mv" => "<old> <new>   scalar/enum".to_string(),
        "ro" | "rw" => "<name>   set advertised access".to_string(),
        "anim" => match toks
            .get(2)
            .and_then(|t| anim::Generator::ALL.into_iter().find(|g| g.name() == *t))
        {
            Some(g) => format!("{} params: {}", g.name(), g.param_help()),
            None => "<name> <gen> [k=v ...]   gen: sine ramp triangle square noise walk count | cycle".to_string(),
        },
        "stop" => "[name]   blank = selected row".to_string(),
        "source" | "so" => "<file>   run a script of commands".to_string(),
        "write" | "w" => "<file>   dump the session as a loadable script".to_string(),
        "rate" => "<hz>   retune the animation tick (live)".to_string(),
        "help" | "h" | "quit" | "q" => String::new(),
        _ => "unknown verb — :help for the list".to_string(),
    }
}

fn help_text() -> String {
    let mut gens = String::from("Generators (params shown with defaults):\n");
    for g in anim::Generator::ALL {
        let note = if anim::is_enum_only(&g) { "   (enum only)" } else { "" };
        gens.push_str(&format!("  {:<9} {}{}\n", g.name(), g.param_help(), note));
    }
    format!(
        "\
Commands (prefix :) — shorthands in ( )
  add|a  <name> <type> [ro|rw] <value>   add PV(s)
  set|s  <name> <value>                  set value (choice name or index for enum)
  del|d  [name]                          delete (blank = selected row)
  rename|mv <old> <new>                  rename (scalar/enum)
  ro|rw  <name>                          set advertised access
  anim   <name> <gen> [k=v ...]          animate
  stop   [name]                          stop animation (blank = selected)
  source|so <file>                       run a script of commands
  write|w  <file>                        dump the session as a loadable script
  rate   <hz>                            retune the animation tick (live)
  help|h    quit|q

Types: bool int8 int16 int32(int) int64(long) uint8 uint16 uint32 uint64
       float(f32) double(f64) string(s) ; arrays: int32[]  ; enum ; table
Patterns: {{1..8}} {{8..1}} {{0..100..10}} {{01..12}} {{A,B,C}}  and products S{{1..4}}:{{A,B}}
{gens}  e.g. :anim RING:BPM{{01..99}} noise min=-1 max=1
Value forms: enum -> OFF,ON,TRIP 1   table -> id:i32=1,2,3 x:f64=0.5,1.5"
    )
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
            KeyCode::Char('?') => { app.help_scroll = 0; app.mode = Mode::Help; }
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
        Mode::Help => {
            // Clamp scroll to the help text's line count so it can't run off
            // into empty space on a tall terminal.
            let max = help_text().lines().count().saturating_sub(1) as u16;
            match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {}
                KeyCode::Down | KeyCode::Char('j') => {
                    app.help_scroll = (app.help_scroll + 1).min(max);
                    app.mode = Mode::Help;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.help_scroll = app.help_scroll.saturating_sub(1);
                    app.mode = Mode::Help;
                }
                KeyCode::PageDown => {
                    app.help_scroll = (app.help_scroll + 10).min(max);
                    app.mode = Mode::Help;
                }
                KeyCode::PageUp => {
                    app.help_scroll = app.help_scroll.saturating_sub(10);
                    app.mode = Mode::Help;
                }
                _ => app.mode = Mode::Help,
            }
        }
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
        help_scroll: 0,
    };
    let result = run_ui(terminal, app, &srv);
    ratatui::restore();
    srv.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn bare_app(mode: Mode) -> App {
        App {
            rows: Vec::new(),
            table: TableState::default(),
            mode,
            status: String::new(),
            tcp_port: 5075,
            udp_port: 5076,
            animators: Arc::new(Mutex::new(HashMap::new())),
            rate: Arc::new(AtomicU64::new(10.0_f64.to_bits())),
            help_scroll: 0,
        }
    }

    /// Render a bare `App` to an off-screen buffer and return its flat text.
    fn rendered(mode: Mode, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let app = bare_app(mode);
        terminal.draw(|f| draw(f, &app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn help_modal_shows_every_generator() {
        // The reported bug: not all generators were visible in `:help`.
        let text = rendered(Mode::Help, 100, 44);
        for g in ["sine", "ramp", "triangle", "square", "noise", "walk", "count", "cycle"] {
            assert!(text.contains(g), "help modal is missing generator {g:?}");
        }
        // and the value forms line below the generators is reachable too
        assert!(text.contains("Value forms"), "help modal truncates before value forms");
    }

    #[test]
    fn help_modal_tail_reachable_by_scroll_on_short_terminal() {
        // On a terminal too short to show all of help at once, scrolling must
        // still reach the tail (the fix for "generators not all visible").
        let mut terminal = Terminal::new(TestBackend::new(90, 16)).unwrap();
        let mut app = bare_app(Mode::Help);
        app.help_scroll = 18;
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Value forms"), "scroll should reveal the help tail");
    }

    #[test]
    fn command_hint_guides_each_verb() {
        // empty buffer lists the verbs
        assert!(command_hint("").contains("anim"));
        // add shows its arg shape
        assert!(command_hint("add").contains("<name>") && command_hint("a").contains("<type>"));
        // anim without a generator shows the generator list...
        assert!(command_hint("anim X").contains("sine"));
        // ...and once a generator is typed, shows THAT generator's params
        let h = command_hint("anim X sine");
        assert!(h.contains("amp=") && h.contains("offset="), "sine hint should show its params: {h}");
        assert!(command_hint("anim X noise").contains("min="));
        // write/w hint
        assert!(command_hint("w").contains("<file>"));
        // unknown verb is called out
        assert!(command_hint("frobnicate").contains("unknown"));
    }
}
