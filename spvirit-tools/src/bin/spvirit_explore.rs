use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use argparse::{ArgumentParser, Store};
use chrono::Local;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, RenderDirection, Sparkline, Wrap};

use spvirit_codec::spvd_decode::{
    DecodedValue, extract_nt_scalar_value, format_compact_value, format_structure_desc,
    format_structure_tree,
};
use spvirit_tools::spvirit_client::cli::CommonClientArgs;
use spvirit_tools::spvirit_client::explore::{
    PvListSource, list_pvs_with_fallback_progress, monitor_pv_from_server,
};
use spvirit_tools::spvirit_client::format::{RenderOptions, format_output};
use spvirit_tools::spvirit_client::search::{
    DiscoveredServer, SearchTarget, build_search_targets, discover_servers,
};
use spvirit_tools::spvirit_client::types::{PvGetOptions, PvGetResult};
use spvirit_tools::tui::{centered_rect, focus_block};

#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusPane {
    Servers,
    Pvs,
    Details,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailsView {
    Text,
    Chart,
}

const CHART_MAX_SAMPLES: usize = 240;
const CHART_SCALE_MAX: u64 = 100;

enum WorkerCommand {
    Discover {
        request_id: u64,
    },
    LoadPvList {
        request_id: u64,
        server: DiscoveredServer,
    },
    FetchSnapshot {
        request_id: u64,
        server: DiscoveredServer,
        pv: String,
    },
    Cancel {
        op: WorkerOp,
        request_id: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerOp {
    Discover,
    List,
    Snapshot,
}

enum WorkerEvent {
    DiscoverDone {
        request_id: u64,
        result: Result<Vec<DiscoveredServer>, String>,
    },
    PvListDone {
        request_id: u64,
        server: DiscoveredServer,
        result: Result<(Vec<String>, PvListSource), String>,
    },
    SnapshotDone {
        request_id: u64,
        server: DiscoveredServer,
        pv: String,
        result: Result<PvGetResult, String>,
    },
    SnapshotStopped {
        request_id: u64,
        server: DiscoveredServer,
        pv: String,
        message: String,
    },
    Progress {
        op: WorkerOp,
        request_id: u64,
        message: String,
    },
}

#[derive(Clone)]
struct WorkerConfig {
    opts: PvGetOptions,
    search_targets: Vec<SearchTarget>,
}

fn resolve_server_addr(server: DiscoveredServer, fallback_tcp_port: u16) -> SocketAddr {
    if server.tcp_addr.port() == 0 {
        SocketAddr::new(server.tcp_addr.ip(), fallback_tcp_port)
    } else {
        server.tcp_addr
    }
}

fn run_worker(config: WorkerConfig, cmd_rx: Receiver<WorkerCommand>, evt_tx: Sender<WorkerEvent>) {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            let _ = evt_tx.send(WorkerEvent::DiscoverDone {
                request_id: 0,
                result: Err(format!("failed to start tokio runtime: {}", err)),
            });
            return;
        }
    };
    let handle = rt.handle().clone();
    let mut discover_task: Option<(u64, tokio::task::JoinHandle<()>)> = None;
    let mut list_task: Option<(u64, tokio::task::JoinHandle<()>)> = None;
    let mut snapshot_task: Option<(u64, tokio::task::JoinHandle<()>)> = None;

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            WorkerCommand::Discover { request_id } => {
                if let Some((_, task)) = discover_task.take() {
                    task.abort();
                }
                let opts = config.opts.clone();
                let targets = config.search_targets.clone();
                let evt_tx_task = evt_tx.clone();
                discover_task = Some((
                    request_id,
                    handle.spawn(async move {
                        let _ = evt_tx_task.send(WorkerEvent::Progress {
                            op: WorkerOp::Discover,
                            request_id,
                            message: format!(
                                "Discovering servers using {} search target(s)...",
                                targets.len()
                            ),
                        });
                        let result =
                            discover_servers(opts.udp_port, opts.timeout, &targets, opts.debug)
                                .await
                                .map_err(|e| e.to_string());
                        let _ = evt_tx_task.send(WorkerEvent::DiscoverDone { request_id, result });
                    }),
                ));
            }
            WorkerCommand::LoadPvList { request_id, server } => {
                if let Some((_, task)) = list_task.take() {
                    task.abort();
                }
                let opts = config.opts.clone();
                let server_addr = resolve_server_addr(server, opts.tcp_port);
                let evt_tx_task = evt_tx.clone();
                list_task = Some((
                    request_id,
                    handle.spawn(async move {
                        let _ = evt_tx_task.send(WorkerEvent::Progress {
                            op: WorkerOp::List,
                            request_id,
                            message: format!("Loading PV list from {}...", server_addr),
                        });
                        let evt_progress = evt_tx_task.clone();
                        let result =
                            list_pvs_with_fallback_progress(&opts, server_addr, move |msg| {
                                let _ = evt_progress.send(WorkerEvent::Progress {
                                    op: WorkerOp::List,
                                    request_id,
                                    message: msg.to_string(),
                                });
                            })
                            .await
                            .map_err(|e| e.to_string());
                        let _ = evt_tx_task.send(WorkerEvent::PvListDone {
                            request_id,
                            server,
                            result,
                        });
                    }),
                ));
            }
            WorkerCommand::FetchSnapshot {
                request_id,
                server,
                pv,
            } => {
                if let Some((_, task)) = snapshot_task.take() {
                    task.abort();
                }
                let opts = config.opts.clone();
                let server_addr = resolve_server_addr(server, opts.tcp_port);
                let evt_tx_task = evt_tx.clone();
                snapshot_task = Some((
                    request_id,
                    handle.spawn(async move {
                        let _ = evt_tx_task.send(WorkerEvent::Progress {
                            op: WorkerOp::Snapshot,
                            request_id,
                            message: format!("Starting monitor for {} on {}...", pv, server_addr),
                        });
                        let evt_updates = evt_tx_task.clone();
                        let pv_for_events = pv.clone();
                        let result = monitor_pv_from_server(
                            &opts,
                            server_addr,
                            &pv,
                            |snapshot| {
                                let _ = evt_updates.send(WorkerEvent::SnapshotDone {
                                    request_id,
                                    server,
                                    pv: pv_for_events.clone(),
                                    result: Ok(snapshot),
                                });
                            },
                            |msg| {
                                let _ = evt_tx_task.send(WorkerEvent::Progress {
                                    op: WorkerOp::Snapshot,
                                    request_id,
                                    message: msg.to_string(),
                                });
                            },
                        )
                        .await;

                        match result {
                            Ok(()) => {
                                let _ = evt_tx_task.send(WorkerEvent::SnapshotStopped {
                                    request_id,
                                    server,
                                    pv,
                                    message: "Monitor stopped by server".to_string(),
                                });
                            }
                            Err(err) => {
                                let _ = evt_tx_task.send(WorkerEvent::SnapshotDone {
                                    request_id,
                                    server,
                                    pv,
                                    result: Err(err.to_string()),
                                });
                            }
                        }
                    }),
                ));
            }
            WorkerCommand::Cancel { op, request_id } => match op {
                WorkerOp::Discover => {
                    if let Some((active_id, task)) = discover_task.take() {
                        if request_id == 0 || request_id == active_id {
                            task.abort();
                            let _ = evt_tx.send(WorkerEvent::Progress {
                                op: WorkerOp::Discover,
                                request_id: active_id,
                                message: "Cancelled discovery".to_string(),
                            });
                        } else {
                            discover_task = Some((active_id, task));
                        }
                    }
                }
                WorkerOp::List => {
                    if let Some((active_id, task)) = list_task.take() {
                        if request_id == 0 || request_id == active_id {
                            task.abort();
                            let _ = evt_tx.send(WorkerEvent::Progress {
                                op: WorkerOp::List,
                                request_id: active_id,
                                message: "Cancelled PV list loading".to_string(),
                            });
                        } else {
                            list_task = Some((active_id, task));
                        }
                    }
                }
                WorkerOp::Snapshot => {
                    if let Some((active_id, task)) = snapshot_task.take() {
                        if request_id == 0 || request_id == active_id {
                            task.abort();
                            let _ = evt_tx.send(WorkerEvent::Progress {
                                op: WorkerOp::Snapshot,
                                request_id: active_id,
                                message: "Cancelled monitor stream".to_string(),
                            });
                        } else {
                            snapshot_task = Some((active_id, task));
                        }
                    }
                }
            },
        }
    }

    if let Some((_, task)) = discover_task {
        task.abort();
    }
    if let Some((_, task)) = list_task {
        task.abort();
    }
    if let Some((_, task)) = snapshot_task {
        task.abort();
    }
}

