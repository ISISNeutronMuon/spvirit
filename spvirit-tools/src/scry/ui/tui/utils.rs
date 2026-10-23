use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
};

pub const REQUEST_TYPES: &[&str] = &[
    "BEACON",
    "CONNECTION_VALIDATION",
    "ECHO",
    "SEARCH",
    "SEARCH_RESPONSE",
    "AUTHNZ",
    "ACL_CHANGE",
    "CREATE_CHANNEL",
    "DESTROY_CHANNEL",
    "CONNECTION_VALIDATED",
    "GET",
    "PUT",
    "PUT_GET",
    "MONITOR",
    "ARRAY",
    "DESTROY_REQUEST",
    "PROCESS",
    "GET_FIELD",
    "MESSAGE",
    "MULTIPLE_DATA",
    "RPC",
    "CANCEL_REQUEST",
    "ORIGIN_TAG",
];

pub fn format_rate(rate: f64) -> String {
    if rate > 1_000_000.0 {
        format!("{:.2} Mb/s", rate / 1_000_000.0)
    } else if rate > 1_000.0 {
        format!("{:.2} Kb/s", rate / 1_000.0)
    } else {
        format!("{:.2} b/s", rate)
    }
}

pub fn format_packet_rate(packet_rate: f64) -> String {
    format!("Packet Rate: {:.2} pps", packet_rate)
}

pub fn color_for_request_type(req: &str) -> Color {
    match req {
        "SEARCH" => Color::Yellow,
        "SEARCH_RESPONSE" => Color::LightYellow,
        "GET" => Color::Cyan,
        "PUT" => Color::Green,
        "PUT_GET" => Color::LightGreen,
        "MONITOR" => Color::Magenta,
        "ARRAY" => Color::Blue,
        "PROCESS" => Color::LightBlue,
        "GET_FIELD" => Color::LightCyan,
        "MESSAGE" => Color::LightMagenta,
        "MULTIPLE_DATA" => Color::LightBlue,
        "RPC" => Color::LightRed,
        "DESTROY_REQUEST" => Color::Red,
        "CANCEL_REQUEST" => Color::Red,
        "BEACON" => Color::Blue,
        "CONNECTION_VALIDATION" => Color::LightGreen,
        "CONNECTION_VALIDATED" => Color::Green,
        "AUTHNZ" => Color::LightGreen,
        "ACL_CHANGE" => Color::LightGreen,
        "ORIGIN_TAG" => Color::Gray,
        _ => Color::White,
    }
}

pub fn colorize_lines_by_req(lines: &[(String, String)]) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    for (line, req_type) in lines {
        let color = color_for_request_type(req_type);
        out.push(Line::from(Span::styled(
            line.clone(),
            Style::default().fg(color),
        )));
    }
    out
}

pub fn extract_req_from_line(line: &str) -> String {
    let mut iter = line.split_whitespace();
    let first = iter.next().unwrap_or("");
    if color_for_request_type(first) != Color::White {
        return first.to_string();
    }
    iter.next().unwrap_or("").to_string()
}

pub fn staleness_color(last_seen_secs: u64) -> Color {
    if last_seen_secs <= 2 {
        Color::Green
    } else if last_seen_secs <= 10 {
        Color::Yellow
    } else {
        Color::Red
    }
}

pub fn connection_line_color(is_beacon: bool, is_broadcast: bool, last_seen_secs: u64) -> Color {
    if is_beacon {
        return Color::Blue;
    }
    if is_broadcast {
        return Color::Blue;
    }
    staleness_color(last_seen_secs)
}

pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints(vec![
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    let horizontal_layout = Layout::default()
        .direction(ratatui::layout::Direction::Horizontal)
        .constraints(vec![
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1]);

    horizontal_layout[1]
}
