//! Behavior tests for the shared editor's mutation operations.
//!
//! Ported in spirit from Pure's editor test coverage: each test drives a public
//! `Editor` operation and asserts the result via either the
//! marker-agnostic `current_block_type()` query or a markdown round-trip. This
//! hardens the shared core that both Piki (FLTK) and Pure (Ratatui) depend on.

use rutle::{BlockType, DocumentPosition, Editor};

// rutle is tdoc::Document-centric; build/serialize Markdown via `tdoc` directly.
fn markdown_to_document(md: &str) -> tdoc::Document {
    tdoc::markdown::parse(std::io::Cursor::new(md.as_bytes()))
        .unwrap_or_else(|_| tdoc::Document::new())
}

fn document_to_markdown(doc: &tdoc::Document) -> String {
    let mut buf: Vec<u8> = Vec::new();
    tdoc::markdown::write(&mut buf, doc).expect("serialize markdown");
    String::from_utf8(buf).unwrap_or_default()
}

fn editor_with(markdown: &str) -> Editor {
    let mut e = Editor::default();
    e.set_document(markdown_to_document(markdown));
    e
}

fn md(e: &Editor) -> String {
    document_to_markdown(e.document())
}

// ----- text insertion / deletion -------------------------------------------------

#[test]
fn insert_text_inserts_at_cursor() {
    let mut e = editor_with("ab\n");
    e.set_cursor(DocumentPosition::new(0, 1));
    e.insert_text("X").unwrap();
    assert!(md(&e).contains("aXb"), "{}", md(&e));
}

#[test]
fn insert_newline_splits_paragraph() {
    let mut e = editor_with("ab\n");
    e.set_cursor(DocumentPosition::new(0, 1));
    e.insert_newline().unwrap();
    assert_eq!(e.document().paragraphs.len(), 2, "{}", md(&e));
}

#[test]
fn delete_backward_at_start_merges_paragraphs() {
    let mut e = editor_with("a\n\nb\n");
    assert_eq!(e.document().paragraphs.len(), 2);
    // Move to the start of the second paragraph and backspace.
    e.move_cursor_down();
    e.delete_backward().unwrap();
    assert_eq!(e.document().paragraphs.len(), 1, "{}", md(&e));
    assert!(md(&e).contains("ab"), "{}", md(&e));
}

// ----- block type changes --------------------------------------------------------

#[test]
fn toggle_heading_makes_heading() {
    let mut e = editor_with("text\n");
    e.toggle_heading().unwrap();
    assert!(
        matches!(e.current_block_type(), BlockType::Heading { level: 1 }),
        "{:?}",
        e.current_block_type()
    );
}

#[test]
fn toggle_unordered_list() {
    let mut e = editor_with("item\n");
    e.toggle_list().unwrap();
    assert!(
        matches!(
            e.current_block_type(),
            BlockType::ListItem { ordered: false, .. }
        ),
        "{:?}",
        e.current_block_type()
    );
}

#[test]
fn toggle_ordered_list() {
    let mut e = editor_with("item\n");
    e.toggle_ordered_list().unwrap();
    assert!(
        matches!(
            e.current_block_type(),
            BlockType::ListItem { ordered: true, .. }
        ),
        "{:?}",
        e.current_block_type()
    );
}

#[test]
fn toggle_checklist_then_check() {
    let mut e = editor_with("task\n");
    e.toggle_checklist().unwrap();
    assert!(
        matches!(
            e.current_block_type(),
            BlockType::ListItem {
                checkbox: Some(false),
                ..
            }
        ),
        "{:?}",
        e.current_block_type()
    );
    e.toggle_current_checkmark().unwrap();
    assert!(
        matches!(
            e.current_block_type(),
            BlockType::ListItem {
                checkbox: Some(true),
                ..
            }
        ),
        "{:?}",
        e.current_block_type()
    );
}

#[test]
fn indent_then_outdent_round_trips() {
    let mut e = editor_with("- one\n- two\n");
    let before = md(&e);
    e.move_cursor_down(); // into the second item
    e.indent_list_item().unwrap();
    assert_ne!(md(&e), before, "indent should change structure");
    e.outdent_list_item().unwrap();
    assert_eq!(md(&e), before, "outdent should restore the original");
}

// ----- horizontal rules ----------------------------------------------------------

#[test]
fn insert_horizontal_rule_between_paragraphs() {
    let mut e = editor_with("above\n\nbelow\n");
    e.move_cursor_to_line_end(); // end of "above"
    e.insert_horizontal_rule().unwrap();
    assert_eq!(md(&e).trim_end(), "above\n\n---\n\nbelow", "{}", md(&e));
    // The caret continues below the rule.
    assert!(
        matches!(e.current_block_type(), BlockType::Paragraph),
        "{:?}",
        e.current_block_type()
    );
    e.insert_text("X").unwrap();
    assert!(md(&e).contains("Xbelow"), "{}", md(&e));
}