struct ExploreApp {
    focus: FocusPane,
    servers: Vec<DiscoveredServer>,
    server_index: usize,
    selected_server: Option<DiscoveredServer>,
    all_pvs: Vec<String>,
    pvs: Vec<String>,
    pv_index: usize,
    selected_pv: Option<String>,
    pv_filter: String,
    filter_input: String,
    filter_editing: bool,
    add_pv_input: String,
    add_pv_editing: bool,
    details_view: DetailsView,
    list_source: Option<PvListSource>,
    last_snapshot: Option<PvGetResult>,
    chart_samples: VecDeque<f64>,
    discover_in_flight: bool,
    list_in_flight: bool,
    snapshot_in_flight: bool,
    poll_paused: bool,
    details_scroll: u16,
    show_help: bool,
    status: String,
    status_log: VecDeque<String>,
    last_error: Option<String>,
    last_update: Option<String>,
    list_progress: Option<String>,
    next_request_id: u64,
    discover_request_id: u64,
    list_request_id: u64,
    snapshot_request_id: u64,
}

impl ExploreApp {
    fn new() -> Self {
        Self {
            focus: FocusPane::Servers,
            servers: Vec::new(),
            server_index: 0,
            selected_server: None,
            all_pvs: Vec::new(),
            pvs: Vec::new(),
            pv_index: 0,
            selected_pv: None,
            pv_filter: String::new(),
            filter_input: String::new(),
            filter_editing: false,
            add_pv_input: String::new(),
            add_pv_editing: false,
            details_view: DetailsView::Text,
            list_source: None,
            last_snapshot: None,
            chart_samples: VecDeque::new(),
            discover_in_flight: false,
            list_in_flight: false,
            snapshot_in_flight: false,
            poll_paused: false,
            details_scroll: 0,
            show_help: false,
            status: "Press r to discover servers".to_string(),
            status_log: VecDeque::from(vec!["Press r to discover servers".to_string()]),
            last_error: None,
            last_update: None,
            list_progress: None,
            next_request_id: 1,
            discover_request_id: 0,
            list_request_id: 0,
            snapshot_request_id: 0,
        }
    }

