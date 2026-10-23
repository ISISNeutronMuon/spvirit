use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use argparse::{ArgumentParser, Store, StoreTrue};
use chrono::Local;
use get_if_addrs::{IfAddr, get_if_addrs};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Table,
    Wrap,
};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use spvirit_codec::epics_decode::{PvaPacket, PvaPacketCommand};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// Info about a single source that searched for a PV.
#[derive(Clone, Debug)]
struct SearcherInfo {
    addr: SocketAddr,
    count: u64,
    last_seen: Instant,
}

/// Info about a server that responded with a PV.
#[derive(Clone, Debug)]
struct ResponderInfo {
    addr: SocketAddr,
    guid: [u8; 12],
    count: u64,
    last_seen: Instant,
}

/// Aggregate state for a single PV name observed on the network.
#[derive(Clone, Debug)]
struct PvSearchEntry {
    pv_name: String,
    search_count: u64,
    found_count: u64,
    first_seen: Instant,
    last_searched: Option<Instant>,
    last_found: Option<Instant>,
    searchers: Vec<SearcherInfo>,
    responders: Vec<ResponderInfo>,
}

impl PvSearchEntry {
    fn new(pv_name: String, now: Instant) -> Self {
        Self {
            pv_name,
            search_count: 0,
            found_count: 0,
            first_seen: now,
            last_searched: None,
            last_found: None,
            searchers: Vec::new(),
            responders: Vec::new(),
        }
    }

    fn record_search(&mut self, addr: SocketAddr, now: Instant) {
        self.search_count += 1;
        self.last_searched = Some(now);
        if let Some(s) = self.searchers.iter_mut().find(|s| s.addr == addr) {
            s.count += 1;
            s.last_seen = now;
        } else {
            self.searchers.push(SearcherInfo {
                addr,
                count: 1,
                last_seen: now,
            });
        }
    }