#[test]
fn cursor_reports_a_rule_as_its_own_block_type() {
    let mut e = editor_with("A\n\n---\n\nB\n");
    e.move_cursor_down();
    assert!(
        matches!(e.current_block_type(), BlockType::HorizontalRule),
        "{:?}",
        e.current_block_type()
    );
}

#[test]
fn backspace_removes_a_rule() {
    let mut e = editor_with("A\n\n---\n\nB\n");
    e.move_cursor_down(); // onto the rule
    e.delete_backward().unwrap();
    assert_eq!(md(&e).trim_end(), "A\n\nB", "{}", md(&e));
}

#[test]
fn rule_survives_a_markdown_round_trip() {
    let e = editor_with("# Title\n\nA\n\n---\n\nB\n");
    assert_eq!(md(&e).trim_end(), "# Title\n\nA\n\n---\n\nB", "{}", md(&e));
}

// ----- definition lists ----------------------------------------------------------

#[test]
fn cursor_reports_a_term_and_its_definition_separately() {
    let mut e = editor_with("Coffee\n: Black hot drink\n");
    assert!(
        matches!(
            e.current_block_type(),
            BlockType::DefinitionTerm { depth: 0 }
        ),
        "{:?}",
        e.current_block_type()
    );
    e.move_cursor_down(); // into the definition
    assert!(
        matches!(e.current_block_type(), BlockType::Paragraph),
        "{:?}",
        e.current_block_type()
    );
}

#[test]
fn typing_in_a_term_and_a_definition_reaches_the_document() {
    let mut e = editor_with("Coffee\n: Black hot drink\n");
    e.move_cursor_to_line_end();
    e.insert_text(" beans").unwrap();
    e.move_cursor_down();
    e.move_cursor_to_line_end();
    e.insert_text(", strong").unwrap();
    assert_eq!(
        md(&e).trim_end(),
        "Coffee beans\n: Black hot drink, strong",
        "{}",
        md(&e)
    );
}

#[test]
fn definition_list_survives_a_markdown_round_trip() {
    let e = editor_with("# Title\n\nCoffee\n: Black hot drink\n\nTea\n: A leaf infusion\n");
    assert_eq!(
        md(&e).trim_end(),
        "# Title\n\nCoffee\n: Black hot drink\n\nTea\n: A leaf infusion",
        "{}",
        md(&e)
    );
}

#[test]
fn copying_a_definition_list_keeps_its_text() {
    // Like lists and quotes, a definition list's *grouping* is not reconstructed by
    // the structure-preserving clipboard yet (see `get_selection_document`); its
    // terms and definitions come across as plain paragraphs. The text must survive.
    let mut e = editor_with("Coffee\n: Black hot drink\n");
    e.select_all();
    let doc = e.get_selection_document().expect("non-empty selection");
    let out = document_to_markdown(&doc);
    assert!(out.contains("Coffee"), "term text lost:\n{out}");
    assert!(
        out.contains("Black hot drink"),
        "definition text lost:\n{out}"
    );
}

// ----- inline styles & links -----------------------------------------------------

#[test]
fn toggle_bold_wraps_selection() {
    let mut e = editor_with("hello\n");
    e.select_all();
    e.toggle_bold().unwrap();
    assert!(md(&e).contains("**"), "{}", md(&e));
}

#[test]
fn link_replaces_selection() {
    let mut e = editor_with("click\n");
    e.select_all();
    e.replace_selection_with_link("https://example.com", "click")
        .unwrap();
    assert!(
        md(&e).contains("[click](https://example.com)"),
        "{}",
        md(&e)
    );
}

// ----- clipboard / selection -----------------------------------------------------

#[test]
fn get_selection_text_returns_selected_text() {
    let mut e = editor_with("hello\n");
    e.select_all();
    assert_eq!(e.get_selection_text(), "hello");
}

#[test]
fn selection_document_preserves_heading_level() {
    // Regression for the ported structure-preserving clipboard: copying a heading
    // must keep it a heading, not degrade it to body text.
    let mut e = editor_with("# Title\n\nbody\n");
    e.select_all();
    let doc = e.get_selection_document().expect("non-empty selection");
    let out = document_to_markdown(&doc);
    assert!(out.contains("# Title"), "heading not preserved:\n{out}");
}

// ----- undo / redo ---------------------------------------------------------------

#[test]
fn undo_redo_round_trips_an_edit() {
    let mut e = editor_with("hello\n");
    e.move_cursor_to_line_end();
    e.insert_text("X").unwrap();
    assert!(md(&e).contains("helloX"));

    assert!(e.undo());
    assert!(
        md(&e).contains("hello") && !md(&e).contains("helloX"),
        "{}",
        md(&e)
    );

    assert!(e.redo());
    assert!(md(&e).contains("helloX"), "{}", md(&e));
}