    fn next_request(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        id
    }

    fn apply_pv_filter(&mut self) {
        let needle = self.pv_filter.to_ascii_lowercase();
        if needle.is_empty() {
            self.pvs = self.all_pvs.clone();
        } else {
            self.pvs = self
                .all_pvs
                .iter()
                .filter(|pv| pv.to_ascii_lowercase().contains(&needle))
                .cloned()
                .collect();
        }
        if self.pv_index >= self.pvs.len() {
            self.pv_index = self.pvs.len().saturating_sub(1);
        }
        if let Some(selected) = self.selected_pv.clone()
            && !self.pvs.contains(&selected) {
                self.selected_pv = None;
                self.last_snapshot = None;
                self.snapshot_in_flight = false;
                self.clear_chart_samples();
            }
    }

    fn start_filter_input(&mut self) {
        self.add_pv_editing = false;
        self.filter_input = self.pv_filter.clone();
        self.filter_editing = true;
        self.push_status("PV filter input started (Enter to apply)");
    }

    fn apply_filter_input(&mut self) {
        self.pv_filter = self.filter_input.clone();
        self.filter_editing = false;
        self.apply_pv_filter();
        if self.pv_filter.is_empty() {
            self.push_status("PV filter cleared");
        } else {
            self.push_status(format!("Applied PV filter '{}'", self.pv_filter));
        }
    }

    fn start_add_pv_input(&mut self) {
        self.filter_editing = false;
        self.add_pv_input.clear();
        self.add_pv_editing = true;
        self.push_status("Add PV input started (Enter to add)");
    }

    fn apply_add_pv_input(&mut self, tx: &Sender<WorkerCommand>) {
        let pv = self.add_pv_input.trim().to_string();
        self.add_pv_editing = false;
        self.add_pv_input.clear();
        if pv.is_empty() {
            self.push_status("Add PV cancelled (empty input)");
            return;
        }

        if !self.all_pvs.contains(&pv) {
            self.all_pvs.push(pv.clone());
            self.all_pvs.sort();
            self.all_pvs.dedup();
        }
        self.apply_pv_filter();

        if let Some(idx) = self.pvs.iter().position(|name| name == &pv) {
            self.pv_index = idx;
        }
        let pv_changed = self.selected_pv.as_deref() != Some(pv.as_str());
        self.selected_pv = Some(pv.clone());
        if pv_changed {
            self.last_snapshot = None;
            self.last_update = None;
            self.clear_chart_samples();
        }

        if let Some(server) = self.selected_server {
            self.issue_snapshot(tx, server, &pv);
            self.push_status(format!("Added PV '{}' and started monitor", pv));
        } else {
            self.push_status(format!(
                "Added PV '{}'; select a server before monitoring",
                pv
            ));
        }
    }

    fn clear_chart_samples(&mut self) {
        self.chart_samples.clear();
    }

    fn push_chart_sample(&mut self, sample: f64) {
        if !sample.is_finite() {
            return;
        }
        if self.chart_samples.len() >= CHART_MAX_SAMPLES {
            self.chart_samples.pop_front();
        }
        self.chart_samples.push_back(sample);
    }

    fn chart_is_available(&self) -> bool {
        !self.chart_samples.is_empty()
            || self
                .last_snapshot
                .as_ref()
                .and_then(|s| chart_sample_from_value(&s.value))
                .is_some()
    }

    fn toggle_details_view(&mut self) {
        match self.details_view {
            DetailsView::Text => {
                if self.chart_is_available() {
                    self.details_view = DetailsView::Chart;
                    self.push_status("Details view set to chart");
                } else {
                    self.push_status(
                        "Chart view unavailable (select numeric scalar PV and wait for update)",
                    );
                }
            }
            DetailsView::Chart => {
                self.details_view = DetailsView::Text;
                self.push_status("Details view set to text");
            }
        }
    }

    fn selected_server_addr(&self) -> Option<SocketAddr> {
        self.selected_server.map(|s| s.tcp_addr)
    }

    fn push_status<S: Into<String>>(&mut self, message: S) {
        let entry = format!("[{}] {}", Local::now().format("%H:%M:%S"), message.into());
        self.status = entry.clone();
        self.status_log.push_front(entry);
        while self.status_log.len() > 4 {
            self.status_log.pop_back();
        }
    }

