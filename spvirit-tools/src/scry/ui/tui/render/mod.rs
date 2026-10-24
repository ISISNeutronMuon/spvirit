use chrono::{DateTime, Local};
use ratatui::{
    prelude::*,
    widgets::{
        Block, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation, Table, Wrap,
    },
};
use std::time::SystemTime;

use self::widgets::{focus_border_style, panel, scrollbar};
use super::options::{OptionId, OptionKind};
use super::state::{
    ConnectionsPanelFocus, ConnectionsView, FilterField, FocusArea, Page,
    FINGERPRINT_BADGE_CELL_WIDTH,
};
use super::{state::AppState, utils};
use crate::scry::common::stats::TrackedConnectionEntry;
use fingerprints::draw_fingerprints;

mod fingerprints;
mod widgets;

pub fn draw(frame: &mut Frame, state: &mut AppState) {
    let root_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());

    let main_area = root_layout[0];
    let legend_area = root_layout[1];

    let main_split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Percentage(80), Constraint::Percentage(20)])
        .split(main_area);

    match state.current_page {
        Page::Dashboard => draw_dashboard(frame, state, main_split[0]),
        Page::Connections => draw_connections(frame, state, main_split[0]),
        Page::Decoded => draw_decoded(frame, state, main_split[0]),
        Page::Diagnostics => draw_diagnostics(frame, state, main_split[0]),
        Page::Fingerprints => draw_fingerprints(frame, state, main_split[0]),
    }

    draw_side_panel(frame, state, main_split[1]);
    let legend_split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Min(1), Constraint::Length(36)])
        .split(legend_area);
    draw_legend(frame, state, legend_split[0]);
    draw_status_indicator(frame, state, legend_split[1]);

    if state.show_help {
        draw_help_overlay(frame);
    }

    if state.interface_menu_open {
        draw_interface_overlay(frame, state);
    }
}

