use crate::scry::common::fingerprint::cluster::{cluster_fingerprints, FingerprintCluster};
use crate::scry::common::fingerprint::similarity::DEFAULT_MATCH_THRESHOLD;
use crate::scry::common::fingerprint::types::FingerprintStatus;
use crate::scry::common::interfaces::InterfaceSelection;
use crate::scry::common::stats::{IfStats, PVEvent, TrackedConnectionEntry};
use chrono::{DateTime, Local};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use super::options::{options_for_page, OptionId, OptionItem, OptionKind};
use super::utils;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Dashboard,
    Connections,
    Decoded,
    Diagnostics,
    Fingerprints,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FingerprintsView {
    List,
    Clusters,
    Events,
}

impl FingerprintsView {
    pub fn next(self) -> Self {
        match self {
            Self::List => Self::Clusters,
            Self::Clusters => Self::Events,
            Self::Events => Self::List,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::List => "List",
            Self::Clusters => "Clusters",
            Self::Events => "Events",
        }
    }
}

/// One rendered row of the fingerprint list. Pre-formatted in the state so
/// the render layer stays free of spec-level judgements about what "no
/// value" means.
#[derive(Clone, Debug, PartialEq)]
pub struct FingerprintRow {
    pub channel: String,
    pub server: String,
    pub phase: String,
    pub lsb: String,
    pub cadence: String,
    pub arrival: String,
    pub samples: String,
    pub flags: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectionsView {
    Summary,
    Detail,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectionsPanelFocus {
    LiveList,
    Detail,
    StaleList,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodedPanelFocus {
    Table,
    Detail,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnectionsSortMode {
    Recent,
    Rate,
    Endpoint,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodedSortMode {
    NewestFirst,
    RequestType,
    Endpoint,
}

impl DecodedSortMode {
    pub fn next(self) -> Self {
        match self {
            Self::NewestFirst => Self::RequestType,
            Self::RequestType => Self::Endpoint,
            Self::Endpoint => Self::NewestFirst,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NewestFirst => "Newest",
            Self::RequestType => "Req",
            Self::Endpoint => "Endpoint",
        }
    }
}

impl ConnectionsSortMode {
    pub fn next(self) -> Self {
        match self {
            Self::Recent => Self::Rate,
            Self::Rate => Self::Endpoint,
            Self::Endpoint => Self::Recent,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Recent => "Recent",
            Self::Rate => "Rate",
            Self::Endpoint => "Endpoint",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FilterField {
    RequestType,
    PvName,
    SourceIp,
    DestinationIp,
    Text,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FocusArea {
    Main,
    Options,
}

pub struct ConnectionsPartitions {
    pub live: Vec<TrackedConnectionEntry>,
    pub stale: Vec<TrackedConnectionEntry>,
    pub filtered_out_total: usize,
    pub source_total: usize,
}

#[derive(Clone, Debug)]
pub struct DecodedRow {
    pub id: String,
    pub ts: String,
    pub ts_nanos: u128,
    pub request_type: String,
    pub source: String,
    pub destination: String,
    pub context_preview: String,
    pub context_full: String,
    pub host_name: String,
}

pub struct AppState {
    pub data1: Vec<f64>,
    pub data2: Vec<f64>,
    pub timestamps: Vec<f64>,
    pub log_hist: Vec<String>,
    pub if_stats_local: IfStats,
    pub event_filter: HashMap<String, bool>,
    pub event_filter_keys: Vec<String>,
    pub pause: bool,
    pub ui_tick_rate_ms: u64,
    pub ui_last_tick_lag_ms: u64,
    pub ui_is_lagging: bool,
    pub show_host: bool,
    pub show_help: bool,
    pub current_page: Page,
    pub focus_area: FocusArea,
    pub dashboard_options_index: usize,
    pub connections_options_index: usize,
    pub decoded_options_index: usize,
    pub diagnostics_options_index: usize,
    pub connections_view: ConnectionsView,
    pub connections_panel_focus: ConnectionsPanelFocus,
    pub decoded_panel_focus: DecodedPanelFocus,
    pub decoded_filter_field: FilterField,
    pub decoded_edit_mode: bool,
    pub decoded_filter_request_type: String,
    pub decoded_filter_pv: String,
    pub decoded_filter_source_ip: String,
    pub decoded_filter_destination_ip: String,
    pub decoded_filter_text: String,
    pub decoded_show_endpoints: bool,
    pub show_midstream: bool,
    pub filter_broadcast_beacon: bool,
    pub connections_search_text: String,
    pub connections_search_edit: bool,
    pub dashboard_top_n: usize,
    pub connections_live_scroll: u16,
    pub connections_stale_scroll: u16,
    pub decoded_table_scroll: u16,
    pub decoded_detail_scroll: u16,
    pub decoded_table_view_height: u16,
    pub decoded_selected_index: i32,
    pub decoded_selected_id: Option<String>,
    pub decoded_sort_mode: DecodedSortMode,
    pub connections_detail_scroll: u16,
    pub connections_live_list_view_height: u16,
    pub connections_stale_list_view_height: u16,
    pub connections_live_selected_index: i32,
    pub connections_stale_selected_index: i32,
    pub connections_selected_id: Option<String>,
    pub connections_live_window_secs: u64,
    pub connections_show_stale_panel: bool,
    pub connections_sort_mode: ConnectionsSortMode,
    pub fingerprints_view: FingerprintsView,
    pub fingerprints_options_index: usize,
    pub fingerprints_scroll: u16,
    pub fingerprints_view_height: u16,
    pub fingerprints_show_pending: bool,
    pub interface_selection: Option<Arc<InterfaceSelection>>,
    pub interface_menu_open: bool,
    pub interface_menu_index: usize,
    pub last_interface_index: Option<usize>,
}

impl AppState {
    pub fn new() -> Self {
        let data1 = vec![0.; 200];
        let data2 = vec![0.; 200];
        let timestamps = vec![0.; 200];
        let log_hist = vec!["".to_string(); 40];
        let if_stats_local = IfStats::new();
        let mut event_filter = HashMap::new();
        for req in utils::REQUEST_TYPES {
            event_filter.insert((*req).to_string(), true);
        }

        let mut event_filter_keys = event_filter.keys().cloned().collect::<Vec<String>>();
        event_filter_keys.sort();

        Self {
            data1,
            data2,
            timestamps,
            log_hist,
            if_stats_local,
            event_filter,
            event_filter_keys,
            pause: false,
            ui_tick_rate_ms: 100,
            ui_last_tick_lag_ms: 0,
            ui_is_lagging: false,
            show_host: false,
            show_help: false,
            current_page: Page::Dashboard,
            focus_area: FocusArea::Options,
            dashboard_options_index: 0,
            connections_options_index: 0,
            decoded_options_index: 0,
            diagnostics_options_index: 0,
            connections_view: ConnectionsView::Summary,
            connections_panel_focus: ConnectionsPanelFocus::LiveList,
            decoded_panel_focus: DecodedPanelFocus::Table,
            decoded_filter_field: FilterField::RequestType,
            decoded_edit_mode: false,
            decoded_filter_request_type: String::new(),
            decoded_filter_pv: String::new(),
            decoded_filter_source_ip: String::new(),
            decoded_filter_destination_ip: String::new(),
            decoded_filter_text: String::new(),
            decoded_show_endpoints: true,
            show_midstream: true,
            filter_broadcast_beacon: true,
            connections_search_text: String::new(),
            connections_search_edit: false,
            dashboard_top_n: 10,
            connections_live_scroll: 0,
            connections_stale_scroll: 0,
            decoded_table_scroll: 0,
            decoded_detail_scroll: 0,
            decoded_table_view_height: 0,
            decoded_selected_index: 0,
            decoded_selected_id: None,
            decoded_sort_mode: DecodedSortMode::NewestFirst,
            connections_detail_scroll: 0,
            connections_live_list_view_height: 0,
            connections_stale_list_view_height: 0,
            connections_live_selected_index: 0,
            connections_stale_selected_index: 0,
            connections_selected_id: None,
            connections_live_window_secs: 30,
            connections_show_stale_panel: false,
            connections_sort_mode: ConnectionsSortMode::Recent,
            fingerprints_view: FingerprintsView::List,
            fingerprints_options_index: 0,
            fingerprints_scroll: 0,
            fingerprints_view_height: 0,
            fingerprints_show_pending: false,
            interface_selection: None,
            interface_menu_open: false,
            interface_menu_index: 0,
            last_interface_index: None,
        }
    }

    pub fn set_interface_selection(&mut self, selection: Arc<InterfaceSelection>) {
        self.last_interface_index = Some(selection.current_index());
        self.interface_menu_index = selection.current_index();
        self.interface_selection = Some(selection);
    }

    pub fn interface_count(&self) -> usize {
        self.interface_selection
            .as_ref()
            .map(|selection| selection.len())
            .unwrap_or(0)
    }

    pub fn current_interface_index(&self) -> Option<usize> {
        self.interface_selection
            .as_ref()
            .map(|selection| selection.current_index())
    }

    pub fn current_interface_label(&self) -> Option<String> {
        let selection = self.interface_selection.as_ref()?;
        let idx = selection.current_index();
        let total = selection.len();
        let name = selection
            .interfaces
            .get(idx)
            .map(|iface| iface.name.clone())
            .unwrap_or_else(|| "unknown".to_string());
        Some(format!("IF {}/{} {}", idx + 1, total.max(1), name))
    }

    pub fn open_interface_menu(&mut self) {
        if self.interface_count() == 0 {
            return;
        }
        self.interface_menu_open = true;
        self.interface_menu_index = self.current_interface_index().unwrap_or(0);
    }

    pub fn close_interface_menu(&mut self) {
        self.interface_menu_open = false;
    }

    pub fn move_interface_menu_selection(&mut self, delta: i32) {
        let count = self.interface_count();
        if count == 0 {
            return;
        }
        let count_i32 = count as i32;
        let idx = self.interface_menu_index as i32;
        let next = (idx + delta).rem_euclid(count_i32);
        self.interface_menu_index = next as usize;
    }

    pub fn apply_interface_menu_selection(&mut self) {
        if let Some(selection) = self.interface_selection.as_ref() {
            selection.set_index(self.interface_menu_index);
        }
        self.close_interface_menu();
    }

    pub fn cycle_interface(&mut self) -> bool {
        if let Some(selection) = self.interface_selection.as_ref() {
            if selection.len() == 0 {
                return false;
            }
            selection.cycle_next();
            return true;
        }
        false
    }

    pub fn sync_interface_change(&mut self) -> bool {
        let current = self.current_interface_index();
        if current != self.last_interface_index {
            self.last_interface_index = current;
            return true;
        }
        false
    }

    pub fn reset_charts(&mut self) {
        self.data1 = vec![0.; 200];
        self.data2 = vec![0.; 200];
        self.timestamps = vec![0.; 200];
        self.log_hist = vec!["".to_string(); 40];
    }

    pub fn reset_selections(&mut self) {
        self.connections_selected_id = None;
        self.connections_live_selected_index = 0;
        self.connections_stale_selected_index = 0;
        self.connections_live_scroll = 0;
        self.connections_stale_scroll = 0;
        self.connections_detail_scroll = 0;
        self.decoded_selected_id = None;
        self.decoded_selected_index = 0;
        self.decoded_table_scroll = 0;
        self.decoded_detail_scroll = 0;
    }

    pub fn active_filter_mut(&mut self) -> &mut String {
        match self.decoded_filter_field {
            FilterField::RequestType => &mut self.decoded_filter_request_type,
            FilterField::PvName => &mut self.decoded_filter_pv,
            FilterField::SourceIp => &mut self.decoded_filter_source_ip,
            FilterField::DestinationIp => &mut self.decoded_filter_destination_ip,
            FilterField::Text => &mut self.decoded_filter_text,
        }
    }

    pub fn clear_filters(&mut self) {
        self.decoded_filter_request_type.clear();
        self.decoded_filter_pv.clear();
        self.decoded_filter_source_ip.clear();
        self.decoded_filter_destination_ip.clear();
        self.decoded_filter_text.clear();
    }

    pub fn clear_connections_search(&mut self) {
        self.connections_search_text.clear();
    }

    pub fn cycle_connections_sort_mode(&mut self) {
        self.connections_sort_mode = self.connections_sort_mode.next();
    }

    pub fn cycle_connections_live_window(&mut self) {
        const WINDOW_CHOICES: [u64; 4] = [10, 30, 60, 120];
        let next_idx = WINDOW_CHOICES
            .iter()
            .position(|v| *v == self.connections_live_window_secs)
            .map(|idx| (idx + 1) % WINDOW_CHOICES.len())
            .unwrap_or(1);
        self.connections_live_window_secs = WINDOW_CHOICES[next_idx];
    }

    pub fn cycle_decoded_sort_mode(&mut self) {
        self.decoded_sort_mode = self.decoded_sort_mode.next();
    }

    pub fn current_options_index_mut(&mut self) -> &mut usize {
        match self.current_page {
            Page::Dashboard => &mut self.dashboard_options_index,
            Page::Connections => &mut self.connections_options_index,
            Page::Decoded => &mut self.decoded_options_index,
            Page::Diagnostics => &mut self.diagnostics_options_index,
            Page::Fingerprints => &mut self.fingerprints_options_index,
        }
    }

    pub fn current_options_index(&self) -> usize {
        match self.current_page {
            Page::Dashboard => self.dashboard_options_index,
            Page::Connections => self.connections_options_index,
            Page::Decoded => self.decoded_options_index,
            Page::Diagnostics => self.diagnostics_options_index,
            Page::Fingerprints => self.fingerprints_options_index,
        }
    }

    pub fn current_options(&self) -> Vec<OptionItem> {
        options_for_page(self.current_page, self.event_filter_keys.len())
    }

    pub fn normalize_options_index(&mut self) {
        let items = self.current_options();
        if items.is_empty() {
            return;
        }
        let len = items.len();
        let mut idx = self.current_options_index().min(len - 1);
        if items[idx].kind == OptionKind::Section {
            for offset in 1..=len {
                let next = (idx + offset) % len;
                if items[next].kind != OptionKind::Section {
                    idx = next;
                    break;
                }
            }
        }
        *self.current_options_index_mut() = idx;
        if let OptionId::DecFilter(field) = items[idx].id {
            self.decoded_filter_field = field;
        }
    }

    pub fn move_options_selection(&mut self, delta: i32) {
        let items = self.current_options();
        if items.is_empty() {
            return;
        }
        let len = items.len() as i32;
        let mut idx = self.current_options_index() as i32;
        let start = idx;
        loop {
            idx = (idx + delta).rem_euclid(len);
            if items[idx as usize].kind != OptionKind::Section {
                break;
            }
            if idx == start {
                break;
            }
        }
        *self.current_options_index_mut() = idx as usize;
        if let OptionId::DecFilter(field) = items[idx as usize].id {
            self.decoded_filter_field = field;
        }
    }

    pub fn activate_option(&mut self, item: OptionItem) {
        match item.id {
            OptionId::Pause => {
                self.pause = !self.pause;
            }
            OptionId::ShowHost => {
                self.show_host = !self.show_host;
            }
            OptionId::ResetCharts => {
                self.data1 = vec![0.; 200];
                self.data2 = vec![0.; 200];
            }
            OptionId::EventFilter(idx) => {
                if let Some(key) = self.event_filter_keys.get(idx)
                    && let Some(value) = self.event_filter.get_mut(key) {
                        *value = !*value;
                    }
            }
            OptionId::ConnView => {
                self.connections_view = match self.connections_view {
                    ConnectionsView::Summary => ConnectionsView::Detail,
                    ConnectionsView::Detail => ConnectionsView::Summary,
                };
                self.connections_selected_id = None;
                self.connections_live_selected_index = 0;
                self.connections_stale_selected_index = 0;
                self.connections_live_scroll = 0;
                self.connections_stale_scroll = 0;
                self.connections_detail_scroll = 0;
            }
            OptionId::ConnMidstream => {
                self.show_midstream = !self.show_midstream;
            }
            OptionId::ConnBroadcast => {
                self.filter_broadcast_beacon = !self.filter_broadcast_beacon;
            }
            OptionId::ConnSort => {
                self.cycle_connections_sort_mode();
            }
            OptionId::ConnLiveWindow => {
                self.cycle_connections_live_window();
            }
            OptionId::ConnShowStale => {
                self.connections_show_stale_panel = !self.connections_show_stale_panel;
                if !self.connections_show_stale_panel
                    && self.connections_panel_focus == ConnectionsPanelFocus::StaleList
                {
                    self.connections_panel_focus = ConnectionsPanelFocus::LiveList;
                }
            }
            OptionId::ConnSearch => {
                self.connections_search_edit = true;
            }
            OptionId::ConnClearSearch => {
                self.clear_connections_search();
                self.connections_live_scroll = 0;
                self.connections_stale_scroll = 0;
                self.connections_detail_scroll = 0;
            }
            OptionId::DecFilter(field) => {
                self.decoded_filter_field = field;
                self.decoded_edit_mode = true;
            }
            OptionId::DecClearFilters => {
                self.clear_filters();
                self.decoded_table_scroll = 0;
                self.decoded_detail_scroll = 0;
            }
            OptionId::DecSort => {
                self.cycle_decoded_sort_mode();
            }
            OptionId::DecQuickFilter => {
                self.decoded_filter_field = FilterField::Text;
                self.decoded_edit_mode = true;
            }
            OptionId::DecShowEndpoints => {
                self.decoded_show_endpoints = !self.decoded_show_endpoints;
            }
            OptionId::FpView => {
                self.fingerprints_view = self.fingerprints_view.next();
                self.fingerprints_scroll = 0;
            }
            OptionId::FpShowPending => {
                self.fingerprints_show_pending = !self.fingerprints_show_pending;
            }
        }
    }

    /// Rows for the fingerprint list, spec §8.
    ///
    /// Formatting lives here rather than in the render layer because the
    /// distinctions it encodes are spec-level, not cosmetic: `pending n/m`
    /// (§6.1, not enough samples yet), `unlocked` (§2.1, phase measured but
    /// not a signature), and `—` (computed, observable genuinely absent) are
    /// three different facts.
    pub fn fingerprint_rows(&self) -> Vec<FingerprintRow> {
        let mut rows: Vec<FingerprintRow> = self
            .if_stats_local
            .fingerprints
            .iter()
            .map(|fp| FingerprintRow {
                channel: fp.channel.clone(),
                server: fp.server_addr.to_string(),
                phase: if fp.phase_is_locked() {
                    format!(
                        "{} ±{:.1}us",
                        format_phase_ms(fp.phase_mean_ms, 3),
                        fp.phase_spread_us
                    )
                } else {
                    "unlocked".to_string()
                },
                lsb: match fp.value_lsb {
                    Some(v) => format!("{} ({:.0}%)", format_lsb(v), fp.lsb_confidence * 100.0),
                    None => "—".to_string(),
                },
                cadence: format!("{:.3}/{:.1}", fp.cadence_modal_s, fp.cadence_p95_s),
                arrival: match fp.arrival_delta_var_ms {
                    Some(v) => format!("{:.2}±{:.2}", fp.arrival_delta_mean_ms, v),
                    None => format!("{:.2}±?", fp.arrival_delta_mean_ms),
                },
                samples: format!("{}/{}", fp.sample_count, fp.observed_count),
                flags: {
                    let mut f = Vec::new();
                    if fp.has_nan_limits {
                        f.push("NaN-limits".to_string());
                    }
                    if !matches!(
                        fp.sampling,
                        crate::scry::common::fingerprint::types::SamplingRegime::Full
                    ) {
                        f.push(fp.sampling.label().to_string());
                    }
                    f.join(" ")
                },
            })
            .collect();

        if self.fingerprints_show_pending {
            for status in &self.if_stats_local.fingerprint_statuses {
                if let FingerprintStatus::Insufficient {
                    key,
                    samples,
                    min_samples,
                    ..
                } = status
                {
                    // A channel that already has a fingerprint is also
                    // accumulating the next window; showing both would be
                    // two rows for one channel.
                    if rows.iter().any(|r| r.channel == key.channel) {
                        continue;
                    }
                    rows.push(FingerprintRow {
                        channel: key.channel.clone(),
                        server: key.server_addr.to_string(),
                        phase: format!("pending {}/{}", samples, min_samples),
                        lsb: "pending".to_string(),
                        cadence: "pending".to_string(),
                        arrival: "pending".to_string(),
                        samples: samples.to_string(),
                        flags: String::new(),
                    });
                }
            }
        }

        rows.sort_by(|a, b| a.channel.cmp(&b.channel).then(a.server.cmp(&b.server)));
        rows
    }

    /// Compact fingerprint for the channel list column (spec §8):
    /// `<phase ms>|<lsb>`, `…n/m` while a window is still filling (spec
    /// §6.1), `—` when the channel has no fingerprint at all.
    ///
    /// A *leading* `*` means the channel is served by more than one address
    /// and the badge shows only the lowest-addressed one. That case is the
    /// dual-source situation D1 exists to catch, so it is marked rather than
    /// resolved silently.
    ///
    /// The marker leads rather than trails because the cell truncates its
    /// tail: at 80 columns the layout squeezes this column to about seven
    /// characters, and a trailing marker is the first thing cut — rendering
    /// a dual-source channel byte-identically to a single-source one, which
    /// is exactly the §6.1 collapse the marker exists to prevent. Low-order
    /// lattice digits are the cheapest thing to lose, so they go last.
    pub fn fingerprint_badge(&self, pv_name: Option<&str>) -> String {
        let Some(pv) = pv_name else {
            return "—".to_string();
        };

        let mut matches: Vec<&crate::scry::common::fingerprint::types::SourceFingerprint> = self
            .if_stats_local
            .fingerprints
            .iter()
            .filter(|fp| fp.channel == pv)
            .collect();

        if matches.is_empty() {
            let pending = self.if_stats_local.fingerprint_statuses.iter().find_map(|s| match s {
                FingerprintStatus::Insufficient {
                    key,
                    samples,
                    min_samples,
                    ..
                } if key.channel == pv => Some((*samples, *min_samples)),
                _ => None,
            });
            return match pending {
                Some((n, m)) => format!("…{}/{}", n, m),
                None => "—".to_string(),
            };
        }

        matches.sort_by_key(|fp| fp.server_addr);
        let fp = matches[0];
        let phase = if fp.phase_is_locked() {
            format_phase_ms(fp.phase_mean_ms, 1)
        } else {
            "~".to_string()
        };
        let lsb = fp.value_lsb.map(format_lsb).unwrap_or_else(|| "—".to_string());
        let multi = if matches.len() > 1 { "*" } else { "" };
        format!("{}{}|{}", multi, phase, lsb)
    }

    /// Clusters for the cluster view, and how many fingerprints the cluster
    /// cap dropped (spec §4.3 / §6.3 — never silently).
    pub fn fingerprint_clusters(&self) -> (Vec<FingerprintCluster>, usize) {
        const MAX_CLUSTER_INPUTS: usize = 2000;
        cluster_fingerprints(
            &self.if_stats_local.fingerprints,
            DEFAULT_MATCH_THRESHOLD,
            MAX_CLUSTER_INPUTS,
        )
    }

    fn connections_source(&self) -> &[TrackedConnectionEntry] {
        match self.connections_view {
            ConnectionsView::Summary => &self.if_stats_local.tracked_connections_summary,
            ConnectionsView::Detail => &self.if_stats_local.tracked_connections_detail,
        }
    }

    fn entry_matches_search(entry: &TrackedConnectionEntry, needle: &str) -> bool {
        entry.endpoint_a.contains(needle)
            || entry.endpoint_b.contains(needle)
            || entry.id.contains(needle)
            || entry
                .pv_name
                .as_ref()
                .is_some_and(|pv_name| pv_name.contains(needle))
            || entry.recent_messages.iter().any(|m| m.contains(needle))
    }

    fn sort_entries(
        entries: &mut Vec<TrackedConnectionEntry>,
        mode: ConnectionsSortMode,
        is_detail: bool,
    ) {
        entries.sort_by(|a, b| match mode {
            ConnectionsSortMode::Recent => a
                .last_seen_secs
                .cmp(&b.last_seen_secs)
                .then_with(|| {
                    b.updates_per_sec
                        .partial_cmp(&a.updates_per_sec)
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| a.id.cmp(&b.id)),
            ConnectionsSortMode::Rate => b
                .updates_per_sec
                .partial_cmp(&a.updates_per_sec)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.last_seen_secs.cmp(&b.last_seen_secs))
                .then_with(|| a.id.cmp(&b.id)),
            ConnectionsSortMode::Endpoint => {
                if is_detail {
                    a.endpoint_a
                        .cmp(&b.endpoint_a)
                        .then_with(|| a.endpoint_b.cmp(&b.endpoint_b))
                        .then_with(|| {
                            a.pv_name
                                .as_deref()
                                .unwrap_or("")
                                .cmp(b.pv_name.as_deref().unwrap_or(""))
                        })
                        .then_with(|| a.cid.cmp(&b.cid))
                        .then_with(|| a.sid.cmp(&b.sid))
                } else {
                    a.endpoint_a
                        .cmp(&b.endpoint_a)
                        .then_with(|| a.endpoint_b.cmp(&b.endpoint_b))
                        .then_with(|| a.id.cmp(&b.id))
                }
            }
        });
    }

    pub fn connections_partitions(&self) -> ConnectionsPartitions {
        let source = self.connections_source();
        let source_total = source.len();
        let mut filtered_out_total = 0;

        let mut visible = Vec::new();
        for entry in source {
            if !self.show_midstream && entry.mid_stream {
                filtered_out_total += 1;
                continue;
            }
            if self.filter_broadcast_beacon && (entry.is_beacon || entry.is_broadcast) {
                filtered_out_total += 1;
                continue;
            }
            if !self.connections_search_text.is_empty()
                && !Self::entry_matches_search(entry, &self.connections_search_text)
            {
                filtered_out_total += 1;
                continue;
            }
            visible.push(entry.clone());
        }

        let mut live: Vec<TrackedConnectionEntry> = Vec::new();
        let mut stale: Vec<TrackedConnectionEntry> = Vec::new();
        for entry in visible {
            if entry.last_seen_secs <= self.connections_live_window_secs {
                live.push(entry);
            } else {
                stale.push(entry);
            }
        }

        let is_detail = self.connections_view == ConnectionsView::Detail;
        Self::sort_entries(&mut live, self.connections_sort_mode, is_detail);
        Self::sort_entries(&mut stale, self.connections_sort_mode, is_detail);

        ConnectionsPartitions {
            live,
            stale,
            filtered_out_total,
            source_total,
        }
    }

    pub fn sync_connections_selection(
        &mut self,
        live: &[TrackedConnectionEntry],
        stale: &[TrackedConnectionEntry],
    ) {
        if !self.connections_show_stale_panel
            && self.connections_panel_focus == ConnectionsPanelFocus::StaleList
        {
            self.connections_panel_focus = ConnectionsPanelFocus::LiveList;
        }

        if live.is_empty() && stale.is_empty() {
            self.connections_selected_id = None;
            self.connections_live_selected_index = 0;
            self.connections_stale_selected_index = 0;
            self.connections_live_scroll = 0;
            self.connections_stale_scroll = 0;
            return;
        }

        let mut found = false;
        if let Some(selected_id) = self.connections_selected_id.as_ref() {
            if let Some((idx, _)) = live.iter().enumerate().find(|(_, e)| e.id == *selected_id) {
                self.connections_live_selected_index = idx as i32;
                found = true;
            }
            if let Some((idx, _)) = stale.iter().enumerate().find(|(_, e)| e.id == *selected_id) {
                self.connections_stale_selected_index = idx as i32;
                found = true;
            }
        }

        if found {
            return;
        }

        if self.connections_panel_focus == ConnectionsPanelFocus::StaleList
            && self.connections_show_stale_panel
            && !stale.is_empty()
        {
            let idx = self
                .connections_stale_selected_index
                .clamp(0, stale.len().saturating_sub(1) as i32) as usize;
            self.connections_selected_id = Some(stale[idx].id.clone());
            self.connections_stale_selected_index = idx as i32;
            return;
        }

        if !live.is_empty() {
            let idx = self
                .connections_live_selected_index
                .clamp(0, live.len().saturating_sub(1) as i32) as usize;
            self.connections_selected_id = Some(live[idx].id.clone());
            self.connections_live_selected_index = idx as i32;
            return;
        }

        if !stale.is_empty() {
            let idx = self
                .connections_stale_selected_index
                .clamp(0, stale.len().saturating_sub(1) as i32) as usize;
            self.connections_selected_id = Some(stale[idx].id.clone());
            self.connections_stale_selected_index = idx as i32;
        }
    }

    pub fn selected_connection<'a>(
        &self,
        live: &'a [TrackedConnectionEntry],
        stale: &'a [TrackedConnectionEntry],
    ) -> Option<&'a TrackedConnectionEntry> {
        if let Some(selected_id) = self.connections_selected_id.as_ref() {
            if let Some(entry) = live.iter().find(|entry| entry.id == *selected_id) {
                return Some(entry);
            }
            if let Some(entry) = stale.iter().find(|entry| entry.id == *selected_id) {
                return Some(entry);
            }
        }

        if self.connections_panel_focus == ConnectionsPanelFocus::StaleList
            && self.connections_show_stale_panel
        {
            let idx = self.connections_stale_selected_index.max(0) as usize;
            return stale.get(idx);
        }

        let idx = self.connections_live_selected_index.max(0) as usize;
        live.get(idx).or_else(|| stale.first())
    }

    pub fn set_selected_live(&mut self, idx: usize, live: &[TrackedConnectionEntry]) {
        if let Some(entry) = live.get(idx) {
            self.connections_live_selected_index = idx as i32;
            self.connections_selected_id = Some(entry.id.clone());
        }
    }

    pub fn set_selected_stale(&mut self, idx: usize, stale: &[TrackedConnectionEntry]) {
        if let Some(entry) = stale.get(idx) {
            self.connections_stale_selected_index = idx as i32;
            self.connections_selected_id = Some(entry.id.clone());
        }
    }

    pub fn top_n_counts(map: &HashMap<String, u64>, n: usize) -> Vec<(String, u64)> {
        let mut items = map
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect::<Vec<(String, u64)>>();
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        items.truncate(n);
        items
    }

    fn event_ts_nanos(event: &PVEvent) -> u128 {
        event
            .system_time
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    fn event_ts_label(event: &PVEvent) -> String {
        let dt: DateTime<Local> = DateTime::from(event.system_time);
        dt.format("%H:%M:%S%.3f").to_string()
    }

    fn decoded_preview(context: &str, max_len: usize) -> String {
        let mut out = context.to_string();
        if out.len() > max_len {
            out.truncate(max_len.saturating_sub(3));
            out.push_str("...");
        }
        out
    }

    fn decoded_id(event: &PVEvent) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            Self::event_ts_nanos(event),
            event.source_ip,
            event.destination_ip,
            event.request_type,
            event.context
        )
    }

    fn event_matches_decoded_filters(&self, event: &PVEvent) -> bool {
        if !*self.event_filter.get(&event.request_type).unwrap_or(&true) {
            return false;
        }
        if !self.decoded_filter_request_type.is_empty()
            && !event
                .request_type
                .contains(&self.decoded_filter_request_type)
        {
            return false;
        }
        if !self.decoded_filter_source_ip.is_empty()
            && !event.source_ip.contains(&self.decoded_filter_source_ip)
        {
            return false;
        }
        if !self.decoded_filter_destination_ip.is_empty()
            && !event
                .destination_ip
                .contains(&self.decoded_filter_destination_ip)
        {
            return false;
        }
        if !self.decoded_filter_pv.is_empty() && !event.context.contains(&self.decoded_filter_pv) {
            return false;
        }
        if !self.decoded_filter_text.is_empty() {
            let needle = self.decoded_filter_text.as_str();
            let text_match = event.request_type.contains(needle)
                || event.source_ip.contains(needle)
                || event.destination_ip.contains(needle)
                || event.context.contains(needle)
                || event.host_name.contains(needle);
            if !text_match {
                return false;
            }
        }
        true
    }

    pub fn decoded_rows(&self) -> Vec<DecodedRow> {
        let mut rows = self
            .if_stats_local
            .events
            .iter()
            .filter(|event| self.event_matches_decoded_filters(event))
            .map(|event| DecodedRow {
                id: Self::decoded_id(event),
                ts: Self::event_ts_label(event),
                ts_nanos: Self::event_ts_nanos(event),
                request_type: event.request_type.clone(),
                source: event.source_ip.clone(),
                destination: event.destination_ip.clone(),
                context_preview: Self::decoded_preview(&event.context, 72),
                context_full: event.context.clone(),
                host_name: event.host_name.clone(),
            })
            .collect::<Vec<DecodedRow>>();

        rows.sort_by(|a, b| match self.decoded_sort_mode {
            DecodedSortMode::NewestFirst => {
                b.ts_nanos.cmp(&a.ts_nanos).then_with(|| a.id.cmp(&b.id))
            }
            DecodedSortMode::RequestType => a
                .request_type
                .cmp(&b.request_type)
                .then_with(|| b.ts_nanos.cmp(&a.ts_nanos))
                .then_with(|| a.id.cmp(&b.id)),
            DecodedSortMode::Endpoint => a
                .source
                .cmp(&b.source)
                .then_with(|| a.destination.cmp(&b.destination))
                .then_with(|| b.ts_nanos.cmp(&a.ts_nanos))
                .then_with(|| a.id.cmp(&b.id)),
        });

        rows
    }

    pub fn sync_decoded_selection(&mut self, rows: &[DecodedRow]) {
        if rows.is_empty() {
            self.decoded_selected_index = 0;
            self.decoded_selected_id = None;
            self.decoded_table_scroll = 0;
            return;
        }

        if let Some(ref selected_id) = self.decoded_selected_id
            && let Some((idx, _)) = rows
                .iter()
                .enumerate()
                .find(|(_, row)| &row.id == selected_id)
            {
                self.decoded_selected_index = idx as i32;
                return;
            }

        let max_idx = rows.len().saturating_sub(1) as i32;
        if self.decoded_selected_index < 0 {
            self.decoded_selected_index = 0;
        } else if self.decoded_selected_index > max_idx {
            self.decoded_selected_index = max_idx;
        }
        let idx = self.decoded_selected_index as usize;
        self.decoded_selected_id = Some(rows[idx].id.clone());
    }

    pub fn selected_decoded_row<'a>(&self, rows: &'a [DecodedRow]) -> Option<&'a DecodedRow> {
        if let Some(ref selected_id) = self.decoded_selected_id
            && let Some(row) = rows.iter().find(|row| &row.id == selected_id) {
                return Some(row);
            }
        let idx = self.decoded_selected_index.max(0) as usize;
        rows.get(idx).or_else(|| rows.first())
    }

    pub fn set_selected_decoded(&mut self, idx: usize, rows: &[DecodedRow]) {
        if let Some(row) = rows.get(idx) {
            self.decoded_selected_index = idx as i32;
            self.decoded_selected_id = Some(row.id.clone());
        }
    }
}

/// Width of the fingerprint badge cell in the connections Detail table.
///
/// Shared with `render::draw_connections_table` so the tests asserting that a
/// badge fits cannot drift away from the constraint that actually sizes it.
pub const FINGERPRINT_BADGE_CELL_WIDTH: usize = 16;

/// Render a sub-second phase, which lives on `[0, 1000)` ms.
///
/// Rounding a circular quantity can push it outside its own range: a phase
/// of 999.96 ms formats as `1000.0`, a value the phase can never actually
/// take, and one character wider than the cell budgeted for it — wide
/// enough to clip the `*` dual-source marker off the end of a badge and
/// hide the very case D1 exists to catch. Wrapping is also the
/// arithmetically correct answer and not merely the one that fits: 999.96 ms
/// and 0.04 ms are 0.08 ms apart, not 999.92 ms apart.
///
/// A non-finite input is passed through rather than wrapped, so a bug
/// upstream shows as `NaN` instead of masquerading as a locked 0.0 phase.
pub fn format_phase_ms(v: f64, decimals: usize) -> String {
    if !v.is_finite() {
        return format!("{:.*}", decimals, v);
    }
    let scale = 10f64.powi(decimals as i32);
    let rounded = (v * scale).round() / scale;
    // `+ 0.0` normalises `-0.0`, which would otherwise render as "-0.0":
    // a phantom sign on a quantity that has none, and a character wider.
    let wrapped = if rounded >= 1000.0 { 0.0 } else { rounded + 0.0 };
    format!("{:.*}", decimals, wrapped)
}

/// Render a lattice (`value_lsb`) to a bounded, four-significant-figure
/// string.
///
/// The lattice is derived as a GCD over float samples, so its exact `f64` is
/// routinely something like `0.017500000000000664` — twenty characters of
/// arithmetic noise. Both places it is shown put meaning *after* the number
/// (the `(nn%)` confidence in the list, the `*` dual-source marker in the
/// badge) inside a fixed-width cell, so an unbounded number does not lose a
/// digit, it loses the suffix. Spec §6.1 forbids that: a low-confidence
/// lattice must stay distinguishable from a certain one.
///
/// Four significant figures, trailing zeros trimmed, exponent form outside
/// 1e-4..1e6 — at most nine characters, and the magnitude always survives.
pub fn format_lsb(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    let mag = v.abs().log10().floor();
    if !mag.is_finite() || !(-4.0..6.0).contains(&mag) {
        return format!("{:.2e}", v);
    }
    let decimals = (3 - mag as i32).max(0) as usize;
    let s = format!("{:.*}", decimals, v);
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scry::common::fingerprint::types::{FingerprintKey, SamplingRegime, SourceFingerprint};
    use std::time::{Duration, UNIX_EPOCH};

    fn mk_fp(channel: &str, server: &str, phase_ms: f64, lsb: Option<f64>) -> SourceFingerprint {
        SourceFingerprint {
            channel: channel.to_string(),
            server_addr: server.parse().unwrap(),
            phase_mean_ms: phase_ms,
            phase_spread_us: 85.6,
            phase_circular_var: 3e-7,
            value_lsb: lsb,
            lsb_confidence: if lsb.is_some() { 1.0 } else { 0.0 },
            cadence_modal_s: 1.0,
            cadence_p50_s: 1.0,
            cadence_p95_s: 25.0,
            arrival_delta_mean_ms: 4.0,
            arrival_delta_var_ms: Some(0.25),
            sample_count: 40,
            window_start: UNIX_EPOCH,
            window_end: UNIX_EPOCH,
            struct_id: "epics:nt/NTScalar:1.0".to_string(),
            struct_id_changed: false,
            has_nan_limits: false,
            observed_count: 40,
            sampling: SamplingRegime::Full,
        }
    }

    #[test]
    fn fingerprint_rows_render_the_reference_values() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprints = vec![mk_fp(
            "R55IWATER::AMBIENT:TEMPERATURE",
            "10.0.0.11:5075",
            262.016,
            Some(0.0175),
        )];
        let rows = state.fingerprint_rows();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].phase.contains("262.016"), "phase was {}", rows[0].phase);
        assert!(rows[0].lsb.contains("0.0175"), "lsb was {}", rows[0].lsb);
        assert_eq!(rows[0].server, "10.0.0.11:5075");
    }

    #[test]
    fn the_badge_is_compact_phase_and_lattice() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprints =
            vec![mk_fp("R55IWATER::AMBIENT:TEMPERATURE", "10.0.0.11:5075", 262.016, Some(0.0175))];
        let badge = state.fingerprint_badge(Some("R55IWATER::AMBIENT:TEMPERATURE"));
        assert_eq!(badge, "262.0|0.0175");
        assert!(
            badge.chars().count() <= FINGERPRINT_BADGE_CELL_WIDTH,
            "badge must fit the column: {:?}",
            badge
        );
    }