    fn sanitize_for_status(message: &str) -> String {
        message
            .replace(['\r', '\n', '\t'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn issue_discover(&mut self, tx: &Sender<WorkerCommand>) {
        let request_id = self.next_request();
        self.discover_request_id = request_id;
        self.discover_in_flight = true;
        self.push_status("Discovering servers...");
        self.last_error = None;
        let _ = tx.send(WorkerCommand::Discover { request_id });
    }

    fn issue_load_pv_list(&mut self, tx: &Sender<WorkerCommand>, server: DiscoveredServer) {
        if self.list_in_flight {
            let _ = tx.send(WorkerCommand::Cancel {
                op: WorkerOp::List,
                request_id: self.list_request_id,
            });
        }
        let request_id = self.next_request();
        self.list_request_id = request_id;
        self.list_in_flight = true;
        self.list_progress = Some(format!("queued list request to {}", server.tcp_addr));
        self.push_status(format!("Loading PV list from {}...", server.tcp_addr));
        self.last_error = None;
        let _ = tx.send(WorkerCommand::LoadPvList { request_id, server });
    }

    fn issue_snapshot(&mut self, tx: &Sender<WorkerCommand>, server: DiscoveredServer, pv: &str) {
        if self.poll_paused {
            self.push_status("Monitor is paused (press p to resume)");
            return;
        }
        if self.snapshot_in_flight {
            let _ = tx.send(WorkerCommand::Cancel {
                op: WorkerOp::Snapshot,
                request_id: self.snapshot_request_id,
            });
        }
        let request_id = self.next_request();
        self.snapshot_request_id = request_id;
        self.snapshot_in_flight = true;
        self.push_status(format!(
            "Starting monitor for {} on {}...",
            pv, server.tcp_addr
        ));
        self.last_error = None;
        let _ = tx.send(WorkerCommand::FetchSnapshot {
            request_id,
            server,
            pv: pv.to_string(),
        });
    }

    fn manual_refresh(&mut self, tx: &Sender<WorkerCommand>) {
        self.issue_discover(tx);
        if let Some(server) = self.selected_server {
            self.issue_load_pv_list(tx, server);
        }
        if let (Some(server), Some(pv)) = (self.selected_server, self.selected_pv.clone()) {
            self.issue_snapshot(tx, server, &pv);
        }
    }

    fn cancel_in_flight(&mut self, tx: &Sender<WorkerCommand>) {
        let mut canceled = Vec::new();

        if self.discover_in_flight {
            let _ = tx.send(WorkerCommand::Cancel {
                op: WorkerOp::Discover,
                request_id: self.discover_request_id,
            });
            self.discover_request_id = self.next_request();
            self.discover_in_flight = false;
            canceled.push("discovery");
        }
        if self.list_in_flight {
            let _ = tx.send(WorkerCommand::Cancel {
                op: WorkerOp::List,
                request_id: self.list_request_id,
            });
            self.list_request_id = self.next_request();
            self.list_in_flight = false;
            self.list_progress = None;
            canceled.push("list");
        }
        if self.snapshot_in_flight {
            let _ = tx.send(WorkerCommand::Cancel {
                op: WorkerOp::Snapshot,
                request_id: self.snapshot_request_id,
            });
            self.snapshot_request_id = self.next_request();
            self.snapshot_in_flight = false;
            canceled.push("monitor");
        }

        if canceled.is_empty() {
            self.push_status("Nothing to cancel");
        } else {
            self.push_status(format!("Cancelled {}", canceled.join(",")));
        }
    }

    fn cancel_monitor(&mut self, tx: &Sender<WorkerCommand>) -> bool {
        if !self.snapshot_in_flight {
            return false;
        }
        let _ = tx.send(WorkerCommand::Cancel {
            op: WorkerOp::Snapshot,
            request_id: self.snapshot_request_id,
        });
        self.snapshot_request_id = self.next_request();
        self.snapshot_in_flight = false;
        true
    }

    fn handle_event(&mut self, evt: WorkerEvent) {
        match evt {
            WorkerEvent::Progress {
                op,
                request_id,
                message,
            } => {
                let matches_request = match op {
                    WorkerOp::Discover => request_id == self.discover_request_id,
                    WorkerOp::List => request_id == self.list_request_id,
                    WorkerOp::Snapshot => request_id == self.snapshot_request_id,
                };
                if !matches_request {
                    return;
                }
                let clean = Self::sanitize_for_status(&message);
                if op == WorkerOp::List {
                    self.list_progress = Some(clean.clone());
                }
                self.push_status(clean);
            }
            WorkerEvent::DiscoverDone { request_id, result } => {
                if request_id != self.discover_request_id {
                    return;
                }
                self.discover_in_flight = false;
                match result {
                    Ok(servers) => {
                        self.servers = servers;
                        if self.server_index >= self.servers.len() {
                            self.server_index = self.servers.len().saturating_sub(1);
                        }
                        if let Some(prev) = self.selected_server
                            && !self.servers.contains(&prev) {
                                self.selected_server = None;
                                self.selected_pv = None;
                                self.all_pvs.clear();
                                self.pvs.clear();
                                self.last_snapshot = None;
                                self.clear_chart_samples();
                                self.list_in_flight = false;
                                self.snapshot_in_flight = false;
                            }
                        self.push_status(format!("Discovered {} server(s)", self.servers.len()));
                    }
                    Err(err) => {
                        self.last_error = Some(Self::sanitize_for_status(&err));
                        self.push_status("Server discovery failed");
                    }
                }
            }
            WorkerEvent::PvListDone {
                request_id,
                server,
                result,
            } => {
                if request_id != self.list_request_id || Some(server) != self.selected_server {
                    return;
                }
                self.list_in_flight = false;
                self.list_progress = None;
                match result {
                    Ok((names, source)) => {
                        self.all_pvs = names;
                        self.apply_pv_filter();
                        self.list_source = Some(source);
                        self.push_status(format!(
                            "Loaded {} PV(s) via {}",
                            self.all_pvs.len(),
                            match source {
                                PvListSource::PvList => "__pvlist",
                                PvListSource::GetField => "GET_FIELD",
                                PvListSource::ServerRpc => "server RPC",
                                PvListSource::ServerGet => "server GET",
                            }
                        ));
                    }
                    Err(err) => {
                        self.last_error = Some(Self::sanitize_for_status(&err));
                        self.push_status("Failed to load PV list");
                        self.all_pvs.clear();
                        self.pvs.clear();
                        self.pv_index = 0;
                        self.selected_pv = None;
                        self.last_snapshot = None;
                        self.clear_chart_samples();
                    }
                }
            }
            WorkerEvent::SnapshotDone {
                request_id,
                server,
                pv,
                result,
            } => {
                if request_id != self.snapshot_request_id
                    || Some(server) != self.selected_server
                    || self.selected_pv.as_deref() != Some(pv.as_str())
                {
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        if let Some(sample) = chart_sample_from_value(&snapshot.value) {
                            self.push_chart_sample(sample);
                        }
                        self.last_snapshot = Some(snapshot);
                        self.last_update =
                            Some(Local::now().format("%Y-%m-%d %H:%M:%S").to_string());
                    }
                    Err(err) => {
                        self.snapshot_in_flight = false;
                        self.last_error = Some(Self::sanitize_for_status(&err));
                        self.push_status(format!("Monitor failed for {}", pv));
                    }
                }
            }
            WorkerEvent::SnapshotStopped {
                request_id,
                server,
                pv,
                message,
            } => {
                if request_id != self.snapshot_request_id
                    || Some(server) != self.selected_server
                    || self.selected_pv.as_deref() != Some(pv.as_str())
                {
                    return;
                }
                self.snapshot_in_flight = false;
                self.push_status(message);
            }
        }
    }

    fn move_up(&mut self) {
        match self.focus {
            FocusPane::Servers => {
                if self.servers.is_empty() {
                    return;
                }
                if self.server_index == 0 {
                    self.server_index = self.servers.len() - 1;
                } else {
                    self.server_index -= 1;
                }
            }
            FocusPane::Pvs => {
                if self.pvs.is_empty() {
                    return;
                }
                if self.pv_index == 0 {
                    self.pv_index = self.pvs.len() - 1;
                } else {
                    self.pv_index -= 1;
                }
            }
            FocusPane::Details => {
                if self.details_view == DetailsView::Text {
                    self.details_scroll = self.details_scroll.saturating_sub(1);
                }
            }
        }
    }

    fn move_down(&mut self) {
        match self.focus {
            FocusPane::Servers => {
                if self.servers.is_empty() {
                    return;
                }
                self.server_index = (self.server_index + 1) % self.servers.len();
            }
            FocusPane::Pvs => {
                if self.pvs.is_empty() {
                    return;
                }
                self.pv_index = (self.pv_index + 1) % self.pvs.len();
            }
            FocusPane::Details => {
                if self.details_view == DetailsView::Text {
                    self.details_scroll = self.details_scroll.saturating_add(1);
                }
            }
        }
    }

    fn activate_selection(&mut self, tx: &Sender<WorkerCommand>) {
        match self.focus {
            FocusPane::Servers => {
                let Some(server) = self.servers.get(self.server_index).copied() else {
                    return;
                };
                if self.snapshot_in_flight {
                    let _ = tx.send(WorkerCommand::Cancel {
                        op: WorkerOp::Snapshot,
                        request_id: self.snapshot_request_id,
                    });
                    self.snapshot_request_id = self.next_request();
                    self.snapshot_in_flight = false;
                }
                self.selected_server = Some(server);
                self.selected_pv = None;
                self.all_pvs.clear();
                self.pvs.clear();
                self.pv_index = 0;
                self.last_snapshot = None;
                self.clear_chart_samples();
                self.details_scroll = 0;
                self.issue_load_pv_list(tx, server);
            }
            FocusPane::Pvs => {
                let (Some(server), Some(pv)) =
                    (self.selected_server, self.pvs.get(self.pv_index).cloned())
                else {
                    return;
                };
                let pv_changed = self.selected_pv.as_deref() != Some(pv.as_str());
                self.selected_pv = Some(pv.clone());
                if pv_changed {
                    self.last_snapshot = None;
                    self.last_update = None;
                    self.clear_chart_samples();
                }
                self.details_scroll = 0;
                self.issue_snapshot(tx, server, &pv);
            }
            FocusPane::Details => {}
        }
    }
}

fn chart_sample_from_value(value: &DecodedValue) -> Option<f64> {
    let scalar = extract_nt_scalar_value(value).unwrap_or(value);
    match scalar {
        DecodedValue::Boolean(v) => Some(if *v { 1.0 } else { 0.0 }),
        DecodedValue::Int8(v) => Some(*v as f64),
        DecodedValue::Int16(v) => Some(*v as f64),
        DecodedValue::Int32(v) => Some(*v as f64),
        DecodedValue::Int64(v) => Some(*v as f64),
        DecodedValue::UInt8(v) => Some(*v as f64),
        DecodedValue::UInt16(v) => Some(*v as f64),
        DecodedValue::UInt32(v) => Some(*v as f64),
        DecodedValue::UInt64(v) => Some(*v as f64),
        DecodedValue::Float32(v) => Some(*v as f64),
        DecodedValue::Float64(v) => Some(*v),
        DecodedValue::String(v) => v.parse::<f64>().ok(),
        _ => None,
    }
}

fn scaled_chart_data(samples: &VecDeque<f64>) -> Vec<u64> {
    if samples.is_empty() {
        return Vec::new();
    }
    let min = samples.iter().fold(f64::INFINITY, |acc, v| acc.min(*v));
    let max = samples.iter().fold(f64::NEG_INFINITY, |acc, v| acc.max(*v));
    if !min.is_finite() || !max.is_finite() {
        return vec![0; samples.len()];
    }
    let range = max - min;
    if range.abs() < f64::EPSILON {
        return vec![CHART_SCALE_MAX / 2; samples.len()];
    }

    samples
        .iter()
        .map(|v| {
            let norm = ((*v - min) / range).clamp(0.0, 1.0);
            (norm * CHART_SCALE_MAX as f64).round() as u64
        })
        .collect()
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &ExploreApp) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(9)])
        .split(frame.area());

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(30),
            Constraint::Percentage(45),
        ])
        .split(outer[0]);

    let server_lines: Vec<Line> = if app.servers.is_empty() {
        vec![Line::from("No discovered servers")]
    } else {
        app.servers
            .iter()
            .enumerate()
            .map(|(idx, server)| {
                let selected_marker = if Some(*server) == app.selected_server {
                    "[*]"
                } else {
                    "   "
                };
                let cursor = if idx == app.server_index { ">" } else { " " };
                let guid_short = hex::encode(server.guid);
                Line::from(format!(
                    "{}{} {} ({})",
                    cursor,
                    selected_marker,
                    server.tcp_addr,
                    &guid_short[..8]
                ))
            })
            .collect()
    };
    let server_view_height = main[0].height.saturating_sub(2) as usize;
    let server_scroll = if server_view_height == 0 {
        0
    } else {
        app.server_index
            .saturating_sub(server_view_height.saturating_sub(1)) as u16
    };

    let servers_widget = Paragraph::new(Text::from(server_lines))
        .block(focus_block(
            format!("Servers ({} found)", app.servers.len()),
            app.focus == FocusPane::Servers && !app.show_help,
        ))
        .scroll((server_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(servers_widget, main[0]);

    let pv_lines: Vec<Line> = if app.pvs.is_empty() {
        if app.all_pvs.is_empty() {
            vec![Line::from("No PVs loaded (select a server, press Enter)")]
        } else {
            vec![Line::from("No PVs match current filter")]
        }
    } else {
        app.pvs
            .iter()
            .enumerate()
            .map(|(idx, pv)| {
                let selected_marker = if app.selected_pv.as_deref() == Some(pv.as_str()) {
                    "[*]"
                } else {
                    "   "
                };
                let cursor = if idx == app.pv_index { ">" } else { " " };
                Line::from(format!("{}{} {}", cursor, selected_marker, pv))
            })
            .collect()
    };
    let pv_view_height = main[1].height.saturating_sub(2) as usize;
    let pv_scroll = if pv_view_height == 0 {
        0
    } else {
        app.pv_index
            .saturating_sub(pv_view_height.saturating_sub(1)) as u16
    };
    let pvs_widget = Paragraph::new(Text::from(pv_lines))
        .block(focus_block(
            format!(
                "PVs ({} found) <[f to apply filter]> <[a add pv]>{}",
                app.pvs.len(),
                if app.add_pv_editing {
                    format!(" [adding: {}_]", app.add_pv_input)
                } else if app.filter_editing {
                    format!(" [typing: {}_]", app.filter_input)
                } else if app.pv_filter.is_empty() {
                    String::new()
                } else {
                    format!(" [filter: {}]", app.pv_filter)
                }
            ),
            app.focus == FocusPane::Pvs && !app.show_help,
        ))
        .scroll((pv_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(pvs_widget, main[1]);

    match app.details_view {
        DetailsView::Text => {
            let mut detail_lines = Vec::new();
            detail_lines.push(format!(
                "Selected server: {}",
                app.selected_server_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            ));
            detail_lines.push(format!(
                "Selected PV: {}",
                app.selected_pv
                    .clone()
                    .unwrap_or_else(|| "<none>".to_string())
            ));
            detail_lines.push(format!(
                "Monitor: {}",
                if app.poll_paused { "paused" } else { "running" },
            ));
            detail_lines.push(format!(
                "PV list source: {}",
                match app.list_source {
                    Some(PvListSource::PvList) => "__pvlist",
                    Some(PvListSource::GetField) => "GET_FIELD fallback",
                    Some(PvListSource::ServerRpc) => "server RPC fallback",
                    Some(PvListSource::ServerGet) => "server GET fallback",
                    None => "<unknown>",
                }
            ));
            detail_lines.push(format!(
                "Last update: {}",
                app.last_update
                    .clone()
                    .unwrap_or_else(|| "<none>".to_string())
            ));
            detail_lines.push(format!(
                "Last error: {}",
                app.last_error
                    .clone()
                    .unwrap_or_else(|| "<none>".to_string())
            ));
            detail_lines.push(String::new());

            if let Some(snapshot) = &app.last_snapshot {
                detail_lines.push("Latest value:".to_string());
                let render_opts = RenderOptions::default();
                detail_lines.push(format_output(
                    &snapshot.pv_name,
                    &snapshot.value,
                    &render_opts,
                ));
                detail_lines.push(format!(
                    "compact: {}",
                    format_compact_value(&snapshot.value)
                ));
                detail_lines.push(String::new());
                detail_lines.push(format!(
                    "Structure summary: {}",
                    format_structure_desc(&snapshot.introspection)
                ));
                detail_lines.push(String::new());
                detail_lines.push("Structure tree:".to_string());
                detail_lines.push(format_structure_tree(&snapshot.introspection));
            } else {
                detail_lines.push("No snapshot yet. Select a PV and press Enter.".to_string());
            }

            let details_widget = Paragraph::new(detail_lines.join("\n"))
                .block(focus_block(
                    "Details (Text) <[t to toggle]>".to_string(),
                    app.focus == FocusPane::Details && !app.show_help,
                ))
                .scroll((app.details_scroll, 0))
                .wrap(Wrap { trim: false });
            frame.render_widget(details_widget, main[2]);
        }
        DetailsView::Chart => {
            if app.chart_samples.is_empty() {
                let message = if app.last_snapshot.is_none() {
                    "No snapshot yet. Select a PV and press Enter, then press t.".to_string()
                } else {
                    "Selected PV is not chartable as numeric scalar data.".to_string()
                };
                let chart_wait = Paragraph::new(message)
                    .block(focus_block(
                        "Details (Chart) <[t to toggle]>".to_string(),
                        app.focus == FocusPane::Details && !app.show_help,
                    ))
                    .wrap(Wrap { trim: true });
                frame.render_widget(chart_wait, main[2]);
            } else {
                let min = app
                    .chart_samples
                    .iter()
                    .fold(f64::INFINITY, |acc, v| acc.min(*v));
                let max = app
                    .chart_samples
                    .iter()
                    .fold(f64::NEG_INFINITY, |acc, v| acc.max(*v));
                let last = *app.chart_samples.back().unwrap_or(&0.0);
                let title = format!(
                    "Details (Chart) n={} last={:.4} min={:.4} max={:.4}",
                    app.chart_samples.len(),
                    last,
                    min,
                    max
                );
                let scaled = scaled_chart_data(&app.chart_samples);
                let sparkline = Sparkline::default()
                    .block(focus_block(
                        format!("{title} <[t to toggle]>"),
                        app.focus == FocusPane::Details && !app.show_help,
                    ))
                    .data(&scaled)
                    .direction(RenderDirection::LeftToRight)
                    .style(Style::default().fg(Color::Cyan));
                frame.render_widget(sparkline, main[2]);
            }
        }
    }

    let mut activity = Vec::new();
    if app.discover_in_flight {
        activity.push("discover");
    }
    if app.list_in_flight {
        activity.push("list");
    }
    if app.snapshot_in_flight {
        activity.push("monitor");
    }
    let activity_summary = if activity.is_empty() {
        "idle".to_string()
    } else {
        activity.join(",")
    };
    let poll_state = if app.poll_paused { "paused" } else { "running" };
    let last_update = app.last_update.as_deref().unwrap_or("<none>");
    let list_stage = app
        .list_progress
        .as_deref()
        .unwrap_or(if app.list_in_flight {
            "<starting>"
        } else {
            "idle"
        });
    let header = format!(
        "ops: {} | monitor: {} | servers: {} | pvs: {} | last: {}",
        activity_summary,
        poll_state,
        app.servers.len(),
        app.pvs.len(),
        last_update
    );

    let latest = app.status.clone();
    let stage_line = format!("list-stage: {}", list_stage);
    let mut detail = app
        .status_log
        .get(1)
        .cloned()
        .unwrap_or_else(|| "No previous status message".to_string());
    if let Some(err) = &app.last_error {
        detail.push_str(&format!(" | error: {}", err));
    }
    let status_widget = Paragraph::new(format!("{header}\n{stage_line}\n{latest}\n{detail}"))
        .block(Block::default().borders(Borders::ALL).title("Status"))
        .wrap(Wrap { trim: false });
    frame.render_widget(status_widget, outer[1]);

    if app.show_help {
        let area = centered_rect(70, 70, frame.area());
        let help = Paragraph::new(
            "Key Bindings\n\
q: quit\n\
h: toggle help\n\
Tab: cycle focus panes\n\
Up/Down: navigate focused pane\n\
Enter: activate selection\n\
f: input PV filter (Enter applies)\n\
a: add PV manually (Enter applies)\n\
t: toggle details/chart view\n\
r: refresh discovery/list/monitor\n\
p: pause/resume monitor\n\
x/Esc: cancel in-flight ops\n\
\n\
Workflow\n\
1) Press r to discover servers\n\
2) Select server (left) and press Enter\n\
3) Select PV (middle) and press Enter\n\
4) Watch streaming value/structure updates (right)\n",
        )
        .block(Block::default().title("Help").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
        frame.render_widget(Clear, area);
        frame.render_widget(help, area);
    }
}

fn run_ui(
    mut terminal: DefaultTerminal,
    mut app: ExploreApp,
    cmd_tx: Sender<WorkerCommand>,
    evt_rx: Receiver<WorkerEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let tick_rate = Duration::from_millis(100);
    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        while let Ok(evt) = evt_rx.try_recv() {
            app.handle_event(evt);
        }

        if event::poll(tick_rate)?
            && let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if app.add_pv_editing {
                    match key.code {
                        KeyCode::Enter => app.apply_add_pv_input(&cmd_tx),
                        KeyCode::Backspace => {
                            app.add_pv_input.pop();
                        }
                        KeyCode::Esc => {
                            app.add_pv_editing = false;
                            app.add_pv_input.clear();
                            app.push_status("Add PV input cancelled");
                        }
                        KeyCode::Char(c) => app.add_pv_input.push(c),
                        _ => {}
                    }
                    continue;
                }
                if app.filter_editing {
                    match key.code {
                        KeyCode::Enter => app.apply_filter_input(),
                        KeyCode::Backspace => {
                            app.filter_input.pop();
                        }
                        KeyCode::Esc => {
                            app.filter_editing = false;
                            app.filter_input = app.pv_filter.clone();
                            app.push_status("PV filter input cancelled");
                        }
                        KeyCode::Char(c) => app.filter_input.push(c),
                        _ => {}
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Esc => {
                        if app.show_help {
                            app.show_help = false;
                        } else {
                            app.cancel_in_flight(&cmd_tx);
                        }
                    }
                    KeyCode::Char('h') => app.show_help = !app.show_help,
                    KeyCode::Char('f') => app.start_filter_input(),
                    KeyCode::Char('a') => app.start_add_pv_input(),
                    KeyCode::Char('t') => app.toggle_details_view(),
                    KeyCode::Tab => {
                        app.focus = match app.focus {
                            FocusPane::Servers => FocusPane::Pvs,
                            FocusPane::Pvs => FocusPane::Details,
                            FocusPane::Details => FocusPane::Servers,
                        }
                    }
                    KeyCode::Char('r') => app.manual_refresh(&cmd_tx),
                    KeyCode::Char('x') => app.cancel_in_flight(&cmd_tx),
                    KeyCode::Char('p') => {
                        app.poll_paused = !app.poll_paused;
                        let message = if app.poll_paused {
                            app.cancel_monitor(&cmd_tx);
                            "Monitor paused"
                        } else {
                            if let (Some(server), Some(pv)) =
                                (app.selected_server, app.selected_pv.clone())
                            {
                                app.issue_snapshot(&cmd_tx, server, &pv);
                            }
                            "Monitor resumed"
                        };
                        app.push_status(message);
                    }
                    KeyCode::Up => app.move_up(),
                    KeyCode::Down => app.move_down(),
                    KeyCode::Enter => app.activate_selection(&cmd_tx),
                    _ => {}
                }
            }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut poll_interval_secs: u64 = 1;
    let mut common = CommonClientArgs::new();

    {
        let mut ap = ArgumentParser::new();
        ap.set_description("Interactive PV server explorer");
        common.add_to_parser(&mut ap);
        ap.refer(&mut poll_interval_secs).add_option(
            &["--poll-interval"],
            Store,
            "PV polling interval in seconds",
        );
        ap.parse_args_or_exit();
    }

    common.init_tracing();
    let base_opts = common.into_pv_get_options(String::new())?;

    let poll_interval_secs = poll_interval_secs.max(1);

    let targets = build_search_targets(base_opts.search_addr, base_opts.bind_addr);
    let worker_cfg = WorkerConfig {
        opts: base_opts.clone(),
        search_targets: targets,
    };
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (evt_tx, evt_rx) = mpsc::channel();
    let worker_handle = thread::spawn(move || run_worker(worker_cfg, cmd_rx, evt_tx));

    color_eyre::install()?;
    let terminal = ratatui::init();
    let mut app = ExploreApp::new();
    if poll_interval_secs != 1 {
        app.push_status(format!(
            "monitor mode ignores --poll-interval={}s",
            poll_interval_secs
        ));
    }
    app.issue_discover(&cmd_tx);
    let ui_result = run_ui(terminal, app, cmd_tx.clone(), evt_rx);
    ratatui::restore();

    drop(cmd_tx);
    let _ = worker_handle.join();
    ui_result
}