fn draw_help_overlay(frame: &mut Frame) {
    let area = utils::centered_rect(70, 70, frame.area());
    let help_text = Text::raw(
        "Key Bindings\n\
q: quit\n\
 h: toggle this help\n\
1/2/3/4: switch pages\n\
Tab/Right: next panel focus\n\
Left: previous panel focus\n\
Up/Down: navigate focused panel\n\
PgUp/PgDn: fast scroll focused list/detail\n\
Space: toggle/activate or enter edit\n\
Enter/Esc: exit edit\n\
\n\
p: pause/resume\n\
H: toggle host display\n\
 i: next interface (non-decoded pages)\n\
 I: interface picker\n\
\n\
Main Panel Notes:\n\
Connections: Tab cycles live/detail/stale/options\n\
Connections: s sort | l live window | t stale panel | / search\n\
Decoded: Tab cycles table/detail/options\n\
Decoded: / text(anywhere) | r req | v pv | i src | o dst\n",
    );
    let block = Block::default().title("Help").borders(Borders::ALL);
    let paragraph = Paragraph::new(help_text)
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn format_local_time(system_time: SystemTime) -> String {
    let dt: DateTime<Local> = DateTime::from(system_time);
    dt.format("%H:%M:%S%.3f").to_string()
}

fn truncate_for_width(value: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let char_count = value.chars().count();
    if char_count <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut out = value
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

fn context_chars_for_table(table_width: u16, fixed_cols: &[u16]) -> usize {
    let fixed: u16 = fixed_cols.iter().copied().sum();
    let spacing = fixed_cols.len().saturating_sub(1) as u16;
    let border = 2u16;
    let available = table_width.saturating_sub(fixed + spacing + border) as usize;
    available.max(4)
}

fn draw_dashboard(frame: &mut Frame, state: &AppState, area: Rect) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![
            Constraint::Length(5),
            Constraint::Length(12),
            Constraint::Min(8),
        ])
        .split(area);

    let kpi_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(root[0]);

    let rate = state.data1[0];
    let packet_rate = state.data2[0];
    let events_count = state.if_stats_local.events.len();
    let buffer_size = state.if_stats_local.buffer_size.max(1);
    let buffer_count = state.if_stats_local.events.len();
    let buffer_pct = (buffer_count as f64 / buffer_size as f64) * 100.0;
    let buffer_color = if buffer_pct >= 90.0 {
        Color::Red
    } else if buffer_pct >= 75.0 {
        Color::Yellow
    } else {
        Color::LightGreen
    };
    let kpis = [
        ("Data Rate", utils::format_rate(rate), Color::Yellow),
        (
            "Packet Rate",
            format!("{:.2} pps", packet_rate),
            Color::LightRed,
        ),
        ("Events", events_count.to_string(), Color::Cyan),
        (
            "Buffer",
            format!("{:.1}% ({}/{})", buffer_pct, buffer_count, buffer_size),
            buffer_color,
        ),
    ];
    for (idx, (title, value, color)) in kpis.iter().enumerate() {
        let text = Paragraph::new(Text::from(vec![Line::from(Span::styled(
            value.clone(),
            Style::default().fg(*color).add_modifier(Modifier::BOLD),
        ))]))
        .block(panel(title, false))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });
        frame.render_widget(text, kpi_layout[idx]);
    }

    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![
            Constraint::Percentage(33),
            Constraint::Percentage(33),
            Constraint::Percentage(34),
        ])
        .split(root[1]);

    let top_n = state.dashboard_top_n;
    let sources = AppState::top_n_counts(&state.if_stats_local.source_ips, top_n);
    let destinations = AppState::top_n_counts(&state.if_stats_local.destination_ips, top_n);
    let reqs = AppState::top_n_counts(&state.if_stats_local.request_types, top_n);

    let draw_counts_table = |frame: &mut Frame,
                             area: Rect,
                             title: String,
                             rows_data: &[(String, u64)],
                             key_header: &str| {
        let header = Row::new(vec!["#", key_header, "Count"]).style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
        let rows = if rows_data.is_empty() {
            vec![Row::new(vec!["", "No data", "0"])]
        } else {
            rows_data
                .iter()
                .enumerate()
                .map(|(idx, (key, count))| {
                    Row::new(vec![
                        Cell::from((idx + 1).to_string()),
                        Cell::from(key.clone()),
                        Cell::from(count.to_string()),
                    ])
                })
                .collect::<Vec<Row>>()
        };
        let table = Table::new(
            rows,
            vec![
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(8),
            ],
        )
        .header(header)
        .block(panel(&title, false))
        .column_spacing(1);
        frame.render_widget(table, area);
    };
    draw_counts_table(
        frame,
        middle[0],
        format!("Top Sources ({})", sources.len()),
        &sources,
        "Source",
    );
    draw_counts_table(
        frame,
        middle[1],
        format!("Top Destinations ({})", destinations.len()),
        &destinations,
        "Destination",
    );
    draw_counts_table(
        frame,
        middle[2],
        format!("Top Request Types ({})", reqs.len()),
        &reqs,
        "Request",
    );

    let feed_events = state
        .if_stats_local
        .events
        .iter()
        .rev()
        .filter(|event| *state.event_filter.get(&event.request_type).unwrap_or(&true))
        .take(32)
        .collect::<Vec<_>>();
    let feed_context_chars = context_chars_for_table(root[2].width, &[14, 12, 16, 16]);
    let header = Row::new(vec!["Time", "Req", "Source", "Destination", "Context"]).style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    let feed_rows = if feed_events.is_empty() {
        vec![Row::new(vec!["-", "-", "-", "-", "No events to show"])]
    } else {
        feed_events
            .iter()
            .map(|event| {
                let ts = format_local_time(event.system_time);
                let context = truncate_for_width(&event.context, feed_context_chars);
                Row::new(vec![
                    Cell::from(ts),
                    Cell::from(event.request_type.clone()),
                    Cell::from(event.source_ip.clone()),
                    Cell::from(event.destination_ip.clone()),
                    Cell::from(context),
                ])
                .style(Style::default().fg(utils::color_for_request_type(&event.request_type)))
            })
            .collect::<Vec<Row>>()
    };
    let feed_title = format!("Recent Decoded Feed ({})", feed_events.len());
    let feed_table = Table::new(
        feed_rows,
        vec![
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(panel(&feed_title, false))
    .column_spacing(1);
    frame.render_widget(feed_table, root[2]);
}

pub(super) fn visible_window(total: usize, scroll: &mut u16, view_height: u16) -> (usize, usize) {
    let vh = view_height.max(1) as usize;
    let max_start = total.saturating_sub(vh);
    if *scroll as usize > max_start {
        *scroll = max_start as u16;
    }
    let start = (*scroll as usize).min(total);
    let end = (start + vh).min(total);
    (start, end)
}

fn connection_flags(entry: &TrackedConnectionEntry) -> String {
    let mut flags = Vec::new();
    if entry.mid_stream {
        flags.push("M");
    }
    if entry.is_beacon {
        flags.push("BEACON");
    }
    if entry.is_broadcast {
        flags.push("BCAST");
    }
    if flags.is_empty() {
        "-".to_string()
    } else {
        flags.join(",")
    }
}

fn draw_connections_table(
    frame: &mut Frame,
    state: &mut AppState,
    area: Rect,
    entries: &[TrackedConnectionEntry],
    selected_id: Option<&str>,
    is_live_panel: bool,
    live_count: usize,
    stale_count: usize,
    filtered_count: usize,
) {
    let focused = state.focus_area == FocusArea::Main
        && state.current_page == Page::Connections
        && ((is_live_panel && state.connections_panel_focus == ConnectionsPanelFocus::LiveList)
            || (!is_live_panel
                && state.connections_panel_focus == ConnectionsPanelFocus::StaleList));

    let view_label = match state.connections_view {
        ConnectionsView::Summary => "Summary",
        ConnectionsView::Detail => "Detail",
    };
    let title = if is_live_panel {
        format!(
            "Live Connections ({}) | Live {} | Stale {} | Filtered {}",
            view_label, live_count, stale_count, filtered_count
        )
    } else {
        format!(
            "Stale Connections (> {}s) | Count {}",
            state.connections_live_window_secs, stale_count
        )
    };

    let view_height = area.height.saturating_sub(3).max(1);
    let scroll = if is_live_panel {
        &mut state.connections_live_scroll
    } else {
        &mut state.connections_stale_scroll
    };
    if is_live_panel {
        state.connections_live_list_view_height = view_height;
    } else {
        state.connections_stale_list_view_height = view_height;
    }
    let (start, end) = visible_window(entries.len(), scroll, view_height);
    let scroll_pos = *scroll;

    let header = match state.connections_view {
        ConnectionsView::Summary => Row::new(vec![
            Cell::from("Sel"),
            Cell::from("Endpoint"),
            Cell::from("Ch"),
            Cell::from("Ops"),
            Cell::from("Rate/s"),
            Cell::from("Age"),
            Cell::from("Flags"),
        ]),
        ConnectionsView::Detail => Row::new(vec![
            Cell::from("Sel"),
            Cell::from("Endpoint"),
            Cell::from("CID"),
            Cell::from("SID"),
            Cell::from("PV"),
            Cell::from("Fingerprint"),
            Cell::from("Rate/s"),
            Cell::from("Age"),
            Cell::from("Flags"),
        ]),
    }
    .style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );

    let rows = if entries.is_empty() {
        match state.connections_view {
            ConnectionsView::Summary => vec![Row::new(vec![
                Cell::from(""),
                Cell::from("No tracked connections"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
            ])],
            ConnectionsView::Detail => vec![Row::new(vec![
                Cell::from(""),
                Cell::from("No tracked channels"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("-"),
            ])],
        }
    } else {
        // Precomputed because the row closure cannot borrow `state` while the
        // scroll fields are mutably borrowed.
        let fingerprint_badges: Vec<String> = entries[start..end]
            .iter()
            .map(|e| state.fingerprint_badge(e.pv_name.as_deref()))
            .collect();

        entries[start..end]
            .iter()
            .enumerate()
            .map(|(row_index, entry)| {
                let is_selected = selected_id.is_some_and(|sid| sid == entry.id);
                let base_color = utils::connection_line_color(
                    entry.is_beacon,
                    entry.is_broadcast,
                    entry.last_seen_secs,
                );
                let style = if is_selected && focused {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if is_selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(base_color)
                };
                let endpoint = format!("{} <-> {}", entry.endpoint_a, entry.endpoint_b);
                let sel = if is_selected { ">" } else { "" };
                let flags = connection_flags(entry);
                match state.connections_view {
                    ConnectionsView::Summary => Row::new(vec![
                        Cell::from(sel.to_string()),
                        Cell::from(endpoint),
                        Cell::from(entry.channel_count.to_string()),
                        Cell::from(entry.operation_count.to_string()),
                        Cell::from(format!("{:.1}", entry.updates_per_sec)),
                        Cell::from(format!("{}s", entry.last_seen_secs)),
                        Cell::from(flags),
                    ])
                    .style(style),
                    ConnectionsView::Detail => {
                        let cid = entry
                            .cid
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string());
                        let sid = entry
                            .sid
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string());
                        let pv = entry.pv_name.clone().unwrap_or_else(|| "-".to_string());
                        Row::new(vec![
                            Cell::from(sel.to_string()),
                            Cell::from(endpoint),
                            Cell::from(cid),
                            Cell::from(sid),
                            Cell::from(pv),
                            Cell::from(
                                fingerprint_badges.get(row_index).cloned().unwrap_or_default(),
                            ),
                            Cell::from(format!("{:.1}", entry.updates_per_sec)),
                            Cell::from(format!("{}s", entry.last_seen_secs)),
                            Cell::from(flags),
                        ])
                        .style(style)
                    }
                }
            })
            .collect::<Vec<Row>>()
    };

    let widths = match state.connections_view {
        ConnectionsView::Summary => vec![
            Constraint::Length(3),
            Constraint::Min(20),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(12),
        ],
        ConnectionsView::Detail => vec![
            Constraint::Length(3),
            Constraint::Min(18),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Min(12),
            Constraint::Length(FINGERPRINT_BADGE_CELL_WIDTH as u16),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(12),
        ],
    };

    let table = Table::new(rows, widths)
        .header(header)
        .block(panel(&title, focused))
        .column_spacing(1);
    frame.render_widget(table, area);

    let mut scroll_state = scrollbar(entries.len(), scroll_pos);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        area,
        &mut scroll_state,
    );
}