    fn record_found(&mut self, addr: SocketAddr, guid: [u8; 12], now: Instant) {
        self.found_count += 1;
        self.last_found = Some(now);
        if let Some(r) = self.responders.iter_mut().find(|r| r.addr == addr) {
            r.count += 1;
            r.last_seen = now;
        } else {
            self.responders.push(ResponderInfo {
                addr,
                guid,
                count: 1,
                last_seen: now,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Worker thread communication
// ---------------------------------------------------------------------------

enum WorkerEvent {
    SearchSeen {
        pv_name: String,
        searcher: SocketAddr,
        ts: Instant,
    },
    FoundSeen {
        pv_name: String,
        responder: SocketAddr,
        guid: [u8; 12],
        ts: Instant,
    },
    Status(String),
    Error(String),
}

// ---------------------------------------------------------------------------
// UDP helpers (mirrors search.rs private helpers)
// ---------------------------------------------------------------------------

const PVA_MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 128);

fn bind_udp_reuse(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    // SO_REUSEADDR on both platforms: this is a broadcast/multicast receiver on
    // the well-known PVA port, which normally already has a local server bound
    // to it. Windows' unicast-hijack caveat does not apply to that case.
    sock.set_reuse_address(true)?;
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

fn join_multicast_v4(socket: &std::net::UdpSocket) {
    let ifaces = match get_if_addrs() {
        Ok(v) => v,
        Err(_) => return,
    };
    for iface in &ifaces {
        if let IfAddr::V4(v4) = &iface.addr {
            if !v4.ip.is_loopback() {
                let _ = socket.join_multicast_v4(&PVA_MULTICAST_V4, &v4.ip);
            }
        }
    }
}

fn decode_response_addr(addr: [u8; 16], port: u16, src: SocketAddr) -> SocketAddr {
    let ip = if addr[0..12] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF] {
        IpAddr::V4(Ipv4Addr::new(addr[12], addr[13], addr[14], addr[15]))
    } else {
        IpAddr::V6(addr.into())
    };
    let ip = if ip.is_unspecified() { src.ip() } else { ip };
    SocketAddr::new(ip, port)
}

// ---------------------------------------------------------------------------
// Worker thread — passive UDP listener
// ---------------------------------------------------------------------------

/// Cache to correlate search responses (which carry CIDs) back to PV names
/// observed in recent search requests.
struct CidCache {
    /// (seq, searcher_addr) → Vec<(cid, pv_name)>
    entries: HashMap<(u32, SocketAddr), (Instant, Vec<(u32, String)>)>,
    max_age: Duration,
}

impl CidCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            max_age: Duration::from_secs(30),
        }
    }

    fn insert(&mut self, seq: u32, searcher: SocketAddr, pvs: Vec<(u32, String)>) {
        self.entries.insert((seq, searcher), (Instant::now(), pvs));
    }

    fn lookup(&self, seq: u32, cids: &[u32]) -> Vec<String> {
        // Search responses echo the seq but come from the server, not the
        // original searcher.  We match on seq alone and scan all entries.
        let mut names = Vec::new();
        for ((entry_seq, _), (ts, pvs)) in &self.entries {
            if *entry_seq != seq {
                continue;
            }
            if ts.elapsed() > self.max_age {
                continue;
            }
            if cids.is_empty() {
                // Server discovery ping — no CID match needed
                continue;
            }
            for (cid, name) in pvs {
                if cids.contains(cid) {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    fn prune(&mut self) {
        self.entries
            .retain(|_, (ts, _)| ts.elapsed() < self.max_age);
    }
}

fn run_worker(udp_port: u16, bind_addr: Option<IpAddr>, evt_tx: Sender<WorkerEvent>) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            let _ = evt_tx.send(WorkerEvent::Error(format!(
                "failed to start tokio runtime: {}",
                err
            )));
            return;
        }
    };

    rt.block_on(async move {
        let bind_ip = bind_addr.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let bind = SocketAddr::new(bind_ip, udp_port);

        let std_sock = match bind_udp_reuse(bind) {
            Ok(s) => s,
            Err(err) => {
                let _ = evt_tx.send(WorkerEvent::Error(format!(
                    "failed to bind UDP {}:{} — {}",
                    bind_ip, udp_port, err
                )));
                return;
            }
        };

        if let Err(err) = std_sock.set_broadcast(true) {
            let _ = evt_tx.send(WorkerEvent::Error(format!(
                "failed to enable broadcast: {}",
                err
            )));
            return;
        }

        // Join multicast on all non-loopback interfaces
        join_multicast_v4(&std_sock);

        let socket = match UdpSocket::from_std(std_sock) {
            Ok(s) => s,
            Err(err) => {
                let _ = evt_tx.send(WorkerEvent::Error(format!(
                    "failed to convert socket: {}",
                    err
                )));
                return;
            }
        };

        let _ = evt_tx.send(WorkerEvent::Status(format!(
            "Listening on {} (multicast {})",
            bind, PVA_MULTICAST_V4,
        )));

        let mut buf = vec![0u8; 65535];
        let mut cid_cache = CidCache::new();
        let mut prune_timer = Instant::now();

        loop {
            let Ok((len, peer)) = socket.recv_from(&mut buf).await else {
                continue;
            };
            let data = &buf[..len];
            if data.len() < 8 {
                continue;
            }
            // Quick magic-byte guard
            if data[0] != 0xCA {
                continue;
            }

            let mut pkt = PvaPacket::new(data);
            let Some(cmd) = pkt.decode_payload() else {
                continue;
            };

            let now = Instant::now();

            match cmd {
                PvaPacketCommand::Search(payload) => {
                    // Cache CID→PV mapping for later response correlation
                    if !payload.pv_requests.is_empty() {
                        cid_cache.insert(payload.seq, peer, payload.pv_requests.clone());
                    }
                    // Emit events for each searched PV
                    for (_cid, name) in &payload.pv_requests {
                        let _ = evt_tx.send(WorkerEvent::SearchSeen {
                            pv_name: name.clone(),
                            searcher: peer,
                            ts: now,
                        });
                    }
                }
                PvaPacketCommand::SearchResponse(payload) => {
                    if !payload.found {
                        continue;
                    }
                    let server_addr = decode_response_addr(payload.addr, payload.port, peer);

                    let pv_names = cid_cache.lookup(payload.seq, &payload.cids);
                    if pv_names.is_empty() {
                        // Could not correlate CIDs — log as unknown
                        continue;
                    }
                    for name in pv_names {
                        let _ = evt_tx.send(WorkerEvent::FoundSeen {
                            pv_name: name,
                            responder: server_addr,
                            guid: payload.guid,
                            ts: now,
                        });
                    }
                }
                _ => {}
            }

            // Periodic cache cleanup
            if prune_timer.elapsed() > Duration::from_secs(10) {
                cid_cache.prune();
                prune_timer = Instant::now();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// TUI state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusPane {
    Table,
    Detail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortMode {
    ByName,
    BySearchCount,
    ByFoundCount,
    ByLastSearched,
    ByLastFound,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::ByName => SortMode::BySearchCount,
            SortMode::BySearchCount => SortMode::ByFoundCount,
            SortMode::ByFoundCount => SortMode::ByLastSearched,
            SortMode::ByLastSearched => SortMode::ByLastFound,
            SortMode::ByLastFound => SortMode::ByName,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortMode::ByName => "Name",
            SortMode::BySearchCount => "Searches",
            SortMode::ByFoundCount => "Found",
            SortMode::ByLastSearched => "Last Searched",
            SortMode::ByLastFound => "Last Found",
        }
    }
}

struct SearchApp {
    entries: HashMap<String, PvSearchEntry>,
    sorted_keys: Vec<String>,
    selected_index: usize,
    selected_pv: Option<String>,
    focus: FocusPane,
    sort_mode: SortMode,
    filter: String,
    filter_input: String,
    filter_editing: bool,
    show_help: bool,
    paused: bool,
    detail_scroll: u16,
    status: String,
    status_log: VecDeque<String>,
    last_error: Option<String>,
    total_search_events: u64,
    total_found_events: u64,
    start_time: Instant,
}

impl SearchApp {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            sorted_keys: Vec::new(),
            selected_index: 0,
            selected_pv: None,
            focus: FocusPane::Table,
            sort_mode: SortMode::BySearchCount,
            filter: String::new(),
            filter_input: String::new(),
            filter_editing: false,
            show_help: false,
            paused: false,
            detail_scroll: 0,
            status: "Listening for PVA search traffic...".to_string(),
            status_log: VecDeque::from(vec!["Listening for PVA search traffic...".to_string()]),
            last_error: None,
            total_search_events: 0,
            total_found_events: 0,
            start_time: Instant::now(),
        }
    }

    fn push_status<S: Into<String>>(&mut self, message: S) {
        let entry = format!("[{}] {}", Local::now().format("%H:%M:%S"), message.into());
        self.status = entry.clone();
        self.status_log.push_front(entry);
        while self.status_log.len() > 4 {
            self.status_log.pop_back();
        }
    }

    fn handle_event(&mut self, evt: WorkerEvent) {
        if self.paused {
            // Still count events even if paused
            match &evt {
                WorkerEvent::SearchSeen { .. } => self.total_search_events += 1,
                WorkerEvent::FoundSeen { .. } => self.total_found_events += 1,
                _ => {}
            }
            return;
        }
        match evt {
            WorkerEvent::SearchSeen {
                pv_name,
                searcher,
                ts,
            } => {
                self.total_search_events += 1;
                let entry = self
                    .entries
                    .entry(pv_name.clone())
                    .or_insert_with(|| PvSearchEntry::new(pv_name, ts));
                entry.record_search(searcher, ts);
            }
            WorkerEvent::FoundSeen {
                pv_name,
                responder,
                guid,
                ts,
            } => {
                self.total_found_events += 1;
                let entry = self
                    .entries
                    .entry(pv_name.clone())
                    .or_insert_with(|| PvSearchEntry::new(pv_name, ts));
                entry.record_found(responder, guid, ts);
            }
            WorkerEvent::Status(msg) => self.push_status(msg),
            WorkerEvent::Error(msg) => {
                self.last_error = Some(msg.clone());
                self.push_status(format!("ERROR: {}", msg));
            }
        }
    }

    fn rebuild_sorted_keys(&mut self) {
        let needle = self.filter.to_ascii_lowercase();
        let mut keys: Vec<String> = self
            .entries
            .keys()
            .filter(|k| needle.is_empty() || k.to_ascii_lowercase().contains(&needle))
            .cloned()
            .collect();

        let entries = &self.entries;
        match self.sort_mode {
            SortMode::ByName => keys.sort(),
            SortMode::BySearchCount => {
                keys.sort_by(|a, b| {
                    let ea = &entries[a];
                    let eb = &entries[b];
                    eb.search_count.cmp(&ea.search_count).then(a.cmp(b))
                });
            }
            SortMode::ByFoundCount => {
                keys.sort_by(|a, b| {
                    let ea = &entries[a];
                    let eb = &entries[b];
                    eb.found_count.cmp(&ea.found_count).then(a.cmp(b))
                });
            }
            SortMode::ByLastSearched => {
                keys.sort_by(|a, b| {
                    let ea = &entries[a];
                    let eb = &entries[b];
                    eb.last_searched.cmp(&ea.last_searched).then(a.cmp(b))
                });
            }
            SortMode::ByLastFound => {
                keys.sort_by(|a, b| {
                    let ea = &entries[a];
                    let eb = &entries[b];
                    eb.last_found.cmp(&ea.last_found).then(a.cmp(b))
                });
            }
        }

        self.sorted_keys = keys;

        // Preserve selection by PV name across re-sorts
        if let Some(ref name) = self.selected_pv {
            if let Some(pos) = self.sorted_keys.iter().position(|k| k == name) {
                self.selected_index = pos;
            } else if self.selected_index >= self.sorted_keys.len() {
                self.selected_index = self.sorted_keys.len().saturating_sub(1);
            }
        } else if self.selected_index >= self.sorted_keys.len() {
            self.selected_index = self.sorted_keys.len().saturating_sub(1);
        }

        // Sync selected_pv to current index
        self.selected_pv = self.sorted_keys.get(self.selected_index).cloned();
    }

    fn selected_entry(&self) -> Option<&PvSearchEntry> {
        self.sorted_keys
            .get(self.selected_index)
            .and_then(|k| self.entries.get(k))
    }

    fn start_filter_input(&mut self) {
        self.filter_input = self.filter.clone();
        self.filter_editing = true;
        self.push_status("Filter input started (Enter to apply, Esc to cancel)");
    }

    fn apply_filter_input(&mut self) {
        self.filter = self.filter_input.clone();
        self.filter_editing = false;
        self.rebuild_sorted_keys();
        if self.filter.is_empty() {
            self.push_status("Filter cleared");
        } else {
            self.push_status(format!("Applied filter '{}'", self.filter));
        }
    }

    fn move_up(&mut self) {
        match self.focus {
            FocusPane::Table => {
                if !self.sorted_keys.is_empty() {
                    if self.selected_index == 0 {
                        self.selected_index = self.sorted_keys.len() - 1;
                    } else {
                        self.selected_index -= 1;
                    }
                    self.selected_pv = self.sorted_keys.get(self.selected_index).cloned();
                    self.detail_scroll = 0;
                }
            }
            FocusPane::Detail => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
            }
        }
    }

    fn move_down(&mut self) {
        match self.focus {
            FocusPane::Table => {
                if !self.sorted_keys.is_empty() {
                    self.selected_index = (self.selected_index + 1) % self.sorted_keys.len();
                    self.selected_pv = self.sorted_keys.get(self.selected_index).cloned();
                    self.detail_scroll = 0;
                }
            }
            FocusPane::Detail => {
                self.detail_scroll = self.detail_scroll.saturating_add(1);
            }
        }
    }

    fn clear_stale(&mut self, max_age: Duration) {
        let now = Instant::now();
        self.entries.retain(|_, e| {
            let last_activity = e
                .last_searched
                .unwrap_or(e.first_seen)
                .max(e.last_found.unwrap_or(e.first_seen));
            now.duration_since(last_activity) < max_age
        });
        self.rebuild_sorted_keys();
        self.push_status(format!(
            "Cleared stale entries (>{:.0}s), {} remaining",
            max_age.as_secs_f64(),
            self.entries.len()
        ));
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn format_ago(instant: Option<Instant>, now: Instant) -> String {
    match instant {
        Some(t) => {
            let elapsed = now.duration_since(t);
            format!("{} ago", format_elapsed(elapsed))
        }
        None => "never".to_string(),
    }
}

fn focus_block(title: String, focused: bool) -> Block<'static> {
    let border_style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style)
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &SearchApp) {
    let now = Instant::now();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(5)])
        .split(frame.area());

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(outer[0]);

