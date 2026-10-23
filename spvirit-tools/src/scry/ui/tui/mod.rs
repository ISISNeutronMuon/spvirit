// based on https://ratatui.rs/examples/widgets/sparkline/

use crate::scry::common::interfaces::InterfaceSelection;
use crate::scry::common::stats::IfStats;
use color_eyre::Result;
use ratatui::{
    crossterm::event::{self, Event, KeyEventKind},
    DefaultTerminal,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::debug;

mod input;
mod options;
mod render;
mod state;
mod utils;

use input::InputOutcome;
use state::AppState;

pub fn main_funcs(
    interface_stats: &mut Arc<Mutex<IfStats>>,
    tick_rate_ms: u64,
    interface_selection: Option<Arc<InterfaceSelection>>,
) -> Result<()> {
    debug!("Starting TUI");
    color_eyre::install()?;
    let terminal = ratatui::init();
    let app_result = App::new(interface_selection).run(terminal, interface_stats, tick_rate_ms);
    ratatui::restore();
    app_result
}

struct App {
    state: AppState,
}

impl App {
    fn new(interface_selection: Option<Arc<InterfaceSelection>>) -> Self {
        let mut state = AppState::new();
        if let Some(selection) = interface_selection {
            state.set_interface_selection(selection);
        }
        Self {
            state,
        }
    }

    fn on_tick(&mut self, interface_stats: &mut Arc<Mutex<IfStats>>) {
        if self.state.sync_interface_change() {
            self.state.reset_charts();
            self.state.reset_selections();
        }
        let if_stats = interface_stats.lock().unwrap();
        self.state.if_stats_local = if_stats.clone();

        let rate: f64 = if_stats.data_rate;
        let packet_rate: f64 = if_stats.packet_rate;
        let timestamp: f64 = if_stats.timestamp.elapsed().as_secs() as f64;

        self.state.data1.pop();
        self.state.data1.insert(0, rate);
        self.state.data2.pop();
        self.state.data2.insert(0, packet_rate);

        self.state.timestamps.pop();
        self.state.timestamps.insert(0, timestamp);

        let str_rate = utils::format_rate(rate);
        let log_str = format!(
            "Data Rate: {} {}",
            str_rate,
            utils::format_packet_rate(packet_rate)
        );
        self.state.log_hist.pop();
        self.state.log_hist.insert(0, log_str);
    }

    fn run(
        mut self,
        mut terminal: DefaultTerminal,
        interface_stats: &mut Arc<Mutex<IfStats>>,
        tick_rate_ms: u64,
    ) -> Result<()> {
        let tick_rate = Duration::from_millis(tick_rate_ms);
        let mut last_tick = Instant::now();
        // Switching pages swaps the legend for one that shares a long common
        // prefix, so the diff renderer repaints almost none of that row. If
        // the screen has drifted out of sync with ratatui's buffer for any
        // reason, a plain redraw cannot repair it - force a full clear.
        let mut last_page = self.state.current_page;
        loop {
            if self.state.current_page != last_page {
                terminal.clear()?;
                last_page = self.state.current_page;
            }
            terminal.draw(|frame| render::draw(frame, &mut self.state))?;

            let timeout = tick_rate.saturating_sub(last_tick.elapsed());
            if event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match input::handle_key_event(&mut self.state, key.code) {
                        InputOutcome::Quit => return Ok(()),
                        InputOutcome::Handled => {}
                        InputOutcome::Continue => {}
                    }
                }
            }
            if self.state.pause {
                self.state.ui_last_tick_lag_ms = 0;
                self.state.ui_is_lagging = false;
                last_tick = Instant::now();
            } else if last_tick.elapsed() >= tick_rate {
                let elapsed = last_tick.elapsed();
                let lag = elapsed.saturating_sub(tick_rate);
                self.state.ui_last_tick_lag_ms = lag.as_millis() as u64;
                self.state.ui_is_lagging = lag > tick_rate;
                self.state.ui_tick_rate_ms = tick_rate.as_millis() as u64;
                self.on_tick(interface_stats);
                last_tick = Instant::now();
            }
        }
    }
}