fn draw_connections_detail(
    frame: &mut Frame,
    state: &mut AppState,
    area: Rect,
    selected: Option<&TrackedConnectionEntry>,
) {
    let focused = state.focus_area == FocusArea::Main
        && state.current_page == Page::Connections
        && state.connections_panel_focus == ConnectionsPanelFocus::Detail;

    let mut lines: Vec<Line> = Vec::new();
    if let Some(entry) = selected {
        lines.push(Line::from(Span::styled(
            format!("ID: {}", entry.id),
            Style::default().fg(Color::White),
        )));
        lines.push(Line::from(Span::raw(format!(
            "Endpoint: {} <-> {}",
            entry.endpoint_a, entry.endpoint_b
        ))));
        lines.push(Line::from(Span::raw(format!(
            "Rate: {:.1}/s | Age: {}s | Flags: {}",
            entry.updates_per_sec,
            entry.last_seen_secs,
            connection_flags(entry)
        ))));
        match state.connections_view {
            ConnectionsView::Summary => {
                lines.push(Line::from(Span::raw(format!(
                    "Channels: {} | Ops: {}",
                    entry.channel_count, entry.operation_count
                ))));
                if let Some(pv) = entry.pv_name.as_ref() {
                    lines.push(Line::from(Span::raw(format!("PVs: {}", pv))));
                }
            }
            ConnectionsView::Detail => {
                let cid = entry
                    .cid
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let sid = entry
                    .sid
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let pv = entry.pv_name.as_deref().unwrap_or("-");
                lines.push(Line::from(Span::raw(format!(
                    "CID: {} | SID: {} | PV: {}",
                    cid, sid, pv
                ))));
            }
        }
        lines.push(Line::from(Span::raw("")));
        lines.push(Line::from(Span::styled(
            "Recent Messages",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        if entry.recent_messages.is_empty() {
            lines.push(Line::from(Span::raw("No recent messages")));
        } else {
            for msg in &entry.recent_messages {
                let req = msg.split_whitespace().next().unwrap_or("");
                let color = utils::color_for_request_type(req);
                lines.push(Line::from(Span::styled(
                    msg.clone(),
                    Style::default().fg(color),
                )));
            }
        }
    } else {
        lines.push(Line::from(Span::raw("No selected connection")));
    }

    let paragraph = Paragraph::new(Text::from(lines))
        .block(panel("Connection Detail", focused))
        .wrap(Wrap { trim: true })
        .scroll((state.connections_detail_scroll, 0));
    frame.render_widget(paragraph, area);

    let line_count = selected
        .map(|entry| entry.recent_messages.len() + 8)
        .unwrap_or(1);
    let mut scroll_state = scrollbar(line_count, state.connections_detail_scroll);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        area,
        &mut scroll_state,
    );
}

fn draw_connections(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let partitions = state.connections_partitions();
    state.sync_connections_selection(&partitions.live, &partitions.stale);
    let selected = state.selected_connection(&partitions.live, &partitions.stale);
    let selected_id = selected.map(|entry| entry.id.as_str());

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Length(3), Constraint::Min(1)])
        .split(area);
    let view_label = match state.connections_view {
        ConnectionsView::Summary => "Summary",
        ConnectionsView::Detail => "Detail",
    };
    let quick_filter = format!(
        "View:{} | Sort:{} | Live<= {}s | Midstream:{} | Bcast/Beacon:{} | Stale:{} | Search={}{} | Live {} Stale {} Hidden {}",
        view_label,
        state.connections_sort_mode.label(),
        state.connections_live_window_secs,
        if state.show_midstream { "show" } else { "hide" },
        if state.filter_broadcast_beacon {
            "hide"
        } else {
            "show"
        },
        if state.connections_show_stale_panel {
            "show"
        } else {
            "hide"
        },
        if state.connections_search_text.is_empty() {
            "*"
        } else {
            state.connections_search_text.as_str()
        },
        if state.connections_search_edit {
            " (EDIT)"
        } else {
            ""
        },
        partitions.live.len(),
        partitions.stale.len(),
        partitions.filtered_out_total,
    );
    let quick_filter_panel = Paragraph::new(Text::raw(quick_filter))
        .block(panel("Quick Filters", false))
        .wrap(Wrap { trim: true });
    frame.render_widget(quick_filter_panel, layout[0]);

    let content = layout[1];
    let outer = if state.connections_show_stale_panel {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(content)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![Constraint::Percentage(100)])
            .split(content)
    };

    let top = outer[0];
    let top_split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Percentage(67), Constraint::Percentage(33)])
        .split(top);

    draw_connections_table(
        frame,
        state,
        top_split[0],
        &partitions.live,
        selected_id,
        true,
        partitions.live.len(),
        partitions.stale.len(),
        partitions.filtered_out_total,
    );
    draw_connections_detail(frame, state, top_split[1], selected);

    if state.connections_show_stale_panel {
        draw_connections_table(
            frame,
            state,
            outer[1],
            &partitions.stale,
            selected_id,
            false,
            partitions.live.len(),
            partitions.stale.len(),
            partitions.filtered_out_total,
        );
    }
}