    /// The lattice is a GCD of floats, so its exact `f64` prints as
    /// `0.017500000000000664`. Both call sites append meaning *after* the
    /// number — `(100%)` in the list, `*` in the badge — and the cells are
    /// fixed width, so an unbounded number does not truncate a digit, it
    /// truncates the suffix. Spec §6.1: a 40%-confidence lattice must not
    /// render byte-identically to a certain one.
    #[test]
    fn an_ugly_lattice_still_leaves_room_for_the_confidence() {
        const UGLY: f64 = 0.017500000000000664;
        let mut state = AppState::new();
        let mut fp = mk_fp("PV:UGLY", "10.0.0.11:5075", 262.016, Some(UGLY));
        fp.lsb_confidence = 0.4;
        state.if_stats_local.fingerprints = vec![fp];
        let rows = state.fingerprint_rows();
        // Constraint::Length(16) on the lsb column (render/fingerprints.rs).
        assert!(rows[0].lsb.chars().count() <= 16, "lsb cell overflows: {:?}", rows[0].lsb);
        assert!(rows[0].lsb.contains("0.0175"), "lattice lost: {:?}", rows[0].lsb);
        assert!(rows[0].lsb.contains("(40%)"), "confidence clipped away: {:?}", rows[0].lsb);
    }

