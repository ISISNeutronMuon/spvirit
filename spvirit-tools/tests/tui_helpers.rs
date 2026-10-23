#![cfg(feature = "tui")]

use ratatui::layout::Rect;
use spvirit_tools::tui::{centered_rect, focus_block};

#[test]
fn centered_rect_is_centered_and_proportional() {
    let full = Rect::new(0, 0, 100, 100);
    let centered = centered_rect(50, 40, full);
    assert_eq!(centered.width, 50);
    assert_eq!(centered.height, 40);
    assert_eq!(centered.x, 25, "equal margins left and right");
    assert_eq!(centered.y, 30, "equal margins top and bottom");
}

#[test]
fn centered_rect_survives_a_tiny_area() {
    let tiny = Rect::new(0, 0, 1, 1);
    let centered = centered_rect(50, 50, tiny);
    assert!(centered.width <= tiny.width);
    assert!(centered.height <= tiny.height);
}

#[test]
fn focus_block_marks_the_focused_pane_differently() {
    // `title` is an owned String — that is the existing bin signature.
    let focused = focus_block("Values".to_string(), true);
    let unfocused = focus_block("Values".to_string(), false);
    assert_ne!(
        format!("{focused:?}"),
        format!("{unfocused:?}"),
        "the focused and unfocused blocks must be visually distinct"
    );
}