fn draw_decoded(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Length(3), Constraint::Min(1)])
        .split(area);

    let enabled_req_types = state
        .event_filter
        .values()
        .filter(|enabled| **enabled)
        .count();
    let quick_filter = format!(
        "Sort:{} | type-select={}/{} | req={} pv={} src={} dst={} text(any)={} | / r v i o edit filters",
        state.decoded_sort_mode.label(),
        enabled_req_types,
        state.event_filter.len(),
        if state.decoded_filter_request_type.is_empty() {
            "*"
        } else {
            state.decoded_filter_request_type.as_str()
        },
        if state.decoded_filter_pv.is_empty() {
            "*"
        } else {
            state.decoded_filter_pv.as_str()
        },
        if state.decoded_filter_source_ip.is_empty() {
            "*"
        } else {
            state.decoded_filter_source_ip.as_str()
        },
        if state.decoded_filter_destination_ip.is_empty() {
            "*"
        } else {
            state.decoded_filter_destination_ip.as_str()
        },
        if state.decoded_filter_text.is_empty() {
            "*"
        } else {
            state.decoded_filter_text.as_str()
        },
    );
    let quick_filter_panel = Paragraph::new(Text::raw(quick_filter))
        .block(panel("Quick Filters", false))
        .wrap(Wrap { trim: true });
    frame.render_widget(quick_filter_panel, layout[0]);

    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(layout[1]);

    let rows = state.decoded_rows();
    state.sync_decoded_selection(&rows);
    let selected = state.selected_decoded_row(&rows);
    let selected_id = selected.map(|r| r.id.as_str());
    let decoded_context_chars = context_chars_for_table(split[0].width, &[14, 10, 16, 16]);

    let table_focused = state.focus_area == FocusArea::Main
        && state.current_page == Page::Decoded
        && state.decoded_panel_focus == super::state::DecodedPanelFocus::Table;
    state.decoded_table_view_height = split[0].height.saturating_sub(3).max(1);
    let (start, end) = visible_window(
        rows.len(),
        &mut state.decoded_table_scroll,
        state.decoded_table_view_height,
    );
    let header = Row::new(vec!["Time", "Req", "Source", "Destination", "Context"]).style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    let table_rows = if rows.is_empty() {
        vec![Row::new(vec![
            "-",
            "-",
            "-",
            "-",
            "No decoded messages match filters",
        ])]
    } else {
        rows[start..end]
            .iter()
            .map(|row| {
                let is_selected = selected_id.is_some_and(|id| id == row.id);
                let req_color = utils::color_for_request_type(&row.request_type);
                let style = if is_selected && table_focused {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if is_selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(req_color)
                };
                Row::new(vec![
                    Cell::from(row.ts.clone()),
                    Cell::from(row.request_type.clone()),
                    Cell::from(if state.decoded_show_endpoints {
                        row.source.clone()
                    } else {
                        "-".to_string()
                    }),
                    Cell::from(if state.decoded_show_endpoints {
                        row.destination.clone()
                    } else {
                        "-".to_string()
                    }),
                    Cell::from(truncate_for_width(&row.context_full, decoded_context_chars)),
                ])
                .style(style)
            })
            .collect::<Vec<Row>>()
    };
    let table_title = format!("Decoded Table ({})", rows.len());
    let table = Table::new(
        table_rows,
        vec![
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Min(24),
        ],
    )
    .header(header)
    .block(panel(&table_title, table_focused))
    .column_spacing(1);
    frame.render_widget(table, split[0]);
    let mut table_scroll_state = scrollbar(rows.len(), state.decoded_table_scroll);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        split[0],
        &mut table_scroll_state,
    );

    let detail_focused = state.focus_area == FocusArea::Main
        && state.current_page == Page::Decoded
        && state.decoded_panel_focus == super::state::DecodedPanelFocus::Detail;
    let mut detail_lines: Vec<Line> = Vec::new();
    if let Some(row) = selected {
        detail_lines.push(Line::from(Span::styled(
            format!("Time: {}", row.ts),
            Style::default().fg(Color::White),
        )));
        detail_lines.push(Line::from(Span::raw(format!(
            "Request: {}",
            row.request_type
        ))));
        detail_lines.push(Line::from(Span::raw(format!(
            "Source: {}",
            if state.decoded_show_endpoints {
                row.source.as_str()
            } else {
                "-"
            }
        ))));
        detail_lines.push(Line::from(Span::raw(format!(
            "Destination: {}",
            if state.decoded_show_endpoints {
                row.destination.as_str()
            } else {
                "-"
            }
        ))));
        detail_lines.push(Line::from(Span::raw(format!("Host: {}", row.host_name))));
        detail_lines.push(Line::from(Span::raw("")));
        detail_lines.push(Line::from(Span::styled(
            "Context",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        detail_lines.push(Line::from(Span::raw(row.context_full.clone())));
    } else {
        detail_lines.push(Line::from(Span::raw("No decoded row selected")));
    }
    let detail = Paragraph::new(Text::from(detail_lines))
        .block(panel("Decoded Detail", detail_focused))
        .wrap(Wrap { trim: true })
        .scroll((state.decoded_detail_scroll, 0));
    frame.render_widget(detail, split[1]);
    let detail_count = selected
        .map(|r| r.context_full.lines().count() + 8)
        .unwrap_or(1);
    let mut detail_scroll_state = scrollbar(detail_count, state.decoded_detail_scroll);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        split[1],
        &mut detail_scroll_state,
    );
}

fn draw_diagnostics(frame: &mut Frame, state: &AppState, area: Rect) {
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let stats = &state.if_stats_local;
    let data_rate = utils::format_rate(stats.data_rate);
    let packet_rate = format!("{:.2} pps", stats.packet_rate);

    let delta_channels_created = stats
        .pva_stats
        .channels_created
        .saturating_sub(stats.pva_stats_prev.channels_created);
    let delta_channels_destroyed = stats
        .pva_stats
        .channels_destroyed
        .saturating_sub(stats.pva_stats_prev.channels_destroyed);
    let delta_channels_expired = stats
        .pva_stats
        .channels_expired
        .saturating_sub(stats.pva_stats_prev.channels_expired);
    let delta_channels_evicted = stats
        .pva_stats
        .channels_evicted
        .saturating_sub(stats.pva_stats_prev.channels_evicted);
    let delta_operations_created = stats
        .pva_stats
        .operations_created
        .saturating_sub(stats.pva_stats_prev.operations_created);
    let delta_operations_completed = stats
        .pva_stats
        .operations_completed
        .saturating_sub(stats.pva_stats_prev.operations_completed);

    let mut left_lines: Vec<Line> = Vec::new();
    left_lines.push(Line::from(Span::styled(
        "Capture",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    left_lines.push(Line::from(Span::raw(format!(
        "Received: {} | Dropped: {} | IF Dropped: {}",
        stats.pcap_received, stats.pcap_dropped, stats.pcap_if_dropped
    ))));
    left_lines.push(Line::from(Span::raw(format!(
        "Data Rate: {} | Packet Rate: {}",
        data_rate, packet_rate
    ))));
    left_lines.push(Line::from(Span::raw("")));
    left_lines.push(Line::from(Span::styled(
        "PVA Tracker",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    left_lines.push(Line::from(Span::raw(format!(
        "Active Connections: {} | Active Channels: {}",
        stats.pva_total_connections, stats.pva_total_channels
    ))));
    left_lines.push(Line::from(Span::raw(format!(
        "Created: {} | Destroyed: {} | Expired: {} | Evicted: {}",
        stats.pva_stats.channels_created,
        stats.pva_stats.channels_destroyed,
        stats.pva_stats.channels_expired,
        stats.pva_stats.channels_evicted,
    ))));
    left_lines.push(Line::from(Span::raw(format!(
        "Ops Created: {} | Completed: {} | Expired: {}",
        stats.pva_stats.operations_created,
        stats.pva_stats.operations_completed,
        stats.pva_stats.operations_expired,
    ))));
    left_lines.push(Line::from(Span::raw(format!(
        "Tracker Memory: {:.1} MiB",
        stats.pva_stats.memory_bytes as f64 / (1024.0 * 1024.0)
    ))));
    // Only shown once the ceiling has actually bitten; a quiet line here is
    // the normal state and would otherwise be four counters of zero.
    let shed_total = stats.pva_stats.shed_placeholder_ops
        + stats.pva_stats.shed_message_rings
        + stats.pva_stats.shed_search_entries
        + stats.pva_stats.shed_channels;
    if shed_total > 0 {
        left_lines.push(Line::from(Span::raw(format!(
            "Shed: placeholders {} | rings {} | search {}",
            stats.pva_stats.shed_placeholder_ops,
            stats.pva_stats.shed_message_rings,
            stats.pva_stats.shed_search_entries,
        ))));
    }
    // Channels are the last thing shed, and shedding them is the only tier
    // that costs introspection we cannot cheaply reacquire. Say so loudly.
    if stats.pva_stats.shed_channels > 0 {
        left_lines.push(Line::from(Span::styled(
            format!(
                "[!] Memory ceiling reached: {} channels shed, introspection lost",
                stats.pva_stats.shed_channels
            ),
            Style::default().fg(Color::Yellow),
        )));
    }
    left_lines.push(Line::from(Span::raw(format!(
        "Search Cache: {} | Resolved: {} | Retroactive: {}",
        stats.pva_stats.search_cache_entries,
        stats.pva_stats.search_responses_resolved,
        stats.pva_stats.search_retroactive_resolves,
    ))));
    left_lines.push(Line::from(Span::raw(format!(
        "PVA Direction: Client(C>): {} | Server(S>): {}",
        stats.pva_stats.client_messages,
        stats.pva_stats.server_messages,
    ))));
    // Show a warning when we see server traffic but zero client traffic,
    // which means outgoing packets aren't being captured (common Npcap issue)
    if stats.pva_stats.server_messages > 0 && stats.pva_stats.client_messages == 0 {
        left_lines.push(Line::from(Span::styled(
            "[!] No outgoing (client) PVA traffic captured - PV names may be unresolvable",
            Style::default().fg(Color::Yellow),
        )));
        left_lines.push(Line::from(Span::styled(
            "    Check Npcap config: reinstall with 'WinPcap API-Compatible Mode' enabled",
            Style::default().fg(Color::Yellow),
        )));
    }
    left_lines.push(Line::from(Span::raw("")));
    left_lines.push(Line::from(Span::styled(
        "PVA Deltas (per tick)",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    left_lines.push(Line::from(Span::raw(format!(
        "Ch +{} -{} exp {} evict {}",
        delta_channels_created,
        delta_channels_destroyed,
        delta_channels_expired,
        delta_channels_evicted
    ))));
    left_lines.push(Line::from(Span::raw(format!(
        "Ops +{} -{}",
        delta_operations_created, delta_operations_completed
    ))));
    left_lines.push(Line::from(Span::raw("")));
    left_lines.push(Line::from(Span::styled(
        "Segmentation",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    left_lines.push(Line::from(Span::raw(format!(
        "Segments Started: {} | Completed: {}",
        stats.pva_seg_started, stats.pva_seg_completed
    ))));
    left_lines.push(Line::from(Span::raw(format!(
        "Segments Dropped: {} | Stream Drops: {}",
        stats.pva_seg_dropped, stats.pva_stream_dropped
    ))));
    left_lines.push(Line::from(Span::raw(format!(
        "Drop short: {} | invalid: {}",
        stats.pva_seg_drop_short, stats.pva_seg_drop_invalid
    ))));
    left_lines.push(Line::from(Span::raw(format!(
        "Drop partial: {} | missing-first: {}",
        stats.pva_seg_drop_partial, stats.pva_seg_drop_missing_first
    ))));

    let left_panel = Paragraph::new(Text::from(left_lines))
        .block(panel("Capture + PVA", false))
        .wrap(Wrap { trim: true });
    frame.render_widget(left_panel, split[0]);

    let midstream_connections = stats
        .tracked_connections_summary
        .iter()
        .filter(|entry| entry.mid_stream)
        .count();
    let midstream_channels = stats
        .tracked_connections_detail
        .iter()
        .filter(|entry| entry.mid_stream)
        .count();

    let mut unresolved = stats
        .tracked_connections_detail
        .iter()
        .filter(|entry| {
            entry
                .pv_name
                .as_deref()
                .unwrap_or("")
                .starts_with("<unknown")
        })
        .collect::<Vec<_>>();
    unresolved.sort_by_key(|entry| entry.last_seen_secs);

    let mut right_lines: Vec<Line> = Vec::new();
    right_lines.push(Line::from(Span::styled(
        "Midstream + Unresolved",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    right_lines.push(Line::from(Span::raw(format!(
        "Midstream Connections: {} | Midstream Channels: {}",
        midstream_connections, midstream_channels
    ))));
    right_lines.push(Line::from(Span::raw("")));

    let list_limit = 12usize;
    if unresolved.is_empty() {
        right_lines.push(Line::from(Span::raw(
            "No unresolved PVs in tracked channels",
        )));
    } else {
        right_lines.push(Line::from(Span::styled(
            format!("Unresolved PVs ({} shown)", list_limit.min(unresolved.len())),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        let max_chars = split[1].width.saturating_sub(2) as usize;
        for entry in unresolved.into_iter().take(list_limit) {
            let endpoint = format!("{} <-> {}", entry.endpoint_a, entry.endpoint_b);
            let cid = entry
                .cid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string());
            let sid = entry
                .sid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string());
            let pv = entry.pv_name.as_deref().unwrap_or("-");
            let line = format!(
                "{}s | {} | cid {} sid {} | {}",
                entry.last_seen_secs, endpoint, cid, sid, pv
            );
            right_lines.push(Line::from(Span::raw(truncate_for_width(
                &line,
                max_chars,
            ))));
        }
    }

    let right_split = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vec![Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(split[1]);

    let right_panel = Paragraph::new(Text::from(right_lines))
        .block(panel("Fault Finder", false))
        .wrap(Wrap { trim: true });
    frame.render_widget(right_panel, right_split[0]);

    let log_lines = if stats.pva_seg_log.is_empty() {
        vec![Line::from(Span::raw("No segment log entries"))]
    } else {
        stats
            .pva_seg_log
            .iter()
            .rev()
            .take(12)
            .map(|line| Line::from(Span::raw(line.clone())))
            .collect::<Vec<Line>>()
    };
    let log_panel = Paragraph::new(Text::from(log_lines))
        .block(panel("Segmentation Log", false))
        .wrap(Wrap { trim: true });
    frame.render_widget(log_panel, right_split[1]);
}

fn draw_legend(frame: &mut Frame, state: &AppState, area: Rect) {
    let legend = match state.current_page {
        Page::Dashboard => {
            "1 Dashboard | 2 Connections | 3 Decoded | 4 Diagnostics | 5 Fingerprints | q Quit | h Help | i Next IF | I Pick IF | Left/Right Panel Focus | Up/Down Navigate | PgUp/PgDn Fast Scroll | Space Select"
        }
        Page::Connections => {
            "1 Dashboard | 2 Connections | 3 Decoded | 4 Diagnostics | 5 Fingerprints | q Quit | h Help | i Next IF | I Pick IF | Left/Right or Tab Cycle Panels | Up/Down Navigate | PgUp/PgDn Fast Scroll | s Sort | l Live Window | t Stale Panel | / Search"
        }
        Page::Decoded => {
            "1 Dashboard | 2 Connections | 3 Decoded | 4 Diagnostics | 5 Fingerprints | q Quit | h Help | I Pick IF | Left/Right or Tab Cycle Panels | Up/Down Navigate | PgUp/PgDn Fast Scroll | / (anywhere) r v i o Edit Filters"
        }
        Page::Diagnostics => {
            "1 Dashboard | 2 Connections | 3 Decoded | 4 Diagnostics | 5 Fingerprints | q Quit | h Help | i Next IF | I Pick IF | Left/Right Panel Focus"
        }
        Page::Fingerprints => {
            "1 Dashboard | 2 Connections | 3 Decoded | 4 Diagnostics | 5 Fingerprints | q Quit | h Help | I Pick IF | Left/Right Panel Focus | Up/Down Navigate | v Cycle View"
        }
    };

    let paragraph = Paragraph::new(Text::raw(legend))
        .block(Block::default().title("Legend").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn draw_status_indicator(frame: &mut Frame, state: &AppState, area: Rect) {
    let pause_span = if state.pause {
        Span::styled(
            "PAUSED",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "RUNNING",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    };

    let lag_span = if state.pause {
        Span::styled("Lag --", Style::default().fg(Color::DarkGray))
    } else if state.ui_is_lagging {
        Span::styled(
            format!(
                "LAG {}ms>{}",
                state.ui_last_tick_lag_ms, state.ui_tick_rate_ms
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!("Lag {}ms", state.ui_last_tick_lag_ms),
            Style::default().fg(Color::Green),
        )
    };

    let mut spans = vec![pause_span, Span::raw(" | "), lag_span];
    if let Some(label) = state.current_interface_label() {
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(label, Style::default().fg(Color::Cyan)));
    }
    let status_line = Line::from(spans);
    let paragraph = Paragraph::new(Text::from(status_line))
        .block(Block::default().title("Status").borders(Borders::ALL))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn draw_interface_overlay(frame: &mut Frame, state: &AppState) {
    let selection = match state.interface_selection.as_ref() {
        Some(selection) => selection,
        None => return,
    };
    let area = utils::centered_rect(70, 60, frame.area());
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Interface Picker",
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::raw("Use Up/Down, Enter to select, Esc to cancel")));
    lines.push(Line::from(Span::raw("")));

    for (idx, iface) in selection.interfaces.iter().enumerate() {
        let is_selected = idx == state.interface_menu_index;
        let prefix = if is_selected { "-> " } else { "   " };
        let label = if iface.description.is_empty() {
            format!("{}{}", prefix, iface.name)
        } else {
            format!("{}{} - {}", prefix, iface.name, iface.description)
        };
        let style = if is_selected {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(label, style)));
    }

    let block = Block::default().title("Interfaces").borders(Borders::ALL);
    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn draw_side_panel(frame: &mut Frame, state: &AppState, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let focus_label = if state.focus_area == FocusArea::Options {
        "Options (focus)"
    } else {
        "Options"
    };
    lines.push(Line::from(Span::styled(
        focus_label,
        Style::default().fg(Color::White),
    )));
    lines.push(Line::from(Span::raw("")));

    let items = state.current_options();
    let selected = state
        .current_options_index()
        .min(items.len().saturating_sub(1));
    let focused = state.focus_area == FocusArea::Options;

    for (idx, item) in items.iter().enumerate() {
        if item.kind == OptionKind::Section {
            lines.push(Line::from(Span::styled(
                "Event Filters",
                Style::default().fg(Color::White),
            )));
            continue;
        }

        let is_selected = idx == selected;
        let prefix = if is_selected { "-> " } else { "   " };
        let base_color = if is_selected && focused {
            Color::Yellow
        } else if is_selected {
            Color::Gray
        } else if focused {
            Color::White
        } else {
            Color::DarkGray
        };

        let label = match item.id {
            OptionId::Pause => format!("[{}] Pause", if state.pause { "x" } else { " " }),
            OptionId::ShowHost => {
                format!("[{}] Show Host", if state.show_host { "x" } else { " " })
            }
            OptionId::ResetCharts => "Reset Charts".to_string(),
            OptionId::EventFilter(idx) => {
                let key = state
                    .event_filter_keys
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                let enabled = *state.event_filter.get(&key).unwrap_or(&false);
                format!("[{}] {}", if enabled { "x" } else { " " }, key)
            }
            OptionId::ConnView => {
                let view = match state.connections_view {
                    super::state::ConnectionsView::Summary => "Summary",
                    super::state::ConnectionsView::Detail => "Detail",
                };
                format!("View: {}", view)
            }
            OptionId::ConnMidstream => {
                format!(
                    "[{}] Mid-stream",
                    if state.show_midstream { "x" } else { " " }
                )
            }
            OptionId::ConnBroadcast => format!(
                "[{}] Filter bcast/beacon",
                if state.filter_broadcast_beacon {
                    "x"
                } else {
                    " "
                }
            ),
            OptionId::ConnSort => {
                format!("Sort: {}", state.connections_sort_mode.label())
            }
            OptionId::ConnLiveWindow => {
                format!("Live Window: {}s", state.connections_live_window_secs)
            }
            OptionId::ConnShowStale => format!(
                "[{}] Show Stale Panel",
                if state.connections_show_stale_panel {
                    "x"
                } else {
                    " "
                }
            ),
            OptionId::ConnSearch => {
                let edit = if state.connections_search_edit {
                    " (EDIT)"
                } else {
                    ""
                };
                let value = if state.connections_search_text.is_empty() {
                    "(empty)"
                } else {
                    state.connections_search_text.as_str()
                };
                format!("Search: {}{}", value, edit)
            }
            OptionId::ConnClearSearch => "Clear Search".to_string(),
            OptionId::DecFilter(field) => {
                let (label, value) = match field {
                    FilterField::RequestType => {
                        ("request_type", &state.decoded_filter_request_type)
                    }
                    FilterField::PvName => ("pv_name", &state.decoded_filter_pv),
                    FilterField::SourceIp => ("source_ip", &state.decoded_filter_source_ip),
                    FilterField::DestinationIp => {
                        ("destination_ip", &state.decoded_filter_destination_ip)
                    }
                    FilterField::Text => ("text", &state.decoded_filter_text),
                };
                let edit = if state.decoded_edit_mode && state.decoded_filter_field == field {
                    " (EDIT)"
                } else {
                    ""
                };
                format!("{}: {}{}", label, value, edit)
            }
            OptionId::DecShowEndpoints => format!(
                "[{}] Show Src/Dst",
                if state.decoded_show_endpoints {
                    "x"
                } else {
                    " "
                }
            ),
            OptionId::DecSort => format!("Sort: {}", state.decoded_sort_mode.label()),
            OptionId::DecQuickFilter => "Edit Text Filter".to_string(),
            OptionId::DecClearFilters => "Clear Filters".to_string(),
            OptionId::FpView => format!("View: {}", state.fingerprints_view.label()),
            OptionId::FpShowPending => format!(
                "[{}] Show Pending",
                if state.fingerprints_show_pending { "x" } else { " " }
            ),
        };

        lines.push(Line::from(Span::styled(
            format!("{}{}", prefix, label),
            Style::default().fg(base_color),
        )));
    }

    lines.push(Line::from(Span::raw("")));

    if state.current_page == Page::Connections {
        lines.push(Line::from(Span::styled(
            "Colors",
            Style::default().fg(Color::White),
        )));
        lines.push(Line::from(Span::styled(
            "• BEACON/BCAST",
            Style::default().fg(Color::Blue),
        )));
        lines.push(Line::from(Span::styled(
            "• Fresh",
            Style::default().fg(Color::Green),
        )));
        lines.push(Line::from(Span::styled(
            "• Aging",
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(Span::styled(
            "• Stale",
            Style::default().fg(Color::Red),
        )));
    }

    if state.current_page == Page::Decoded {
        lines.push(Line::from(Span::styled(
            "Colors",
            Style::default().fg(Color::White),
        )));
        let req_colors = [
            ("GET", "GET"),
            ("PUT", "PUT"),
            ("PUT_GET", "PUT_GET"),
            ("MONITOR", "MONITOR"),
            ("ARRAY", "ARRAY"),
            ("PROCESS", "PROCESS"),
            ("RPC", "RPC"),
            ("BEACON", "BEACON"),
        ];
        for (label, key) in req_colors {
            let color = utils::color_for_request_type(key);
            lines.push(Line::from(Span::styled(
                format!("• {}", label),
                Style::default().fg(color),
            )));
        }
    }

    let paragraph = Paragraph::new(Text::from(lines))
        .block(
            Block::default()
                .title("Options")
                .borders(Borders::ALL)
                .border_style(focus_border_style(state.focus_area == FocusArea::Options)),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}