    // --- Left pane: PV search table ---
    let header_row = Row::new(vec![
        "PV Name",
        "Searches",
        "Found",
        "Last Searched",
        "Last Found",
    ])
    .style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );

    let table_height = main[0].height.saturating_sub(3) as usize; // borders + header
    let scroll_offset = if table_height == 0 {
        0
    } else {
        app.selected_index
            .saturating_sub(table_height.saturating_sub(1))
    };

    let rows: Vec<Row> = app
        .sorted_keys
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(table_height)
        .map(|(idx, key)| {
            let entry = &app.entries[key];
            let style = if idx == app.selected_index {
                if app.focus == FocusPane::Table {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                }
            } else if entry.found_count > 0 {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };

            Row::new(vec![
                entry.pv_name.clone(),
                entry.search_count.to_string(),
                entry.found_count.to_string(),
                format_ago(entry.last_searched, now),
                format_ago(entry.last_found, now),
            ])
            .style(style)
        })
        .collect();

    let filter_display = if app.filter_editing {
        format!(" [typing: {}_]", app.filter_input)
    } else if app.filter.is_empty() {
        String::new()
    } else {
        format!(" [filter: {}]", app.filter)
    };

    let pause_display = if app.paused { " [PAUSED]" } else { "" };

    let table_title = format!(
        "PV Searches ({} PVs) [sort: {}]{}{}",
        app.sorted_keys.len(),
        app.sort_mode.label(),
        filter_display,
        pause_display,
    );

    let widths = [
        Constraint::Min(20),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(14),
        Constraint::Length(14),
    ];

    let table = Table::new(rows, widths)
        .header(header_row)
        .block(focus_block(
            table_title,
            app.focus == FocusPane::Table && !app.show_help,
        ));
    frame.render_widget(table, main[0]);

    // --- Scrollbar for table ---
    if app.sorted_keys.len() > table_height {
        let mut scrollbar_state =
            ScrollbarState::new(app.sorted_keys.len()).position(app.selected_index);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        // Render inside the table block's inner area
        let scrollbar_area = Rect {
            x: main[0].x + main[0].width.saturating_sub(1),
            y: main[0].y + 1,
            width: 1,
            height: main[0].height.saturating_sub(2),
        };
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    // --- Right pane: Detail panel ---
    let mut detail_lines: Vec<Line> = Vec::new();

    if let Some(entry) = app.selected_entry() {
        detail_lines.push(Line::from(vec![
            Span::styled("PV: ", Style::default().fg(Color::Cyan)),
            Span::raw(&entry.pv_name),
        ]));
        detail_lines.push(Line::from(format!(
            "First seen: {} ago",
            format_elapsed(now.duration_since(entry.first_seen))
        )));
        detail_lines.push(Line::from(format!(
            "Total searches: {}",
            entry.search_count
        )));
        detail_lines.push(Line::from(format!("Total found: {}", entry.found_count)));
        detail_lines.push(Line::from(format!(
            "Last searched: {}",
            format_ago(entry.last_searched, now)
        )));
        detail_lines.push(Line::from(format!(
            "Last found: {}",
            format_ago(entry.last_found, now)
        )));
        detail_lines.push(Line::from(""));

        // Searchers sub-table
        detail_lines.push(Line::from(Span::styled(
            format!("Searchers ({})", entry.searchers.len()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        if entry.searchers.is_empty() {
            detail_lines.push(Line::from("  (none)"));
        } else {
            detail_lines.push(Line::from(Span::styled(
                "  Address               Count  Last Seen",
                Style::default().fg(Color::DarkGray),
            )));
            let mut sorted_searchers = entry.searchers.clone();
            sorted_searchers.sort_by(|a, b| b.count.cmp(&a.count));
            for s in &sorted_searchers {
                detail_lines.push(Line::from(format!(
                    "  {:<22} {:<6} {}",
                    s.addr,
                    s.count,
                    format_ago(Some(s.last_seen), now),
                )));
            }
        }
        detail_lines.push(Line::from(""));

        // Responders sub-table
        detail_lines.push(Line::from(Span::styled(
            format!("Responders ({})", entry.responders.len()),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
        if entry.responders.is_empty() {
            detail_lines.push(Line::from("  (none)"));
        } else {
            detail_lines.push(Line::from(Span::styled(
                "  Address               GUID         Count  Last Seen",
                Style::default().fg(Color::DarkGray),
            )));
            let mut sorted_responders = entry.responders.clone();
            sorted_responders.sort_by(|a, b| b.count.cmp(&a.count));
            for r in &sorted_responders {
                let guid_short = &hex::encode(r.guid)[..8];
                detail_lines.push(Line::from(format!(
                    "  {:<22} {:<12} {:<6} {}",
                    r.addr,
                    guid_short,
                    r.count,
                    format_ago(Some(r.last_seen), now),
                )));
            }
        }
    } else {
        detail_lines.push(Line::from("No PV selected"));
        detail_lines.push(Line::from(""));
        detail_lines.push(Line::from("Use Up/Down to select a PV from the table."));
    }

    let detail_widget = Paragraph::new(Text::from(detail_lines))
        .block(focus_block(
            "Detail".to_string(),
            app.focus == FocusPane::Detail && !app.show_help,
        ))
        .scroll((app.detail_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(detail_widget, main[1]);

    // --- Bottom: Status bar ---
    let uptime = format_elapsed(now.duration_since(app.start_time));
    let header = format!(
        "uptime: {} | PVs: {} | searches: {} | found: {} | state: {}",
        uptime,
        app.entries.len(),
        app.total_search_events,
        app.total_found_events,
        if app.paused { "paused" } else { "running" },
    );
    let latest = &app.status;
    let status_widget = Paragraph::new(format!("{header}\n{latest}"))
        .block(Block::default().borders(Borders::ALL).title("Status"))
        .wrap(Wrap { trim: false });
    frame.render_widget(status_widget, outer[1]);

    // --- Help overlay ---
    if app.show_help {
        let area = centered_rect(60, 60, frame.area());
        let help = Paragraph::new(
            "Key Bindings\n\
\n\
q       : quit\n\
h       : toggle help\n\
Tab     : cycle focus (table ↔ detail)\n\
Up/Down : navigate\n\
/       : filter PV names (Enter applies, Esc cancels)\n\
s       : cycle sort mode\n\
p       : pause/resume updates\n\
c       : clear stale entries (>5min)\n\
\n\
The tool passively listens on the PVA UDP search\n\
multicast group and displays all PV names being\n\
searched for and found on the network.\n\
\n\
Green rows = PV has been found by at least one server.\n\
Detail panel shows who searched and who responded.\n",
        )
        .block(Block::default().title("Help").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
        frame.render_widget(Clear, area);
        frame.render_widget(help, area);
    }
}

// ---------------------------------------------------------------------------
// TUI event loop
// ---------------------------------------------------------------------------

fn run_ui(
    mut terminal: DefaultTerminal,
    mut app: SearchApp,
    evt_rx: Receiver<WorkerEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let tick_rate = Duration::from_millis(100);

    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        // Drain worker events
        while let Ok(evt) = evt_rx.try_recv() {
            app.handle_event(evt);
        }
        app.rebuild_sorted_keys();

        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                // Filter editing mode
                if app.filter_editing {
                    match key.code {
                        KeyCode::Enter => app.apply_filter_input(),
                        KeyCode::Backspace => {
                            app.filter_input.pop();
                        }
                        KeyCode::Esc => {
                            app.filter_editing = false;
                            app.filter_input = app.filter.clone();
                            app.push_status("Filter input cancelled");
                        }
                        KeyCode::Char(c) => app.filter_input.push(c),
                        _ => {}
                    }
                    continue;
                }

                // Normal mode
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Esc => {
                        if app.show_help {
                            app.show_help = false;
                        }
                    }
                    KeyCode::Char('h') => app.show_help = !app.show_help,
                    KeyCode::Char('/') => app.start_filter_input(),
                    KeyCode::Char('s') => {
                        app.sort_mode = app.sort_mode.next();
                        app.rebuild_sorted_keys();
                        app.push_status(format!("Sort mode: {}", app.sort_mode.label()));
                    }
                    KeyCode::Tab => {
                        app.focus = match app.focus {
                            FocusPane::Table => FocusPane::Detail,
                            FocusPane::Detail => FocusPane::Table,
                        };
                    }
                    KeyCode::Char('p') => {
                        app.paused = !app.paused;
                        app.push_status(if app.paused {
                            "Updates paused"
                        } else {
                            "Updates resumed"
                        });
                    }
                    KeyCode::Char('c') => {
                        app.clear_stale(Duration::from_secs(300));
                    }
                    KeyCode::Up => app.move_up(),
                    KeyCode::Down => app.move_down(),
                    KeyCode::PageUp => {
                        for _ in 0..10 {
                            app.move_up();
                        }
                    }
                    KeyCode::PageDown => {
                        for _ in 0..10 {
                            app.move_down();
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut udp_port: u16 = 5076;
    let mut bind_addr = String::new();
    let mut debug = false;

    {
        let mut ap = ArgumentParser::new();
        ap.set_description(
            "Passive PVA search monitor — listens for PVA search requests and \
             responses on the network and displays observed PV names in a TUI.",
        );
        ap.refer(&mut udp_port).add_option(
            &["--udp-port", "-p"],
            Store,
            "UDP search port (default 5076)",
        );
        ap.refer(&mut bind_addr).add_option(
            &["--bind-addr", "-b"],
            Store,
            "Local bind IP for UDP listener",
        );
        ap.refer(&mut debug)
            .add_option(&["--debug", "-d"], StoreTrue, "Enable debug logging");
        ap.parse_args_or_exit();
    }

    let max_level = if debug {
        tracing::Level::DEBUG
    } else {
        tracing::Level::WARN
    };
    tracing_subscriber::fmt().with_max_level(max_level).init();

    let bind_ip = if bind_addr.trim().is_empty() {
        None
    } else {
        Some(bind_addr.parse::<IpAddr>()?)
    };

    // Spawn worker thread
    let (evt_tx, evt_rx) = mpsc::channel();
    let worker_handle = thread::spawn(move || run_worker(udp_port, bind_ip, evt_tx));

    // Start TUI
    color_eyre::install()?;
    let terminal = ratatui::init();
    let app = SearchApp::new();
    let ui_result = run_ui(terminal, app, evt_rx);
    ratatui::restore();

    drop(worker_handle);
    ui_result
}

#[cfg(test)]
mod tests {
    use super::bind_udp_reuse;
    use std::net::SocketAddr;

    #[test]
    fn bind_udp_reuse_sets_so_reuseaddr() {
        let addr: SocketAddr = "0.0.0.0:0".parse().expect("addr");
        let sock = bind_udp_reuse(addr).expect("bind");
        assert!(sock.local_addr().expect("local addr").port() > 0);
        assert!(
            socket2::SockRef::from(&sock)
                .reuse_address()
                .expect("read SO_REUSEADDR"),
            "SO_REUSEADDR must be set on every platform"
        );
    }

    /// Windows-only: SO_REUSEADDR permits a second bind to the same addr:port.
    /// On Unix that needs SO_REUSEPORT, so this is not asserted there.
    #[cfg(windows)]
    #[test]
    fn two_binds_share_the_same_port_on_windows() {
        let first = bind_udp_reuse("0.0.0.0:0".parse().expect("addr")).expect("first bind");
        let port = first.local_addr().expect("local addr").port();
        let second = bind_udp_reuse(format!("0.0.0.0:{port}").parse().expect("addr"));
        assert!(second.is_ok(), "second bind to {port} failed: {:?}", second.err());
    }
}
