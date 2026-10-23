use ratatui::crossterm::event::KeyCode;

use super::options::OptionKind;
use super::state::{
    AppState, ConnectionsPanelFocus, DecodedPanelFocus, FilterField, FingerprintsView, FocusArea,
    Page,
};

#[derive(Debug, PartialEq, Eq)]
pub enum InputOutcome {
    Quit,
    Handled,
    Continue,
}

fn update_scroll_for_selection(scroll: &mut u16, selection: usize, view_height: u16) {
    let vh = view_height.max(1) as usize;
    let current = *scroll as usize;
    if selection < current {
        *scroll = selection as u16;
    } else if selection >= current + vh {
        *scroll = selection.saturating_sub(vh.saturating_sub(1)) as u16;
    }
}

fn page_step(view_height: u16) -> i32 {
    view_height.saturating_sub(1).max(1) as i32
}

fn page_jump_index(current: i32, len: i32, step: i32, forward: bool) -> usize {
    if len <= 0 {
        return 0;
    }
    let current = current.clamp(0, len - 1);
    let next = if forward {
        (current + step).min(len - 1)
    } else {
        (current - step).max(0)
    };
    next as usize
}

fn cycle_focus_forward(state: &mut AppState) {
    if state.current_page == Page::Connections {
        if state.focus_area == FocusArea::Options {
            state.focus_area = FocusArea::Main;
            state.connections_panel_focus = ConnectionsPanelFocus::LiveList;
        } else {
            match state.connections_panel_focus {
                ConnectionsPanelFocus::LiveList => {
                    state.connections_panel_focus = ConnectionsPanelFocus::Detail;
                }
                ConnectionsPanelFocus::Detail => {
                    if state.connections_show_stale_panel {
                        state.connections_panel_focus = ConnectionsPanelFocus::StaleList;
                    } else {
                        state.focus_area = FocusArea::Options;
                    }
                }
                ConnectionsPanelFocus::StaleList => {
                    state.focus_area = FocusArea::Options;
                }
            }
        }
    } else if state.current_page == Page::Decoded {
        if state.focus_area == FocusArea::Options {
            state.focus_area = FocusArea::Main;
            state.decoded_panel_focus = DecodedPanelFocus::Table;
        } else {
            match state.decoded_panel_focus {
                DecodedPanelFocus::Table => {
                    state.decoded_panel_focus = DecodedPanelFocus::Detail;
                }
                DecodedPanelFocus::Detail => {
                    state.focus_area = FocusArea::Options;
                }
            }
        }
    } else {
        state.focus_area = match state.focus_area {
            FocusArea::Main => FocusArea::Options,
            FocusArea::Options => FocusArea::Main,
        };
        if state.current_page == Page::Dashboard {
            state.focus_area = FocusArea::Options;
        }
        if state.current_page == Page::Diagnostics {
            state.focus_area = FocusArea::Main;
        }
    }
    state.normalize_options_index();
}

fn cycle_focus_backward(state: &mut AppState) {
    if state.current_page == Page::Connections {
        if state.focus_area == FocusArea::Options {
            state.focus_area = FocusArea::Main;
            state.connections_panel_focus = if state.connections_show_stale_panel {
                ConnectionsPanelFocus::StaleList
            } else {
                ConnectionsPanelFocus::Detail
            };
        } else {
            match state.connections_panel_focus {
                ConnectionsPanelFocus::LiveList => {
                    state.focus_area = FocusArea::Options;
                }
                ConnectionsPanelFocus::Detail => {
                    state.connections_panel_focus = ConnectionsPanelFocus::LiveList;
                }
                ConnectionsPanelFocus::StaleList => {
                    state.connections_panel_focus = ConnectionsPanelFocus::Detail;
                }
            }
        }
    } else if state.current_page == Page::Decoded {
        if state.focus_area == FocusArea::Options {
            state.focus_area = FocusArea::Main;
            state.decoded_panel_focus = DecodedPanelFocus::Detail;
        } else {
            match state.decoded_panel_focus {
                DecodedPanelFocus::Table => {
                    state.focus_area = FocusArea::Options;
                }
                DecodedPanelFocus::Detail => {
                    state.decoded_panel_focus = DecodedPanelFocus::Table;
                }
            }
        }
    } else {
        state.focus_area = if state.current_page == Page::Diagnostics {
            FocusArea::Main
        } else {
            FocusArea::Options
        };
    }
    state.normalize_options_index();
}

