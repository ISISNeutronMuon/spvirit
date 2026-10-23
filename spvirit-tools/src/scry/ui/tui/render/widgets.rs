use ratatui::{
    style::{Color, Style},
    widgets::{Block, Borders, ScrollbarState},
};

pub fn focus_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    }
}

pub fn panel(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(focus_border_style(focused))
}

pub fn scrollbar(total: usize, pos: u16) -> ScrollbarState {
    ScrollbarState::new(total).position(pos as usize)
}