    /// The same lattice in the channel-list badge, where the clipped suffix
    /// would be the `*` that marks the dual-source case D1 exists to catch.
    #[test]
    fn an_ugly_lattice_still_leaves_room_for_the_dual_source_marker() {
        const UGLY: f64 = 0.017500000000000664;
        let mut state = AppState::new();
        state.if_stats_local.fingerprints = vec![
            mk_fp("PV:DUAL", "10.0.0.12:5075", 695.9, Some(UGLY)),
            mk_fp("PV:DUAL", "10.0.0.11:5075", 262.0, Some(UGLY)),
        ];
        let badge = state.fingerprint_badge(Some("PV:DUAL"));
        assert!(badge.chars().count() <= FINGERPRINT_BADGE_CELL_WIDTH, "badge overflows its cell: {:?}", badge);
        assert!(badge.contains("0.0175"), "lattice lost: {:?}", badge);
        assert!(badge.starts_with('*'), "dual-source marker clipped away: {:?}", badge);
    }

    /// The reviewer's worst case: a locked `999.9` phase, a small-lattice
    /// `value_lsb` near 1e-4 that `format_lsb` renders at its full 9
    /// characters, and a second source on the same channel appending the
    /// `*` dual-source marker. `Constraint::Length(16)` on the fingerprint
    /// column (render/mod.rs) must fit all of it.
    #[test]
    fn the_worst_case_badge_still_fits_its_cell() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprints = vec![
            mk_fp("PV:WORST", "10.0.0.11:5075", 999.9, Some(0.0001234)),
            mk_fp("PV:WORST", "10.0.0.12:5075", 999.9, Some(0.0001234)),
        ];
        let badge = state.fingerprint_badge(Some("PV:WORST"));
        assert!(badge.chars().count() <= FINGERPRINT_BADGE_CELL_WIDTH, "badge overflows its cell: {:?}", badge);
        assert!(badge.starts_with('*'), "dual-source marker clipped away: {:?}", badge);
        assert!(badge.contains("0.0001234"), "lattice lost: {:?}", badge);
    }

    /// A phase is circular on `[0, 1000)` ms, so rounding must wrap rather
    /// than produce a value the quantity cannot hold. Left unwrapped this
    /// prints `1000.0`, one character too wide for the badge cell.
    #[test]
    fn a_phase_rounding_up_to_a_full_second_wraps_to_zero() {
        assert_eq!(format_phase_ms(999.96, 1), "0.0");
        assert_eq!(format_phase_ms(999.9996, 3), "0.000");
        // Just below the rounding boundary it must NOT wrap.
        assert_eq!(format_phase_ms(999.94, 1), "999.9");
        // Ordinary values are untouched.
        assert_eq!(format_phase_ms(262.016, 3), "262.016");
        assert_eq!(format_phase_ms(262.016, 1), "262.0");
        // A bug upstream must stay visible, not read as a locked 0.0 phase.
        assert_eq!(format_phase_ms(f64::NAN, 1), "NaN");
    }

    /// The true worst case: the phase that rounds up to a full second is one
    /// character wider than 999.9, which is what pushed the badge past its
    /// cell even after the column was widened to 16.
    #[test]
    fn a_wrapping_phase_does_not_widen_the_badge() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprints = vec![
            mk_fp("PV:WRAP", "10.0.0.11:5075", 999.96, Some(0.0001234)),
            mk_fp("PV:WRAP", "10.0.0.12:5075", 999.96, Some(0.0001234)),
        ];
        let badge = state.fingerprint_badge(Some("PV:WRAP"));
        assert!(badge.chars().count() <= FINGERPRINT_BADGE_CELL_WIDTH, "badge overflows its cell: {:?}", badge);
        assert!(badge.starts_with('*'), "dual-source marker clipped away: {:?}", badge);
        assert!(badge.starts_with("*0.0|"), "phase did not wrap: {:?}", badge);
    }

    /// Spec §6.1: at 80 columns the layout squeezes the fingerprint column to
    /// roughly seven characters, and the cell truncates its tail. A trailing
    /// dual-source marker would be the first thing cut, collapsing a
    /// two-source channel onto a one-source rendering. Leading, it survives
    /// any width that shows anything at all.
    #[test]
    fn the_dual_source_marker_survives_a_narrow_cell() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprints = vec![
            mk_fp("PV:NARROW", "10.0.0.11:5075", 999.96, Some(0.0001234)),
            mk_fp("PV:NARROW", "10.0.0.12:5075", 999.96, Some(0.0001234)),
        ];
        let badge = state.fingerprint_badge(Some("PV:NARROW"));
        let single = {
            let mut lone = AppState::new();
            lone.if_stats_local.fingerprints =
                vec![mk_fp("PV:NARROW", "10.0.0.11:5075", 999.96, Some(0.0001234))];
            lone.fingerprint_badge(Some("PV:NARROW"))
        };
        // Every width the cell can actually be squeezed to, down to one char.
        for width in 1..=FINGERPRINT_BADGE_CELL_WIDTH {
            let cut: String = badge.chars().take(width).collect();
            let cut_single: String = single.chars().take(width).collect();
            assert!(
                cut.contains('*'),
                "dual-source marker lost at width {}: {:?}",
                width,
                cut
            );
            assert_ne!(
                cut, cut_single,
                "dual and single source render identically at width {}",
                width
            );
        }
    }

    /// A non-finite phase must stay visible rather than wrap to a value that
    /// reads as a locked zero (spec §6.1).
    #[test]
    fn a_non_finite_phase_is_not_disguised_as_zero() {
        assert_eq!(format_phase_ms(f64::INFINITY, 1), "inf");
        assert_eq!(format_phase_ms(f64::NEG_INFINITY, 1), "-inf");
        assert_eq!(format_phase_ms(f64::NAN, 3), "NaN");
    }

    /// `-0.0` is a phantom sign on a quantity that has none, and it is a
    /// character wider than the cell budget assumes.
    #[test]
    fn a_negative_zero_phase_renders_unsigned() {
        assert_eq!(format_phase_ms(-0.0, 1), "0.0");
        assert!(!format_phase_ms(-0.0, 3).starts_with('-'));
    }

    /// `format_lsb` bounds the width without inventing precision or losing
    /// the magnitude.
    #[test]
    fn format_lsb_is_bounded_and_keeps_the_magnitude() {
        assert_eq!(format_lsb(0.0175), "0.0175");
        assert_eq!(format_lsb(0.017500000000000664), "0.0175");
        assert_eq!(format_lsb(0.01), "0.01");
        assert_eq!(format_lsb(0.0), "0");
        assert_eq!(format_lsb(-0.0175), "-0.0175");
        for v in [1e-9, 3.0517578125e-5, 1.0 / 3.0, 6.103515625e-5, 1234567.0, f64::MAX] {
            let s = format_lsb(v);
            assert!(s.chars().count() <= 9, "{} rendered as {:?}", v, s);
        }
    }

    /// Spec §8: `—` when there is nothing to show.
    #[test]
    fn an_unknown_channel_badges_as_a_dash() {
        let state = AppState::new();
        assert_eq!(state.fingerprint_badge(Some("PV:UNSEEN")), "—");
        assert_eq!(state.fingerprint_badge(None), "—");
    }

    /// Spec §6.1: "not yet" must not look like "nothing remarkable". The
    /// record's own `min_samples` must drive the count, not any global
    /// default — `fingerprint_coverage.min_samples` is deliberately set to a
    /// different value here so a badge built from the wrong field would be
    /// caught.
    #[test]
    fn an_accumulating_channel_badges_with_its_progress() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprint_coverage.min_samples = 30;
        state.if_stats_local.fingerprint_statuses = vec![FingerprintStatus::Insufficient {
            key: FingerprintKey::new("PV:NEW".to_string(), "10.0.0.11:5075".parse().unwrap()),
            samples: 7,
            min_samples: 12,
            window_start: UNIX_EPOCH,
        }];
        assert_eq!(state.fingerprint_badge(Some("PV:NEW")), "…7/12");
    }

    /// Spec §2.1: a phase that never locked is not a signature, so it must
    /// not be printed as one.
    #[test]
    fn an_unlocked_phase_badges_without_a_number() {
        let mut state = AppState::new();
        let mut fp = mk_fp("PV:EVT", "10.0.0.11:5075", 431.0, Some(0.01));
        fp.phase_circular_var = 0.87;
        state.if_stats_local.fingerprints = vec![fp];
        assert_eq!(state.fingerprint_badge(Some("PV:EVT")), "~|0.01");
    }

    #[test]
    fn a_channel_without_a_lattice_badges_phase_only() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprints = vec![mk_fp("PV:CALC", "10.0.0.11:5075", 262.0, None)];
        assert_eq!(state.fingerprint_badge(Some("PV:CALC")), "262.0|—");
    }

    /// Spec §3 keys a fingerprint by (channel, server). A channel appearing
    /// on two servers is the dual-source case, and the badge must not hide it.
    #[test]
    fn a_channel_on_two_servers_is_marked() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprints = vec![
            mk_fp("PV:DUAL", "10.0.0.12:5075", 695.9, Some(0.01)),
            mk_fp("PV:DUAL", "10.0.0.11:5075", 262.0, Some(0.0175)),
        ];
        let badge = state.fingerprint_badge(Some("PV:DUAL"));
        assert!(badge.starts_with('*'), "badge was {:?}", badge);
        // Lowest server address wins, so the badge does not flicker between
        // ticks.
        assert!(badge.starts_with("*262.0"), "badge was {:?}", badge);
    }

    /// Spec §6.1: "not enough samples yet" and "computed, nothing remarkable"
    /// are different facts and must not render the same.
    #[test]
    fn pending_channels_are_distinguished_from_absent_observables() {
        let mut state = AppState::new();
        state.fingerprints_show_pending = true;
        state.if_stats_local.fingerprint_statuses = vec![FingerprintStatus::Insufficient {
            key: FingerprintKey::new("PV:NEW".to_string(), "10.0.0.11:5075".parse().unwrap()),
            samples: 7,
            // Deliberately not 30: pins the rendered string against the
            // record's own `min_samples` rather than a hard-coded guess. If
            // production ever reverted to `format!("pending {}/{}", samples,
            // 30)`, this would render "pending 7/30" and the exact-string
            // assertion below would fail.
            min_samples: 17,
            window_start: UNIX_EPOCH,
        }];
        state.if_stats_local.fingerprints = vec![mk_fp("PV:OLD", "10.0.0.11:5075", 262.0, None)];

        let rows = state.fingerprint_rows();
        let pending = rows.iter().find(|r| r.channel == "PV:NEW").unwrap();
        assert_eq!(pending.phase, "pending 7/17", "phase was {}", pending.phase);

        let no_lattice = rows.iter().find(|r| r.channel == "PV:OLD").unwrap();
        assert_eq!(no_lattice.lsb, "—");
    }

    /// Spec §2.1: a wide phase is absence of evidence, so it must not render
    /// as a number that invites comparison.
    #[test]
    fn an_unlocked_phase_renders_as_unlocked() {
        let mut state = AppState::new();
        let mut fp = mk_fp("PV:EVT", "10.0.0.11:5075", 431.0, None);
        fp.phase_circular_var = 0.87;
        state.if_stats_local.fingerprints = vec![fp];
        assert_eq!(state.fingerprint_rows()[0].phase, "unlocked");
    }

    #[test]
    fn pending_rows_are_hidden_when_the_option_is_off() {
        let mut state = AppState::new();
        assert!(!state.fingerprints_show_pending);
        state.if_stats_local.fingerprint_statuses = vec![FingerprintStatus::Insufficient {
            key: FingerprintKey::new("PV:NEW".to_string(), "10.0.0.11:5075".parse().unwrap()),
            samples: 7,
            min_samples: 30,
            window_start: UNIX_EPOCH,
        }];
        assert!(state.fingerprint_rows().is_empty());
    }

    /// Spec §3.1: synthesised metadata is the cheapest bridge marker there
    /// is, so it must be visible without opening anything.
    #[test]
    fn synthetic_limits_are_flagged_in_the_row() {
        let mut state = AppState::new();
        let mut fp = mk_fp("PV:BRIDGED", "10.0.0.30:5075", 262.0, Some(0.01));
        fp.has_nan_limits = true;
        state.if_stats_local.fingerprints = vec![fp];
        assert!(state.fingerprint_rows()[0].flags.contains("NaN"));
    }

    /// Spec §8: the cluster view groups by similarity, and a cluster that
    /// crosses server boundaries is the thing worth seeing.
    #[test]
    fn clusters_group_by_source_and_report_spanned_servers() {
        let mut state = AppState::new();
        state.if_stats_local.fingerprints = vec![
            mk_fp("A", "10.0.0.11:5075", 262.016, Some(0.0175)),
            mk_fp("B", "10.0.0.99:5075", 262.018, Some(0.0175)),
            mk_fp("C", "10.0.0.12:5075", 695.973, Some(0.01)),
        ];
        let (clusters, dropped) = state.fingerprint_clusters();
        assert_eq!(dropped, 0);
        assert_eq!(clusters.len(), 2);
        let crossing = clusters.iter().find(|c| c.servers.len() > 1).unwrap();
        assert_eq!(crossing.members.len(), 2);
    }

    #[test]
    fn the_fingerprints_view_cycles() {
        let mut state = AppState::new();
        assert_eq!(state.fingerprints_view, FingerprintsView::List);
        state.activate_option(OptionItem { kind: OptionKind::Toggle, id: OptionId::FpView });
        assert_eq!(state.fingerprints_view, FingerprintsView::Clusters);
        state.activate_option(OptionItem { kind: OptionKind::Toggle, id: OptionId::FpView });
        assert_eq!(state.fingerprints_view, FingerprintsView::Events);
        state.activate_option(OptionItem { kind: OptionKind::Toggle, id: OptionId::FpView });
        assert_eq!(state.fingerprints_view, FingerprintsView::List);
    }

    fn mk_entry(
        id: &str,
        endpoint_a: &str,
        endpoint_b: &str,
        last_seen_secs: u64,
        updates_per_sec: f64,
    ) -> TrackedConnectionEntry {
        TrackedConnectionEntry {
            id: id.to_string(),
            endpoint_a: endpoint_a.to_string(),
            endpoint_b: endpoint_b.to_string(),
            cid: None,
            sid: None,
            pv_name: None,
            channel_count: 1,
            operation_count: 1,
            updates_per_sec,
            last_seen_secs,
            mid_stream: false,
            is_beacon: false,
            is_broadcast: false,
            recent_messages: vec![],
        }
    }

    fn mk_event(ts_secs: u64, req: &str, src: &str, dst: &str, context: &str) -> PVEvent {
        let mut event = PVEvent::new(src.to_string(), dst.to_string(), req.to_string());
        event.context = context.to_string();
        event.system_time = UNIX_EPOCH + Duration::from_secs(ts_secs);
        event
    }

    #[test]
    fn selection_remains_pinned_when_timestamps_change() {
        let mut state = AppState::new();
        state.if_stats_local.tracked_connections_summary = vec![
            mk_entry("a", "10.0.0.1:1111", "10.0.0.2:2222", 1, 2.0),
            mk_entry("b", "10.0.0.3:3333", "10.0.0.4:4444", 2, 3.0),
        ];
        state.connections_selected_id = Some("b".to_string());

        let p1 = state.connections_partitions();
        state.sync_connections_selection(&p1.live, &p1.stale);
        assert_eq!(state.connections_selected_id.as_deref(), Some("b"));

        state.if_stats_local.tracked_connections_summary[1].last_seen_secs = 6;
        let p2 = state.connections_partitions();
        state.sync_connections_selection(&p2.live, &p2.stale);
        assert_eq!(state.connections_selected_id.as_deref(), Some("b"));
    }

    #[test]
    fn recent_sort_and_rate_sort_are_deterministic() {
        let mut state = AppState::new();
        state.if_stats_local.tracked_connections_summary = vec![
            mk_entry("c", "10.0.0.9:1", "10.0.0.9:2", 4, 2.0),
            mk_entry("a", "10.0.0.1:1", "10.0.0.1:2", 1, 2.0),
            mk_entry("b", "10.0.0.2:1", "10.0.0.2:2", 1, 3.0),
        ];

        state.connections_sort_mode = ConnectionsSortMode::Recent;
        let recent = state.connections_partitions();
        let recent_ids = recent
            .live
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(recent_ids, vec!["b", "a", "c"]);

        state.connections_sort_mode = ConnectionsSortMode::Rate;
        let rate = state.connections_partitions();
        let rate_ids = rate.live.iter().map(|e| e.id.as_str()).collect::<Vec<_>>();
        assert_eq!(rate_ids, vec!["b", "a", "c"]);
    }

    #[test]
    fn live_stale_partition_respects_live_window() {
        let mut state = AppState::new();
        state.connections_live_window_secs = 30;
        state.if_stats_local.tracked_connections_summary = vec![
            mk_entry("live", "10.0.0.1:1", "10.0.0.1:2", 10, 1.0),
            mk_entry("stale", "10.0.0.2:1", "10.0.0.2:2", 31, 1.0),
        ];

        let p = state.connections_partitions();
        assert_eq!(p.live.len(), 1);
        assert_eq!(p.stale.len(), 1);
        assert_eq!(p.live[0].id, "live");
        assert_eq!(p.stale[0].id, "stale");
    }

    #[test]
    fn stale_hidden_does_not_panic_or_clear_data() {
        let mut state = AppState::new();
        state.connections_live_window_secs = 30;
        state.connections_show_stale_panel = false;
        state.if_stats_local.tracked_connections_summary =
            vec![mk_entry("only-stale", "10.0.0.1:1", "10.0.0.1:2", 80, 0.2)];

        let p = state.connections_partitions();
        state.sync_connections_selection(&p.live, &p.stale);

        assert!(p.live.is_empty());
        assert_eq!(p.stale.len(), 1);
        assert_eq!(state.connections_selected_id.as_deref(), Some("only-stale"));
    }

    #[test]
    fn decoded_selection_remains_pinned_across_refresh() {
        let mut state = AppState::new();
        state.if_stats_local.events = vec![
            mk_event(100, "GET", "10.0.0.1", "10.0.0.2", "alpha"),
            mk_event(101, "PUT", "10.0.0.3", "10.0.0.4", "beta"),
        ];
        let rows = state.decoded_rows();
        let pinned = rows[1].id.clone();
        state.decoded_selected_id = Some(pinned.clone());

        state.if_stats_local.events.push(mk_event(
            102,
            "MONITOR",
            "10.0.0.9",
            "10.0.0.10",
            "gamma",
        ));
        let rows2 = state.decoded_rows();
        state.sync_decoded_selection(&rows2);
        assert_eq!(state.decoded_selected_id.as_deref(), Some(pinned.as_str()));
    }

    #[test]
    fn decoded_selection_falls_back_when_selected_row_disappears() {
        let mut state = AppState::new();
        state.if_stats_local.events = vec![
            mk_event(100, "GET", "10.0.0.1", "10.0.0.2", "alpha"),
            mk_event(101, "PUT", "10.0.0.3", "10.0.0.4", "beta"),
        ];
        let rows = state.decoded_rows();
        state.decoded_selected_id = Some(rows[1].id.clone());

        state.if_stats_local.events = vec![mk_event(102, "GET", "10.0.0.5", "10.0.0.6", "only")];
        let rows2 = state.decoded_rows();
        state.sync_decoded_selection(&rows2);
        assert_eq!(
            state.decoded_selected_id.as_deref(),
            Some(rows2[0].id.as_str())
        );
    }

    #[test]
    fn decoded_newest_first_sort_is_deterministic() {
        let mut state = AppState::new();
        state.decoded_sort_mode = DecodedSortMode::NewestFirst;
        state.if_stats_local.events = vec![
            mk_event(50, "GET", "10.0.0.1", "10.0.0.2", "a"),
            mk_event(52, "GET", "10.0.0.1", "10.0.0.2", "b"),
            mk_event(51, "GET", "10.0.0.1", "10.0.0.2", "c"),
        ];
        let rows = state.decoded_rows();
        assert!(rows[0].ts_nanos > rows[1].ts_nanos);
        assert!(rows[1].ts_nanos > rows[2].ts_nanos);
    }

    #[test]
    fn decoded_rows_respect_event_filter_selection() {
        let mut state = AppState::new();
        state.if_stats_local.events = vec![
            mk_event(100, "GET", "10.0.0.1", "10.0.0.2", "pv=A"),
            mk_event(101, "PUT", "10.0.0.1", "10.0.0.3", "pv=B"),
        ];
        state.event_filter.insert("PUT".to_string(), false);

        let rows = state.decoded_rows();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_type, "GET");
    }

    #[test]
    fn decoded_text_filter_matches_anywhere() {
        let mut state = AppState::new();
        state.if_stats_local.events = vec![
            mk_event(100, "GET", "10.0.0.1", "10.0.0.2", "pv=alpha"),
            mk_event(101, "PUT", "10.0.1.1", "10.0.1.2", "pv=beta"),
        ];
        state.decoded_filter_text = "10.0.1.1".to_string();

        let rows = state.decoded_rows();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_type, "PUT");
    }

    #[test]
    fn dashboard_top_n_counts_respects_cap_and_order() {
        let mut map = HashMap::new();
        map.insert("a".to_string(), 3);
        map.insert("b".to_string(), 10);
        map.insert("c".to_string(), 7);
        let top = AppState::top_n_counts(&map, 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0, "b");
        assert_eq!(top[1].0, "c");
    }
}