pub fn handle_key_event(state: &mut AppState, key: KeyCode) -> InputOutcome {
    if key == KeyCode::Char('q') {
        return InputOutcome::Quit;
    }

    if state.interface_menu_open {
        match key {
            KeyCode::Esc => {
                state.close_interface_menu();
            }
            KeyCode::Enter => {
                state.apply_interface_menu_selection();
            }
            KeyCode::Up => {
                state.move_interface_menu_selection(-1);
            }
            KeyCode::Down => {
                state.move_interface_menu_selection(1);
            }
            KeyCode::PageUp => {
                for _ in 0..8 {
                    state.move_interface_menu_selection(-1);
                }
            }
            KeyCode::PageDown => {
                for _ in 0..8 {
                    state.move_interface_menu_selection(1);
                }
            }
            _ => {}
        }
        return InputOutcome::Handled;
    }

    if state.current_page == Page::Connections && state.connections_search_edit {
        match key {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Tab => {
                state.connections_search_edit = false;
            }
            KeyCode::Backspace => {
                state.connections_search_text.pop();
            }
            KeyCode::Char(c) => {
                state.connections_search_text.push(c);
            }
            _ => {}
        }
        return InputOutcome::Handled;
    }

    if state.current_page == Page::Decoded && state.decoded_edit_mode {
        match key {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Tab => {
                state.decoded_edit_mode = false;
            }
            KeyCode::Backspace => {
                state.active_filter_mut().pop();
            }
            KeyCode::Char(c) => {
                state.active_filter_mut().push(c);
            }
            _ => {}
        }
        return InputOutcome::Handled;
    }

    if key == KeyCode::Char('1') {
        state.current_page = Page::Dashboard;
        state.focus_area = FocusArea::Options;
        state.connections_search_edit = false;
        state.decoded_edit_mode = false;
        state.normalize_options_index();
        return InputOutcome::Handled;
    }
    if key == KeyCode::Char('2') {
        state.current_page = Page::Connections;
        state.focus_area = FocusArea::Main;
        state.connections_panel_focus = ConnectionsPanelFocus::LiveList;
        state.connections_search_edit = false;
        state.decoded_edit_mode = false;
        state.normalize_options_index();
        return InputOutcome::Handled;
    }
    if key == KeyCode::Char('3') {
        state.current_page = Page::Decoded;
        state.focus_area = FocusArea::Main;
        state.decoded_panel_focus = DecodedPanelFocus::Table;
        state.connections_search_edit = false;
        state.decoded_edit_mode = false;
        state.normalize_options_index();
        return InputOutcome::Handled;
    }
    if key == KeyCode::Char('4') {
        state.current_page = Page::Diagnostics;
        state.focus_area = FocusArea::Main;
        state.connections_search_edit = false;
        state.decoded_edit_mode = false;
        state.normalize_options_index();
        return InputOutcome::Handled;
    }
    if key == KeyCode::Char('5') {
        state.current_page = Page::Fingerprints;
        state.focus_area = FocusArea::Main;
        state.connections_search_edit = false;
        state.decoded_edit_mode = false;
        state.normalize_options_index();
        return InputOutcome::Handled;
    }

    if key == KeyCode::Char('p') {
        state.pause = !state.pause;
    }
    if key == KeyCode::Char('H') {
        state.show_host = !state.show_host;
    }
    if key == KeyCode::Char('h') {
        state.show_help = !state.show_help;
    }
    if key == KeyCode::Char('I') {
        state.open_interface_menu();
        return InputOutcome::Handled;
    }
    if key == KeyCode::Char('i') && state.current_page != Page::Decoded {
        if state.cycle_interface() {
            return InputOutcome::Handled;
        }
    }

    if state.current_page == Page::Connections {
        match key {
            KeyCode::Char('/') => {
                state.connections_search_edit = true;
                return InputOutcome::Handled;
            }
            KeyCode::Char('s') => {
                state.cycle_connections_sort_mode();
                return InputOutcome::Handled;
            }
            KeyCode::Char('t') => {
                state.connections_show_stale_panel = !state.connections_show_stale_panel;
                if !state.connections_show_stale_panel
                    && state.connections_panel_focus == ConnectionsPanelFocus::StaleList
                {
                    state.connections_panel_focus = ConnectionsPanelFocus::LiveList;
                }
                return InputOutcome::Handled;
            }
            KeyCode::Char('l') => {
                state.cycle_connections_live_window();
                return InputOutcome::Handled;
            }
            _ => {}
        }
    }

    if state.current_page == Page::Decoded {
        match key {
            KeyCode::Char('/') => {
                state.decoded_filter_field = FilterField::Text;
                state.decoded_edit_mode = true;
                return InputOutcome::Handled;
            }
            KeyCode::Char('r') => {
                state.decoded_filter_field = FilterField::RequestType;
                state.decoded_edit_mode = true;
                return InputOutcome::Handled;
            }
            KeyCode::Char('v') => {
                state.decoded_filter_field = FilterField::PvName;
                state.decoded_edit_mode = true;
                return InputOutcome::Handled;
            }
            KeyCode::Char('i') => {
                state.decoded_filter_field = FilterField::SourceIp;
                state.decoded_edit_mode = true;
                return InputOutcome::Handled;
            }
            KeyCode::Char('o') => {
                state.decoded_filter_field = FilterField::DestinationIp;
                state.decoded_edit_mode = true;
                return InputOutcome::Handled;
            }
            _ => {}
        }
    }

    if state.current_page == Page::Fingerprints && key == KeyCode::Char('v') {
        state.fingerprints_view = state.fingerprints_view.next();
        state.fingerprints_scroll = 0;
        return InputOutcome::Handled;
    }

    if key == KeyCode::Tab || key == KeyCode::Right {
        cycle_focus_forward(state);
        return InputOutcome::Handled;
    }
    if key == KeyCode::Left {
        cycle_focus_backward(state);
        return InputOutcome::Handled;
    }

    if state.focus_area == FocusArea::Options {
        if key == KeyCode::Up {
            state.move_options_selection(-1);
            return InputOutcome::Handled;
        }
        if key == KeyCode::Down {
            state.move_options_selection(1);
            return InputOutcome::Handled;
        }
        if key == KeyCode::PageUp {
            for _ in 0..8 {
                state.move_options_selection(-1);
            }
            return InputOutcome::Handled;
        }
        if key == KeyCode::PageDown {
            for _ in 0..8 {
                state.move_options_selection(1);
            }
            return InputOutcome::Handled;
        }
        if key == KeyCode::Char(' ') {
            let items = state.current_options();
            if !items.is_empty() {
                let idx = state.current_options_index().min(items.len() - 1);
                let item = items[idx];
                if item.kind != OptionKind::Section {
                    state.activate_option(item);
                }
            }
            return InputOutcome::Handled;
        }
    } else if state.focus_area == FocusArea::Main {
        if state.current_page == Page::Connections {
            let partitions = state.connections_partitions();
            state.sync_connections_selection(&partitions.live, &partitions.stale);

            match state.connections_panel_focus {
                ConnectionsPanelFocus::LiveList => {
                    let live_len = partitions.live.len() as i32;
                    if key == KeyCode::Up && live_len > 0 {
                        let next = if state.connections_live_selected_index <= 0 {
                            live_len - 1
                        } else {
                            state.connections_live_selected_index - 1
                        };
                        let next_idx = next as usize;
                        state.set_selected_live(next_idx, &partitions.live);
                        update_scroll_for_selection(
                            &mut state.connections_live_scroll,
                            next_idx,
                            state.connections_live_list_view_height,
                        );
                        state.connections_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::Down && live_len > 0 {
                        let next = if state.connections_live_selected_index >= live_len - 1 {
                            0
                        } else {
                            state.connections_live_selected_index + 1
                        };
                        let next_idx = next as usize;
                        state.set_selected_live(next_idx, &partitions.live);
                        update_scroll_for_selection(
                            &mut state.connections_live_scroll,
                            next_idx,
                            state.connections_live_list_view_height,
                        );
                        state.connections_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::PageUp && live_len > 0 {
                        let step = page_step(state.connections_live_list_view_height);
                        let next_idx = page_jump_index(
                            state.connections_live_selected_index,
                            live_len,
                            step,
                            false,
                        );
                        state.set_selected_live(next_idx, &partitions.live);
                        update_scroll_for_selection(
                            &mut state.connections_live_scroll,
                            next_idx,
                            state.connections_live_list_view_height,
                        );
                        state.connections_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::PageDown && live_len > 0 {
                        let step = page_step(state.connections_live_list_view_height);
                        let next_idx = page_jump_index(
                            state.connections_live_selected_index,
                            live_len,
                            step,
                            true,
                        );
                        state.set_selected_live(next_idx, &partitions.live);
                        update_scroll_for_selection(
                            &mut state.connections_live_scroll,
                            next_idx,
                            state.connections_live_list_view_height,
                        );
                        state.connections_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                }
                ConnectionsPanelFocus::StaleList => {
                    let stale_len = partitions.stale.len() as i32;
                    if key == KeyCode::Up && stale_len > 0 {
                        let next = if state.connections_stale_selected_index <= 0 {
                            stale_len - 1
                        } else {
                            state.connections_stale_selected_index - 1
                        };
                        let next_idx = next as usize;
                        state.set_selected_stale(next_idx, &partitions.stale);
                        update_scroll_for_selection(
                            &mut state.connections_stale_scroll,
                            next_idx,
                            state.connections_stale_list_view_height,
                        );
                        state.connections_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::Down && stale_len > 0 {
                        let next = if state.connections_stale_selected_index >= stale_len - 1 {
                            0
                        } else {
                            state.connections_stale_selected_index + 1
                        };
                        let next_idx = next as usize;
                        state.set_selected_stale(next_idx, &partitions.stale);
                        update_scroll_for_selection(
                            &mut state.connections_stale_scroll,
                            next_idx,
                            state.connections_stale_list_view_height,
                        );
                        state.connections_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::PageUp && stale_len > 0 {
                        let step = page_step(state.connections_stale_list_view_height);
                        let next_idx = page_jump_index(
                            state.connections_stale_selected_index,
                            stale_len,
                            step,
                            false,
                        );
                        state.set_selected_stale(next_idx, &partitions.stale);
                        update_scroll_for_selection(
                            &mut state.connections_stale_scroll,
                            next_idx,
                            state.connections_stale_list_view_height,
                        );
                        state.connections_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::PageDown && stale_len > 0 {
                        let step = page_step(state.connections_stale_list_view_height);
                        let next_idx = page_jump_index(
                            state.connections_stale_selected_index,
                            stale_len,
                            step,
                            true,
                        );
                        state.set_selected_stale(next_idx, &partitions.stale);
                        update_scroll_for_selection(
                            &mut state.connections_stale_scroll,
                            next_idx,
                            state.connections_stale_list_view_height,
                        );
                        state.connections_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                }
                ConnectionsPanelFocus::Detail => {
                    if key == KeyCode::Up {
                        state.connections_detail_scroll =
                            state.connections_detail_scroll.saturating_sub(1);
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::Down {
                        state.connections_detail_scroll =
                            state.connections_detail_scroll.saturating_add(1);
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::PageUp {
                        state.connections_detail_scroll =
                            state.connections_detail_scroll.saturating_sub(10);
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::PageDown {
                        state.connections_detail_scroll =
                            state.connections_detail_scroll.saturating_add(10);
                        return InputOutcome::Handled;
                    }
                }
            }
        }

        if state.current_page == Page::Decoded {
            let rows = state.decoded_rows();
            state.sync_decoded_selection(&rows);
            match state.decoded_panel_focus {
                DecodedPanelFocus::Table => {
                    let len = rows.len() as i32;
                    if key == KeyCode::Up && len > 0 {
                        let next = if state.decoded_selected_index <= 0 {
                            len - 1
                        } else {
                            state.decoded_selected_index - 1
                        };
                        let next_idx = next as usize;
                        state.set_selected_decoded(next_idx, &rows);
                        update_scroll_for_selection(
                            &mut state.decoded_table_scroll,
                            next_idx,
                            state.decoded_table_view_height,
                        );
                        state.decoded_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::Down && len > 0 {
                        let next = if state.decoded_selected_index >= len - 1 {
                            0
                        } else {
                            state.decoded_selected_index + 1
                        };
                        let next_idx = next as usize;
                        state.set_selected_decoded(next_idx, &rows);
                        update_scroll_for_selection(
                            &mut state.decoded_table_scroll,
                            next_idx,
                            state.decoded_table_view_height,
                        );
                        state.decoded_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::PageUp && len > 0 {
                        let step = page_step(state.decoded_table_view_height);
                        let next_idx =
                            page_jump_index(state.decoded_selected_index, len, step, false);
                        state.set_selected_decoded(next_idx, &rows);
                        update_scroll_for_selection(
                            &mut state.decoded_table_scroll,
                            next_idx,
                            state.decoded_table_view_height,
                        );
                        state.decoded_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::PageDown && len > 0 {
                        let step = page_step(state.decoded_table_view_height);
                        let next_idx =
                            page_jump_index(state.decoded_selected_index, len, step, true);
                        state.set_selected_decoded(next_idx, &rows);
                        update_scroll_for_selection(
                            &mut state.decoded_table_scroll,
                            next_idx,
                            state.decoded_table_view_height,
                        );
                        state.decoded_detail_scroll = 0;
                        return InputOutcome::Handled;
                    }
                }
                DecodedPanelFocus::Detail => {
                    if key == KeyCode::Up {
                        state.decoded_detail_scroll = state.decoded_detail_scroll.saturating_sub(1);
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::Down {
                        state.decoded_detail_scroll = state.decoded_detail_scroll.saturating_add(1);
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::PageUp {
                        state.decoded_detail_scroll =
                            state.decoded_detail_scroll.saturating_sub(10);
                        return InputOutcome::Handled;
                    }
                    if key == KeyCode::PageDown {
                        state.decoded_detail_scroll =
                            state.decoded_detail_scroll.saturating_add(10);
                        return InputOutcome::Handled;
                    }
                }
            }
        }

        if state.current_page == Page::Fingerprints {
            let total = match state.fingerprints_view {
                FingerprintsView::List => state.fingerprint_rows().len(),
                FingerprintsView::Clusters => state.fingerprint_clusters().0.len(),
                FingerprintsView::Events => state.if_stats_local.fingerprint_events.len(),
            } as u16;
            let view_height = state.fingerprints_view_height;
            let max_start = total.saturating_sub(view_height.max(1));
            if key == KeyCode::Up {
                state.fingerprints_scroll = state.fingerprints_scroll.saturating_sub(1);
                return InputOutcome::Handled;
            }
            if key == KeyCode::Down {
                state.fingerprints_scroll =
                    state.fingerprints_scroll.saturating_add(1).min(max_start);
                return InputOutcome::Handled;
            }
            if key == KeyCode::PageUp {
                let step = page_step(view_height) as u16;
                state.fingerprints_scroll = state.fingerprints_scroll.saturating_sub(step);
                return InputOutcome::Handled;
            }
            if key == KeyCode::PageDown {
                let step = page_step(view_height) as u16;
                state.fingerprints_scroll =
                    state.fingerprints_scroll.saturating_add(step).min(max_start);
                return InputOutcome::Handled;
            }
        }
    }

    InputOutcome::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scry::common::fingerprint::types::{FingerprintKey, FingerprintStatus};
    use crate::scry::common::stats::{PVEvent, TrackedConnectionEntry};
    use std::time::{Duration, UNIX_EPOCH};

    fn mk_entry(id: &str, age: u64) -> TrackedConnectionEntry {
        TrackedConnectionEntry {
            id: id.to_string(),
            endpoint_a: "10.0.0.1:1111".to_string(),
            endpoint_b: "10.0.0.2:2222".to_string(),
            cid: None,
            sid: None,
            pv_name: None,
            channel_count: 1,
            operation_count: 0,
            updates_per_sec: 1.0,
            last_seen_secs: age,
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
    fn key_five_opens_the_fingerprints_page() {
        let mut state = AppState::new();
        assert_eq!(handle_key_event(&mut state, KeyCode::Char('5')), InputOutcome::Handled);
        assert_eq!(state.current_page, Page::Fingerprints);
        assert_eq!(state.focus_area, FocusArea::Main);
    }

    /// Finding 1 (fix round 1): before this fix, `fingerprints_scroll` was
    /// never mutated by input handling at all — with a discontinuity log
    /// full of dropped events, the operator could see only the newest
    /// screenful and had no way to reach the rest. Up/Down/PageUp/PageDown
    /// must move the scroll offset, and it must clamp against
    /// `fingerprints_view_height` rather than scrolling past the end.
    #[test]
    fn fingerprints_scroll_moves_and_clamps_to_view_height() {
        let mut state = AppState::new();
        state.current_page = Page::Fingerprints;
        state.focus_area = FocusArea::Main;
        state.fingerprints_view_height = 2;
        state.fingerprints_show_pending = true;
        state.if_stats_local.fingerprint_statuses = (0..4)
            .map(|i| FingerprintStatus::Insufficient {
                key: FingerprintKey::new(
                    format!("PV:{}", i),
                    "10.0.0.11:5075".parse().unwrap(),
                ),
                samples: 1,
                min_samples: 30,
                window_start: UNIX_EPOCH,
            })
            .collect();

        assert_eq!(handle_key_event(&mut state, KeyCode::Down), InputOutcome::Handled);
        assert_eq!(state.fingerprints_scroll, 1);

        // total=4, view_height=2 => max_start = 2: repeated Down must clamp,
        // never scroll past the point where the last row is still visible.
        for _ in 0..10 {
            handle_key_event(&mut state, KeyCode::Down);
        }
        assert_eq!(state.fingerprints_scroll, 2);

        assert_eq!(handle_key_event(&mut state, KeyCode::Up), InputOutcome::Handled);
        assert_eq!(state.fingerprints_scroll, 1);

        assert_eq!(handle_key_event(&mut state, KeyCode::PageDown), InputOutcome::Handled);
        assert_eq!(state.fingerprints_scroll, 2);

        assert_eq!(handle_key_event(&mut state, KeyCode::PageUp), InputOutcome::Handled);
        assert!(state.fingerprints_scroll < 2);
    }

    #[test]
    fn tab_cycles_connections_panels_without_stale() {
        let mut state = AppState::new();
        state.current_page = Page::Connections;
        state.focus_area = FocusArea::Main;
        state.connections_show_stale_panel = false;
        state.connections_panel_focus = ConnectionsPanelFocus::LiveList;

        handle_key_event(&mut state, KeyCode::Tab);
        assert_eq!(state.connections_panel_focus, ConnectionsPanelFocus::Detail);

        handle_key_event(&mut state, KeyCode::Tab);
        assert_eq!(state.focus_area, FocusArea::Options);

        handle_key_event(&mut state, KeyCode::Tab);
        assert_eq!(state.focus_area, FocusArea::Main);
        assert_eq!(
            state.connections_panel_focus,
            ConnectionsPanelFocus::LiveList
        );
    }

    #[test]
    fn arrow_keys_cycle_connections_panels() {
        let mut state = AppState::new();
        state.current_page = Page::Connections;
        state.focus_area = FocusArea::Main;
        state.connections_show_stale_panel = false;
        state.connections_panel_focus = ConnectionsPanelFocus::LiveList;

        handle_key_event(&mut state, KeyCode::Right);
        assert_eq!(state.connections_panel_focus, ConnectionsPanelFocus::Detail);

        handle_key_event(&mut state, KeyCode::Left);
        assert_eq!(
            state.connections_panel_focus,
            ConnectionsPanelFocus::LiveList
        );

        handle_key_event(&mut state, KeyCode::Left);
        assert_eq!(state.focus_area, FocusArea::Options);
    }

    #[test]
    fn tab_cycles_connections_panels_with_stale() {
        let mut state = AppState::new();
        state.current_page = Page::Connections;
        state.focus_area = FocusArea::Main;
        state.connections_show_stale_panel = true;
        state.connections_panel_focus = ConnectionsPanelFocus::LiveList;

        handle_key_event(&mut state, KeyCode::Tab);
        assert_eq!(state.connections_panel_focus, ConnectionsPanelFocus::Detail);

        handle_key_event(&mut state, KeyCode::Tab);
        assert_eq!(
            state.connections_panel_focus,
            ConnectionsPanelFocus::StaleList
        );

        handle_key_event(&mut state, KeyCode::Tab);
        assert_eq!(state.focus_area, FocusArea::Options);
    }

    #[test]
    fn hotkeys_update_connections_controls() {
        let mut state = AppState::new();
        state.current_page = Page::Connections;

        handle_key_event(&mut state, KeyCode::Char('s'));
        assert_eq!(state.connections_sort_mode.label(), "Rate");

        handle_key_event(&mut state, KeyCode::Char('l'));
        assert_eq!(state.connections_live_window_secs, 60);

        handle_key_event(&mut state, KeyCode::Char('t'));
        assert!(state.connections_show_stale_panel);

        handle_key_event(&mut state, KeyCode::Char('/'));
        assert!(state.connections_search_edit);
    }

    #[test]
    fn live_list_navigation_updates_selected_id() {
        let mut state = AppState::new();
        state.current_page = Page::Connections;
        state.focus_area = FocusArea::Main;
        state.connections_panel_focus = ConnectionsPanelFocus::LiveList;
        state.if_stats_local.tracked_connections_summary = vec![mk_entry("a", 1), mk_entry("b", 2)];

        let p0 = state.connections_partitions();
        state.sync_connections_selection(&p0.live, &p0.stale);

        handle_key_event(&mut state, KeyCode::Down);
        assert_eq!(state.connections_selected_id.as_deref(), Some("b"));
    }

    #[test]
    fn connections_page_keys_jump_live_selection() {
        let mut state = AppState::new();
        state.current_page = Page::Connections;
        state.focus_area = FocusArea::Main;
        state.connections_panel_focus = ConnectionsPanelFocus::LiveList;
        state.connections_live_list_view_height = 5;
        state.if_stats_local.tracked_connections_summary =
            (0..20).map(|i| mk_entry(&format!("id-{i}"), 1)).collect();

        let p = state.connections_partitions();
        state.sync_connections_selection(&p.live, &p.stale);
        assert_eq!(state.connections_live_selected_index, 0);

        handle_key_event(&mut state, KeyCode::PageDown);
        assert_eq!(state.connections_live_selected_index, 4);

        handle_key_event(&mut state, KeyCode::PageDown);
        assert_eq!(state.connections_live_selected_index, 8);

        handle_key_event(&mut state, KeyCode::PageUp);
        assert_eq!(state.connections_live_selected_index, 4);
    }

    #[test]
    fn decoded_tab_cycles_table_detail_options() {
        let mut state = AppState::new();
        state.current_page = Page::Decoded;
        state.focus_area = FocusArea::Main;
        state.decoded_panel_focus = DecodedPanelFocus::Table;

        handle_key_event(&mut state, KeyCode::Tab);
        assert_eq!(state.decoded_panel_focus, DecodedPanelFocus::Detail);

        handle_key_event(&mut state, KeyCode::Tab);
        assert_eq!(state.focus_area, FocusArea::Options);

        handle_key_event(&mut state, KeyCode::Tab);
        assert_eq!(state.focus_area, FocusArea::Main);
        assert_eq!(state.decoded_panel_focus, DecodedPanelFocus::Table);
    }

    #[test]
    fn arrow_keys_cycle_decoded_panels() {
        let mut state = AppState::new();
        state.current_page = Page::Decoded;
        state.focus_area = FocusArea::Main;
        state.decoded_panel_focus = DecodedPanelFocus::Table;

        handle_key_event(&mut state, KeyCode::Right);
        assert_eq!(state.decoded_panel_focus, DecodedPanelFocus::Detail);

        handle_key_event(&mut state, KeyCode::Left);
        assert_eq!(state.decoded_panel_focus, DecodedPanelFocus::Table);

        handle_key_event(&mut state, KeyCode::Left);
        assert_eq!(state.focus_area, FocusArea::Options);
    }

    #[test]
    fn decoded_quick_filter_shortcuts_enter_edit_mode() {
        let mut state = AppState::new();
        state.current_page = Page::Decoded;

        handle_key_event(&mut state, KeyCode::Char('/'));
        assert!(state.decoded_edit_mode);
        assert_eq!(state.decoded_filter_field, FilterField::Text);
        state.decoded_edit_mode = false;

        handle_key_event(&mut state, KeyCode::Char('r'));
        assert!(state.decoded_edit_mode);
        assert_eq!(state.decoded_filter_field, FilterField::RequestType);
        state.decoded_edit_mode = false;

        handle_key_event(&mut state, KeyCode::Char('v'));
        assert!(state.decoded_edit_mode);
        assert_eq!(state.decoded_filter_field, FilterField::PvName);
        state.decoded_edit_mode = false;

        handle_key_event(&mut state, KeyCode::Char('i'));
        assert!(state.decoded_edit_mode);
        assert_eq!(state.decoded_filter_field, FilterField::SourceIp);
        state.decoded_edit_mode = false;

        handle_key_event(&mut state, KeyCode::Char('o'));
        assert!(state.decoded_edit_mode);
        assert_eq!(state.decoded_filter_field, FilterField::DestinationIp);
    }

    #[test]
    fn decoded_table_navigation_updates_selected_id() {
        let mut state = AppState::new();
        state.current_page = Page::Decoded;
        state.focus_area = FocusArea::Main;
        state.decoded_panel_focus = DecodedPanelFocus::Table;
        state.if_stats_local.events = vec![
            mk_event(100, "GET", "10.0.0.1", "10.0.0.2", "alpha"),
            mk_event(101, "PUT", "10.0.0.3", "10.0.0.4", "beta"),
        ];

        let rows = state.decoded_rows();
        state.sync_decoded_selection(&rows);
        let first_id = rows[0].id.clone();
        let second_id = rows[1].id.clone();
        assert_eq!(
            state.decoded_selected_id.as_deref(),
            Some(first_id.as_str())
        );

        handle_key_event(&mut state, KeyCode::Down);
        assert_eq!(
            state.decoded_selected_id.as_deref(),
            Some(second_id.as_str())
        );
    }

    #[test]
    fn decoded_page_keys_jump_table_selection() {
        let mut state = AppState::new();
        state.current_page = Page::Decoded;
        state.focus_area = FocusArea::Main;
        state.decoded_panel_focus = DecodedPanelFocus::Table;
        state.decoded_table_view_height = 6;
        state.if_stats_local.events = (0..20)
            .map(|i| mk_event(100 + i, "GET", "10.0.0.1", "10.0.0.2", &format!("msg-{i}")))
            .collect();

        let rows = state.decoded_rows();
        state.sync_decoded_selection(&rows);
        let expected_0 = rows[0].id.clone();
        let expected_5 = rows[5].id.clone();
        let expected_10 = rows[10].id.clone();
        assert_eq!(
            state.decoded_selected_id.as_deref(),
            Some(expected_0.as_str())
        );

        handle_key_event(&mut state, KeyCode::PageDown);
        assert_eq!(
            state.decoded_selected_id.as_deref(),
            Some(expected_5.as_str())
        );

        handle_key_event(&mut state, KeyCode::PageDown);
        assert_eq!(
            state.decoded_selected_id.as_deref(),
            Some(expected_10.as_str())
        );

        handle_key_event(&mut state, KeyCode::PageUp);
        assert_eq!(
            state.decoded_selected_id.as_deref(),
            Some(expected_5.as_str())
        );
    }

    #[test]
    fn decoded_detail_focus_scrolls_detail_pane() {
        let mut state = AppState::new();
        state.current_page = Page::Decoded;
        state.focus_area = FocusArea::Main;
        state.decoded_panel_focus = DecodedPanelFocus::Detail;
        state.if_stats_local.events = vec![mk_event(100, "GET", "10.0.0.1", "10.0.0.2", "alpha")];

        handle_key_event(&mut state, KeyCode::Down);
        assert_eq!(state.decoded_detail_scroll, 1);
    }
}
