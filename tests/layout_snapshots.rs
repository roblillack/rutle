//! Layout snapshot tests — proportional (NotoSans) backend.
//!
//! Ported from Piki's `renderer_snapshots.rs`. The scenarios
//! live in [`common`] and are shared with the monospace suite
//! (`layout_snapshots_mono.rs`); this file just binds each to insta under the
//! proportional font mode. Review changes with `cargo insta review`; the
//! `.snap.svg` files under `tests/snapshots/` open directly in a browser.

mod common;

use common::FontMode;

const MODE: FontMode = FontMode::Proportional;

#[test]
fn richtext_basic_render() {
    insta::assert_binary_snapshot!(".svg", common::basic_render(MODE));
}

#[test]
fn complex_rendering() {
    insta::assert_binary_snapshot!(".svg", common::complex_rendering(MODE));
}

#[test]
fn selection_single_block() {
    insta::assert_binary_snapshot!(".svg", common::selection_single_block(MODE));
}

#[test]
fn cursor_positioning_middle_of_line() {
    insta::assert_binary_snapshot!(".svg", common::cursor_positioning_middle_of_line(MODE));
}

#[test]
fn caret_affinity_left() {
    insta::assert_binary_snapshot!(".svg", common::caret_affinity_left(MODE));
}

#[test]
fn caret_affinity_right() {
    insta::assert_binary_snapshot!(".svg", common::caret_affinity_right(MODE));
}

#[test]
fn table_render() {
    insta::assert_binary_snapshot!(".svg", common::table_render(MODE));
}

#[test]
fn table_force_wrap_long_tokens() {
    insta::assert_binary_snapshot!(".svg", common::table_force_wrap_long_tokens(MODE));
}

#[test]
fn selection_across_blocks() {
    insta::assert_binary_snapshot!(".svg", common::selection_across_blocks(MODE));
}

#[test]
fn list_then_paragraph() {
    insta::assert_binary_snapshot!(".svg", common::list_then_paragraph(MODE));
}

#[test]
fn list_with_continuation_paragraph() {
    insta::assert_binary_snapshot!(".svg", common::list_with_continuation_paragraph(MODE));
}
