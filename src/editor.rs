// Editor — rutle's editing engine (layer 1).
//
// Editing operations over a `tdoc::Document` (the authoritative document tree). Cursor
// and selection are tree-path positions (`DocumentPosition { path, offset }`). Intra-leaf
// text editing reuses the `Block` inline helpers via a transient round-trip
// (spans -> InlineContent runs -> edit -> spans). Tree navigation goes through `tree_walk`.
//
// PHASE 1 SCOPE: storage, positions, cursor/selection, movement, intra-leaf insert/delete,
// undo/redo, load. Structural ops (block-spanning splits/merges, list/quote/style toggles,
// links, moves, paste of multi-paragraph content) are stubbed and land in Phase 2/3 — see
// the `// TODO(phase2)` markers.

use super::inline_convert::{inline_to_spans, spans_to_inline};
use super::reveal::{
    RevealStyle, clear_reveal_style, next_tag_boundary, prev_tag_boundary, reveal_tag_count_at,
    reveal_tag_to_remove, unwrap_links,
};
use super::structured_document::{
    Block, BlockType, InlineContent, TextRun, TextStyle, normalize_plain_text,
};
use super::tree_edit;
use super::tree_path::PathSegment;
use super::tree_path::{DocumentPosition, TreePath};
use super::tree_walk::{self, LeafInfo};
use std::time::{Duration, Instant};
use tdoc::Document;
use tdoc::inline::Span;
use tdoc::paragraph::{ChecklistItem, Paragraph};

/// Inline-style labels active at byte `offset` within a leaf's flat runs, outermost
/// first. At a run boundary `affinity` selects which side is read: `Left` inherits the
/// run ending there (so a cursor resting at the end of a bold word still reads as bold —
/// matching classic Pure), `Right` the run beginning there.
fn inline_labels_at(
    runs: &[InlineContent],
    offset: usize,
    affinity: Affinity,
) -> Vec<&'static str> {
    let mut pos = 0usize;
    for item in runs {
        let len = item.to_plain_text().len();
        // Left affinity reads the run ending at `offset`; Right affinity the run
        // beginning there. They differ only at a style boundary.
        let in_run = match affinity {
            Affinity::Left => offset > pos && offset <= pos + len,
            Affinity::Right => offset >= pos && offset < pos + len,
        };
        if in_run {
            return match item {
                InlineContent::Text(run) => style_labels(run.style),
                InlineContent::Link { content, .. } => {
                    let mut labels = vec!["Link"];
                    let inner = inline_labels_at(content, offset - pos, affinity);
                    // A link nested directly inside a link is degenerate; show a
                    // single "Link" rather than "Link > Link".
                    let inner = if inner.first() == Some(&"Link") {
                        &inner[1..]
                    } else {
                        &inner[..]
                    };
                    labels.extend_from_slice(inner);
                    labels
                }
                InlineContent::HardBreak => Vec::new(),
            };
        }
        pos += len;
    }
    Vec::new()
}

/// Style labels for a run, in classic Pure's outer-to-inner nesting order.
fn style_labels(style: TextStyle) -> Vec<&'static str> {
    let mut labels = Vec::new();
    if style.highlight {
        labels.push("Highlight");
    }
    if style.underline {
        labels.push("Underline");
    }
    if style.strikethrough {
        labels.push("Strikethrough");
    }
    if style.bold {
        labels.push("Bold");
    }
    if style.italic {
        labels.push("Italic");
    }
    if style.code {
        labels.push("Code");
    }
    labels
}

/// Which side of an inline-style boundary the caret associates with — its *affinity*.
///
/// At a style boundary (e.g. the seam between `Hello ` and a bold `World!`) a single
/// byte offset denotes two distinct caret positions: one belonging to the run that
/// *ends* there and one to the run that *begins* there. Affinity disambiguates them,
/// deciding which run newly typed text joins (and hence what style it inherits) and
/// which way the caret's direction indicator points. Away from a boundary it is always
/// `Left`, so the default preserves the historical left-biased behavior. This is the
/// mechanism that gives "two caret positions per style boundary" even when reveal codes
/// is off — see [`Editor::cursor_at_style_boundary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Affinity {
    /// Associate with the run *ending* at the offset (the character to the left).
    #[default]
    Left,
    /// Associate with the run *beginning* at the offset (the character to the right).
    Right,
}

/// Result of an editing operation
pub type EditResult = Result<(), EditError>;

/// Errors that can occur during editing
#[derive(Debug, Clone, PartialEq)]
pub enum EditError {
    InvalidPosition,
    InvalidBlockIndex,
    EmptyDocument,
    ConversionFailed(String),
}

/// Maximum number of undo steps retained on the undo stack.
const MAX_UNDO_STEPS: usize = 200;

/// Idle gap after which the next typing/deletion starts a fresh undo step.
const UNDO_COALESCE_IDLE: Duration = Duration::from_secs(2);

/// Classifies an edit so consecutive edits of the same kind can be coalesced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoKind {
    Typing,
    Deleting,
    Other,
}

/// An immutable snapshot of the editable state, used for undo/redo.
#[derive(Debug, Clone)]
struct EditorSnapshot {
    tdoc: Document,
    cursor: DocumentPosition,
    selection: Option<(DocumentPosition, DocumentPosition)>,
}

/// The structured editor with cursor state.
pub struct Editor {
    tdoc: Document,
    cursor: DocumentPosition,
    selection: Option<(DocumentPosition, DocumentPosition)>,
    paragraph_cb: Option<Box<dyn FnMut(BlockType) + 'static>>,
    undo_stack: Vec<EditorSnapshot>,
    redo_stack: Vec<EditorSnapshot>,
    undo_baseline: EditorSnapshot,
    last_edit_kind: Option<UndoKind>,
    last_edit_time: Option<Instant>,
    /// Whether reveal-codes mode is active. When on, inline-style boundaries are
    /// shown as `[Bold>` / `<Bold]` tags by the display, and backspace/delete next
    /// to such a tag removes the style instead of editing text. Off by default, so
    /// frontends that never enable it (the GUI) are wholly unaffected.
    reveal_codes: bool,
    /// While reveal codes is on, how many of the inline-style tags rendered at
    /// the cursor's byte offset the caret sits *past* (0 = before all of them).
    /// This lets Left/Right step onto a zero-width `[Bold>`/`<Bold]` tag — a stop
    /// the bare `DocumentPosition` can't express — without changing row:col.
    /// Always 0 unless reveal codes is on.
    cursor_reveal_stop: usize,
    /// Whether the caret pauses for an extra affinity stop at each inline-style
    /// boundary (the "two caret positions per boundary" behavior). On by default.
    /// When off, Left/Right step a plain grapheme, insertion is left-biased, and
    /// the caret draws no direction trail — i.e. the classic single-caret model.
    /// Independent of (and inert under) reveal codes, which has its own stepping.
    style_boundary_stops: bool,
    /// Which side of an inline-style boundary the caret associates with. Only
    /// meaningful when the cursor sits exactly on a style boundary (see
    /// [`Editor::cursor_at_style_boundary`]); elsewhere it stays `Left`. Drives
    /// both the style newly typed text inherits and the caret's on-screen
    /// direction indicator, providing the "two caret positions per boundary"
    /// behavior when reveal codes is *off*.
    cursor_affinity: Affinity,
    /// Whether the *backend* can render the affinity lean at all. A cell backend
    /// can't (its caret is one indivisible cell), so the renderer syncs this from
    /// [`crate::RenderContext::supports_caret_affinity`] on every layout pass. When
    /// `false`, affinity is inert no matter what `style_boundary_stops` says — the
    /// two stops collapse to one and the caret never leans. `true` by default,
    /// matching a pixel backend (and a bare editor with no renderer attached yet).
    affinity_supported: bool,
}

impl Editor {
    /// Create a new editor with an empty document.
    pub fn new() -> Self {
        Self::with_tdoc(Document::new())
    }

    /// Create an editor wrapping an existing tdoc document.
    pub fn with_tdoc(tdoc: Document) -> Self {
        let baseline = EditorSnapshot {
            tdoc: tdoc.clone(),
            cursor: DocumentPosition::start(),
            selection: None,
        };
        let mut editor = Editor {
            tdoc,
            cursor: DocumentPosition::start(),
            selection: None,
            paragraph_cb: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            undo_baseline: baseline,
            last_edit_kind: None,
            last_edit_time: None,
            reveal_codes: false,
            cursor_reveal_stop: 0,
            style_boundary_stops: true,
            cursor_affinity: Affinity::Left,
            affinity_supported: true,
        };
        editor.normalize_cursor();
        editor
    }

    /// The authoritative document tree.
    pub fn document(&self) -> &Document {
        &self.tdoc
    }

    /// Mutable access to the authoritative document tree. Callers that mutate it should
    /// follow up with [`Editor::after_external_change`].
    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.tdoc
    }

    /// Replace the whole document (e.g. when loading a page). Resets the caret and
    /// clears undo/redo history, making the new document the undo baseline.
    pub fn set_document(&mut self, document: Document) {
        self.tdoc = document;
        self.cursor = DocumentPosition::start();
        self.selection = None;
        self.normalize_cursor();
        self.trigger_paragraph_change();
        self.reset_undo_history();
    }

    /// Re-clamp the caret after the document was mutated through `document_mut`.
    pub fn after_external_change(&mut self) {
        self.normalize_cursor();
        self.trigger_paragraph_change();
    }

    pub fn set_paragraph_change_callback(
        &mut self,
        cb: Option<Box<dyn FnMut(BlockType) + 'static>>,
    ) {
        self.paragraph_cb = cb;
        self.trigger_paragraph_change();
    }

    fn trigger_paragraph_change(&mut self) {
        if self.paragraph_cb.is_some() {
            let block_type = self.block_type_at(&self.cursor.path);
            if let Some(cb) = self.paragraph_cb.as_mut() {
                cb(block_type);
            }
        }
    }

    // ----- Leaf helpers --------------------------------------------------------------

    fn leaves(&self) -> Vec<LeafInfo> {
        tree_walk::enumerate_leaves(&self.tdoc)
    }

    fn leaf_paths(&self) -> Vec<TreePath> {
        tree_walk::leaf_paths(&self.tdoc)
    }

    fn leaf_index(&self, path: &TreePath) -> Option<usize> {
        self.leaf_paths().iter().position(|p| p == path)
    }

    fn leaf_count(&self) -> usize {
        tree_walk::leaf_count(&self.tdoc)
    }

    fn leaf_text_len(&self, path: &TreePath) -> usize {
        tree_walk::leaf_text_len(&self.tdoc, path)
    }

    fn leaf_plain_text(&self, path: &TreePath) -> String {
        tree_walk::leaf_plain_text(&self.tdoc, path)
    }

    /// The presentation block type at the cursor (for menus / paragraph-style UI).
    pub fn current_block_type(&self) -> BlockType {
        self.block_type_at(&self.cursor.path)
    }

    /// Inline-style breadcrumb labels at the cursor — e.g. `["Bold"]`, `["Link"]`,
    /// or `["Link", "Bold"]` for a bold link — outermost first. Empty when the
    /// cursor sits on unstyled text. Used by a status-bar breadcrumb; mirrors the
    /// inline portion of classic Pure's cursor breadcrumb.
    pub fn cursor_inline_labels(&self) -> Vec<&'static str> {
        let runs = tree_walk::leaf_inline(&self.tdoc, &self.cursor.path);
        inline_labels_at(&runs, self.cursor.offset, self.cursor_affinity)
    }

    /// Whether reveal-codes mode is active (see the `reveal_codes` field).
    pub fn reveal_codes(&self) -> bool {
        self.reveal_codes
    }

    /// Enable or disable reveal-codes mode.
    pub fn set_reveal_codes(&mut self, enabled: bool) {
        self.reveal_codes = enabled;
        self.cursor_reveal_stop = 0;
    }

    /// Whether the caret pauses for an extra affinity stop at inline-style
    /// boundaries (see the `style_boundary_stops` field). On by default.
    pub fn style_boundary_stops(&self) -> bool {
        self.style_boundary_stops
    }

    /// Enable or disable the extra affinity stop at inline-style boundaries. When
    /// disabling, the caret's affinity is reset to the neutral `Left` so no stale
    /// right-bias lingers.
    pub fn set_style_boundary_stops(&mut self, enabled: bool) {
        self.style_boundary_stops = enabled;
        if !enabled {
            self.cursor_affinity = Affinity::Left;
        }
    }

    /// Tell the editor whether the backend can render the affinity lean. Driven by
    /// the renderer from [`crate::RenderContext::supports_caret_affinity`]; hosts
    /// don't normally call it directly. A cell backend passes `false`, which makes
    /// affinity inert (the two boundary stops collapse to one) irrespective of the
    /// user's `style_boundary_stops` preference. Idempotent; clearing support resets
    /// any lingering right-bias to the neutral `Left`, like disabling the toggle.
    pub fn set_affinity_supported(&mut self, supported: bool) {
        if self.affinity_supported == supported {
            return;
        }
        self.affinity_supported = supported;
        if !supported {
            self.cursor_affinity = Affinity::Left;
        }
    }

    /// Whether inline-style affinity is *actually* in effect right now: the user
    /// preference ([`Self::style_boundary_stops`]) is on **and** the backend can
    /// render the lean ([`Self::set_affinity_supported`]). This is the single gate
    /// the navigation, insertion, and caret-drawing paths consult.
    pub(crate) fn affinity_active(&self) -> bool {
        self.style_boundary_stops && self.affinity_supported
    }

    /// How many reveal tags at the cursor's offset the caret sits past (see the
    /// `cursor_reveal_stop` field). Consumed by the display when placing the caret.
    pub fn cursor_reveal_stop(&self) -> usize {
        self.cursor_reveal_stop
    }

    /// The caret's current affinity — which side of a style boundary it associates
    /// with. Consumed by the display to point the caret's direction indicator and by
    /// [`Self::insert_text`] to pick the style newly typed text inherits.
    pub fn cursor_affinity(&self) -> Affinity {
        self.cursor_affinity
    }

    /// Whether the caret currently sits exactly on an inline-style boundary — an offset
    /// where the run to its left and the run to its right carry different styles
    /// (including a leaf's leading/trailing style edges). Only at such a position does
    /// [`Self::cursor_affinity`] have an effect and does Left/Right stepping pause for
    /// the extra affinity stop.
    pub fn cursor_at_style_boundary(&self) -> bool {
        self.style_boundary_at(&self.cursor.path, self.cursor.offset)
    }

    /// Whether `offset` is an inline-style boundary within the leaf at `path`. Derived
    /// from the same reveal-tag model reveal codes uses, but without requiring reveal
    /// codes to be on — a style transition (or a leaf's leading/trailing styled edge)
    /// produces at least one tag there.
    fn style_boundary_at(&self, path: &TreePath, offset: usize) -> bool {
        let content = tree_walk::leaf_inline(&self.tdoc, path);
        reveal_tag_count_at(&content, offset) > 0
    }

    /// Number of reveal-tag cursor stops at `offset` in the given leaf (0 when
    /// reveal codes is off or `offset` isn't a style boundary).
    fn reveal_stops_at(&self, path: &TreePath, offset: usize) -> usize {
        if !self.reveal_codes {
            return 0;
        }
        let content = tree_walk::leaf_inline(&self.tdoc, path);
        reveal_tag_count_at(&content, offset)
    }

    /// Reveal-aware left step: walk back through the tags at the current offset
    /// before crossing to the previous character (landing past that character's
    /// trailing tags). Returns the new position and reveal-stop.
    fn reveal_position_left(&self) -> (DocumentPosition, usize) {
        if self.reveal_codes && self.cursor_reveal_stop > 0 {
            return (self.cursor.clone(), self.cursor_reveal_stop - 1);
        }
        let prev = self.position_left(&self.cursor);
        if self.reveal_codes && prev != self.cursor {
            let stops = self.reveal_stops_at(&prev.path, prev.offset);
            return (prev, stops);
        }
        (prev, 0)
    }

    /// Reveal-aware right step: walk forward through the tags at the current
    /// offset before crossing to the next character.
    fn reveal_position_right(&self) -> (DocumentPosition, usize) {
        if self.reveal_codes {
            let stops = self.reveal_stops_at(&self.cursor.path, self.cursor.offset);
            if self.cursor_reveal_stop < stops {
                return (self.cursor.clone(), self.cursor_reveal_stop + 1);
            }
        }
        (self.position_right(&self.cursor), 0)
    }

    /// Reveal-aware word-right: step through the tags at the current offset one at
    /// a time (each tag is its own word stop, like classic Pure), then jump to the
    /// next word — stopping at any intervening style boundary so its tags stay
    /// reachable. Returns the new position and reveal-stop.
    fn reveal_word_right(&self) -> (DocumentPosition, usize) {
        if !self.reveal_codes {
            return (self.word_right_position(&self.cursor), 0);
        }
        let stops = self.reveal_stops_at(&self.cursor.path, self.cursor.offset);
        if self.cursor_reveal_stop < stops {
            return (self.cursor.clone(), self.cursor_reveal_stop + 1);
        }
        let word = self.word_right_position(&self.cursor);
        let content = tree_walk::leaf_inline(&self.tdoc, &self.cursor.path);
        if let Some(b) = next_tag_boundary(&content, self.cursor.offset) {
            let crosses_leaf = word.path != self.cursor.path;
            if crosses_leaf || b < word.offset {
                return (DocumentPosition::at(self.cursor.path.clone(), b), 0);
            }
        }
        (word, 0)
    }

    /// Reveal-aware word-left: mirror of [`Self::reveal_word_right`]. Lands at a
    /// boundary's *last* reveal-stop (after its tags) so the next steps walk back
    /// through them.
    fn reveal_word_left(&self) -> (DocumentPosition, usize) {
        if !self.reveal_codes {
            return (self.word_left_position(&self.cursor), 0);
        }
        if self.cursor_reveal_stop > 0 {
            return (self.cursor.clone(), self.cursor_reveal_stop - 1);
        }
        let word = self.word_left_position(&self.cursor);
        let content = tree_walk::leaf_inline(&self.tdoc, &self.cursor.path);
        if let Some(b) = prev_tag_boundary(&content, self.cursor.offset) {
            let crosses_leaf = word.path != self.cursor.path;
            if crosses_leaf || b > word.offset {
                let stops = reveal_tag_count_at(&content, b);
                return (DocumentPosition::at(self.cursor.path.clone(), b), stops);
            }
        }
        let stops = self.reveal_stops_at(&word.path, word.offset);
        (word, stops)
    }

    /// Affinity-aware left step used when reveal codes is *off*. At a style boundary the
    /// caret pauses for two stops — `Right` (the run to its right) then `Left` (the run
    /// to its left) — before crossing to the previous grapheme. Mirror of
    /// [`Self::affinity_position_right`].
    fn affinity_position_left(&self) -> (DocumentPosition, Affinity) {
        // Standing on a boundary with Right affinity: flip to Left in place.
        if self.cursor_affinity == Affinity::Right
            && self.style_boundary_at(&self.cursor.path, self.cursor.offset)
        {
            return (self.cursor.clone(), Affinity::Left);
        }
        let prev = self.position_left(&self.cursor);
        // Crossing left onto a boundary lands on its right-hand (later) stop first.
        let affinity = if prev != self.cursor && self.style_boundary_at(&prev.path, prev.offset) {
            Affinity::Right
        } else {
            Affinity::Left
        };
        (prev, affinity)
    }

    /// Affinity-aware right step used when reveal codes is *off*. Mirror of
    /// [`Self::affinity_position_left`]: on a boundary, `Left` flips to `Right` in place
    /// before the next press crosses to the following grapheme.
    fn affinity_position_right(&self) -> (DocumentPosition, Affinity) {
        if self.cursor_affinity == Affinity::Left
            && self.style_boundary_at(&self.cursor.path, self.cursor.offset)
        {
            return (self.cursor.clone(), Affinity::Right);
        }
        // Crossing right always arrives on the earlier (Left) stop of the destination;
        // a boundary's Right stop is only ever reached by the in-place flip above.
        (self.position_right(&self.cursor), Affinity::Left)
    }

    /// When reveal codes is on, delete the inline-style tag the caret sits beside
    /// (`backward` = the `[Bold>`/`<Bold]` just left of the caret, mirrored for
    /// forward), removing that style — or, for a `[Link>`/`<Link]` tag, unwrapping
    /// the link — from its span without touching the text. Which tag is targeted
    /// depends on the caret's reveal-stop. Returns `true` when a tag was removed,
    /// so the caller skips the normal character deletion. The caret stays put.
    fn remove_reveal_tag(&mut self, backward: bool) -> bool {
        if !self.reveal_codes {
            return false;
        }
        let path = self.cursor.path.clone();
        if self.is_table_leaf(&path) {
            return false;
        }
        let runs = tree_walk::leaf_inline(&self.tdoc, &path);
        let Some((style, range_start, range_end)) =
            reveal_tag_to_remove(&runs, self.cursor.offset, self.cursor_reveal_stop, backward)
        else {
            return false;
        };
        if range_start >= range_end {
            return false;
        }
        let offset = self.cursor.offset;
        self.edit_leaf(&path, |content| {
            let (before, selected, after) =
                split_content_for_style(content, range_start, range_end);
            let cleared = if style == RevealStyle::Link {
                // A link isn't a text-style flag; remove it by unwrapping.
                unwrap_links(selected)
            } else {
                map_style_on_runs(selected, &mut |s: &mut TextStyle| {
                    clear_reveal_style(s, style)
                })
            };
            *content = before.into_iter().chain(cleared).chain(after).collect();
        });
        self.cursor = DocumentPosition::at(path, offset);
        self.normalize_cursor();
        true
    }

    /// The presentation block type at a path (`Paragraph` when the path is invalid).
    fn block_type_at(&self, path: &TreePath) -> BlockType {
        self.leaves()
            .iter()
            .find(|l| &l.path == path)
            .map(|info| tree_walk::leaf_block_type(&self.tdoc, info))
            .unwrap_or(BlockType::Paragraph)
    }

    fn is_table_leaf(&self, path: &TreePath) -> bool {
        matches!(self.block_type_at(path), BlockType::Table { .. })
    }

    /// Edit the inline runs of the leaf at `path` in place. The closure receives the
    /// leaf's content as flat runs; the result is written back as spans. No-op (and the
    /// closure still runs on a throwaway copy) for tables / invalid paths.
    fn edit_leaf<R>(&mut self, path: &TreePath, f: impl FnOnce(&mut Vec<InlineContent>) -> R) -> R {
        let mut content = tree_walk::leaf_inline(&self.tdoc, path);
        let result = f(&mut content);
        tree_walk::set_leaf_inline(&mut self.tdoc, path, &content);
        result
    }

    // ----- Undo / redo ---------------------------------------------------------------

    fn current_snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            tdoc: self.tdoc.clone(),
            cursor: self.cursor.clone(),
            selection: self.selection.clone(),
        }
    }

    pub fn commit_undo_step(&mut self, kind: UndoKind, now: Instant) {
        let within_idle_window = self
            .last_edit_time
            .is_some_and(|t| now.saturating_duration_since(t) < UNDO_COALESCE_IDLE);
        self.last_edit_time = Some(now);
        self.record_step(kind, within_idle_window);
    }

    fn record_step(&mut self, kind: UndoKind, within_idle_window: bool) {
        if self.undo_baseline.tdoc == self.tdoc {
            self.undo_baseline.cursor = self.cursor.clone();
            self.undo_baseline.selection = self.selection.clone();
            return;
        }

        let coalesce = kind != UndoKind::Other
            && self.last_edit_kind == Some(kind)
            && within_idle_window
            && !self.undo_stack.is_empty();

        if !coalesce {
            self.undo_stack.push(self.undo_baseline.clone());
            if self.undo_stack.len() > MAX_UNDO_STEPS {
                self.undo_stack.remove(0);
            }
        }

        self.redo_stack.clear();
        self.undo_baseline = self.current_snapshot();
        self.last_edit_kind = Some(kind);
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty() || self.undo_baseline.tdoc != self.tdoc
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        self.flush_pending_edit();
        let Some(previous) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack.push(self.undo_baseline.clone());
        self.undo_baseline = previous.clone();
        self.last_edit_kind = None;
        self.restore_snapshot(previous);
        true
    }

    pub fn redo(&mut self) -> bool {
        self.flush_pending_edit();
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack.push(self.undo_baseline.clone());
        self.undo_baseline = next.clone();
        self.last_edit_kind = None;
        self.restore_snapshot(next);
        true
    }

    pub fn reset_undo_history(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit_kind = None;
        self.last_edit_time = None;
        self.undo_baseline = self.current_snapshot();
    }

    fn break_undo_coalescing(&mut self) {
        self.last_edit_kind = None;
    }

    fn flush_pending_edit(&mut self) {
        self.record_step(UndoKind::Other, false);
    }

    fn restore_snapshot(&mut self, snapshot: EditorSnapshot) {
        self.tdoc = snapshot.tdoc;
        self.cursor = snapshot.cursor;
        self.selection = snapshot.selection;
        self.normalize_cursor();
        self.trigger_paragraph_change();
    }

    fn normalize_cursor(&mut self) {
        self.cursor = tree_walk::clamp_position(&self.tdoc, &self.cursor);
        // Any cursor move other than reveal-tag stepping lands "before" the tags;
        // the two horizontal movers restore the stepped value after calling this.
        self.cursor_reveal_stop = 0;
        // Likewise, most moves reset affinity to the historical left bias; the
        // horizontal movers restore the stepped affinity after normalizing.
        self.cursor_affinity = Affinity::Left;
    }

    // ----- Cursor & selection --------------------------------------------------------

    pub fn cursor(&self) -> DocumentPosition {
        self.cursor.clone()
    }

    pub fn set_cursor(&mut self, pos: DocumentPosition) {
        self.break_undo_coalescing();
        self.cursor = tree_walk::clamp_position(&self.tdoc, &pos);
        self.selection = None;
        self.cursor_affinity = Affinity::Left;
        self.trigger_paragraph_change();
    }

    pub fn selection(&self) -> Option<(DocumentPosition, DocumentPosition)> {
        self.selection.clone()
    }

    pub fn set_selection(&mut self, start: DocumentPosition, end: DocumentPosition) {
        self.break_undo_coalescing();
        let start = tree_walk::clamp_position(&self.tdoc, &start);
        let end = tree_walk::clamp_position_forward(&self.tdoc, &end);
        self.selection = Some((start, end));
    }

    pub fn clear_selection(&mut self) {
        self.break_undo_coalescing();
        self.selection = None;
    }

    pub fn select_all(&mut self) {
        self.break_undo_coalescing();
        let paths = self.leaf_paths();
        let (Some(first), Some(last)) = (paths.first().cloned(), paths.last().cloned()) else {
            self.selection = None;
            return;
        };
        let start = DocumentPosition::at(first, 0);
        let end = DocumentPosition::at(last.clone(), self.leaf_text_len(&last));
        self.selection = Some((start, end.clone()));
        self.cursor = end;
        self.normalize_cursor();
    }

    pub fn extend_selection_to(&mut self, end: DocumentPosition) {
        self.break_undo_coalescing();
        let end = tree_walk::clamp_position(&self.tdoc, &end);
        if let Some((start, _)) = self.selection.clone() {
            self.selection = Some((start, end.clone()));
        } else {
            self.selection = Some((self.cursor.clone(), end.clone()));
        }
        self.cursor = end;
        self.normalize_cursor();
    }

    pub fn select_word_at(&mut self, pos: DocumentPosition) {
        let pos = tree_walk::clamp_position(&self.tdoc, &pos);
        let text = self.leaf_plain_text(&pos.path);
        if text.is_empty() || pos.offset >= text.len() {
            return;
        }

        let mut start = pos.offset;
        let mut end = pos.offset;

        while start > 0 {
            let ch = text[..start].chars().next_back().unwrap();
            if ch.is_whitespace() || ch.is_ascii_punctuation() {
                break;
            }
            start = text[..start]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }

        for (_, ch) in text[end..].char_indices() {
            if ch.is_whitespace() || ch.is_ascii_punctuation() {
                break;
            }
            end = text[..end]
                .chars()
                .next()
                .map(|c| end + c.len_utf8())
                .unwrap_or(end);
        }

        if start == end {
            end = text[end..]
                .chars()
                .next()
                .map(|c| end + c.len_utf8())
                .unwrap_or(end);
        }

        let start_pos = DocumentPosition::at(pos.path.clone(), start);
        let end_pos = DocumentPosition::at(pos.path, end);
        self.set_selection(start_pos, end_pos.clone());
        self.cursor = tree_walk::clamp_position_forward(&self.tdoc, &end_pos);
    }

    pub fn select_line_at(&mut self, pos: DocumentPosition) {
        let pos = tree_walk::clamp_position(&self.tdoc, &pos);
        let len = self.leaf_text_len(&pos.path);
        let start_pos = DocumentPosition::at(pos.path.clone(), 0);
        let end_pos = DocumentPosition::at(pos.path, len);
        self.set_selection(start_pos, end_pos.clone());
        self.cursor = tree_walk::clamp_position_forward(&self.tdoc, &end_pos);
    }

    // ----- Insertion -----------------------------------------------------------------

    pub fn insert_text(&mut self, text: &str) -> EditResult {
        // Right affinity (only reachable at a style boundary, and irrelevant when a
        // selection is about to be replaced) makes the inserted text join the run
        // beginning at the offset instead of the one ending there.
        let bias_right = self.affinity_active()
            && self.selection.is_none()
            && self.cursor_affinity == Affinity::Right;
        self.cursor_reveal_stop = 0;
        self.cursor_affinity = Affinity::Left;
        if self.leaf_count() == 0 {
            self.tdoc
                .add_paragraph(Paragraph::new_text().with_content(inline_to_spans(&[
                    InlineContent::Text(TextRun::plain(text)),
                ])));
            self.cursor = DocumentPosition::at(TreePath::root(0), text.len());
            return Ok(());
        }

        // Replace any selection first (intra-leaf for now).
        if self.selection.is_some() {
            self.delete_selection()?;
        }

        let path = self.cursor.path.clone();
        // Tables are read-only.
        if self.is_table_leaf(&path) {
            return Ok(());
        }

        let offset = self.cursor.offset;
        self.edit_leaf(&path, |content| {
            insert_into_content(content, offset, text, bias_right)
        });
        self.cursor.offset = offset + text.len();
        Ok(())
    }

    pub fn insert_hard_break(&mut self) -> EditResult {
        if self.leaf_count() == 0 {
            self.tdoc.add_paragraph(
                Paragraph::new_text().with_content(inline_to_spans(&[InlineContent::HardBreak])),
            );
            self.cursor = DocumentPosition::at(TreePath::root(0), 1);
            return Ok(());
        }

        if self.selection.is_some() {
            self.delete_selection()?;
        }

        let path = self.cursor.path.clone();
        if self.is_table_leaf(&path) {
            return Ok(());
        }
        let offset = self.cursor.offset;
        self.edit_leaf(&path, |content| {
            let mut block = Block::paragraph();
            block.content = std::mem::take(content);
            let right = block.split_content_at(offset);
            block.content.push(InlineContent::HardBreak);
            block.content.extend(right);
            *content = block.content;
        });
        self.cursor.offset = offset + 1;
        Ok(())
    }

    /// Split the current leaf into a new sibling. TODO(phase2): full tree-aware split that
    /// continues lists / quotes and outdents empty items. Phase 1 handles only the
    /// top-level paragraph case; otherwise it is a no-op.
    pub fn insert_newline(&mut self) -> EditResult {
        if self.leaf_count() == 0 {
            self.tdoc.add_paragraph(Paragraph::new_text());
            self.tdoc.add_paragraph(Paragraph::new_text());
            self.cursor = DocumentPosition::at(TreePath::root(1), 0);
            return Ok(());
        }
        if self.selection.is_some() {
            self.delete_selection()?;
        }
        let path = self.cursor.path.clone();
        if self.is_table_leaf(&path) {
            return Ok(());
        }
        // Enter in an empty block promotes it up one structural level rather than inserting
        // another empty sibling, so repeated Enter walks it out one container per press:
        //   - an empty *continuation* paragraph in a multi-paragraph list item splits off
        //     into a new item of that list (its content is preserved, not dissolved);
        //   - a genuinely empty item, or an empty quote child, exits its container.
        if self.leaf_text_len(&path) == 0 {
            if let Some(new_path) = tree_edit::split_list_entry(&mut self.tdoc, &path) {
                self.cursor = DocumentPosition::at(new_path, 0);
                self.normalize_cursor();
                self.trigger_paragraph_change();
                return Ok(());
            }
            if matches!(
                path.last(),
                Some(
                    PathSegment::ListEntry { .. }
                        | PathSegment::ChecklistItem(_)
                        | PathSegment::QuoteChild(_)
                )
            ) {
                // Enter on an empty item drops list-ness (an empty item in a quote becomes a
                // plain quote paragraph, not a lifted-out list), so delist rather than unnest.
                return self.outdent_list_item_delisting();
            }
        }
        let offset = self.cursor.offset;
        if let Some(new_path) = tree_edit::split_leaf(&mut self.tdoc, &path, offset) {
            self.cursor = DocumentPosition::at(new_path, 0);
        }
        self.normalize_cursor();
        Ok(())
    }

    /// Insert a continuation paragraph. Like [`Self::insert_newline`], but inside a list
    /// item it adds another paragraph to the *same* item rather than starting a new item —
    /// so a list entry (or, equivalently, a quote) can hold multiple paragraphs.
    pub fn insert_continuation(&mut self) -> EditResult {
        if self.leaf_count() == 0 {
            return self.insert_newline();
        }
        if self.selection.is_some() {
            self.delete_selection()?;
        }
        let path = self.cursor.path.clone();
        if self.is_table_leaf(&path) {
            return Ok(());
        }
        // On an empty item there is nothing to continue; fall back to newline (which
        // outdents the empty item, matching Enter).
        if self.cursor_is_list_item() && self.leaf_text_len(&path) == 0 {
            return self.insert_newline();
        }
        let offset = self.cursor.offset;
        if let Some(new_path) = tree_edit::split_leaf_continuation(&mut self.tdoc, &path, offset) {
            self.cursor = DocumentPosition::at(new_path, 0);
        }
        self.normalize_cursor();
        Ok(())
    }

    // ----- Deletion ------------------------------------------------------------------

    pub fn delete_backward(&mut self) -> EditResult {
        if self.leaf_count() == 0 {
            return Err(EditError::EmptyDocument);
        }
        if self.selection.is_some() {
            return self.delete_selection();
        }

        // Reveal codes: backspacing into an inline-style tag removes the style.
        if self.remove_reveal_tag(true) {
            return Ok(());
        }

        let path = self.cursor.path.clone();
        let offset = self.cursor.offset;

        if self.is_table_leaf(&path) {
            return Ok(());
        }
        if offset == 0 {
            // Backspace at the start of a nested list item outdents it instead of merging.
            if self.cursor_is_list_item() && self.cursor_list_depth() > 0 {
                return self.outdent_list_item();
            }
            if let Some((pos_path, pos_off)) = tree_edit::merge_with_previous(&mut self.tdoc, &path)
            {
                self.cursor = DocumentPosition::at(pos_path, pos_off);
                self.normalize_cursor();
            }
            return Ok(());
        }

        let prev = tree_walk::previous_grapheme_position(
            &self.tdoc,
            &DocumentPosition::at(path.clone(), offset),
        )
        .offset;
        if prev < offset {
            self.delete_intra_leaf_range(&path, prev, offset);
            self.cursor.offset = prev;
        }
        self.normalize_cursor();
        Ok(())
    }

    pub fn delete_backward_bytes(&mut self, byte_count: usize) -> Result<bool, EditError> {
        if byte_count == 0 {
            return Ok(false);
        }
        if self.leaf_count() == 0 {
            return Err(EditError::EmptyDocument);
        }
        self.normalize_cursor();

        let path = self.cursor.path.clone();
        let end = self.cursor.offset;
        let text = self.leaf_plain_text(&path);
        // Walk back grapheme by grapheme within this leaf until we've covered byte_count.
        let mut start = end;
        let mut remaining = byte_count;
        while start > 0 && remaining > 0 {
            let prev = tree_walk::previous_grapheme_position(
                &self.tdoc,
                &DocumentPosition::at(path.clone(), start),
            )
            .offset;
            if prev >= start {
                break;
            }
            let removed = start - prev;
            start = prev;
            remaining = remaining.saturating_sub(removed);
        }
        let _ = text;
        if start == end {
            return Ok(false);
        }
        self.set_selection(
            DocumentPosition::at(path.clone(), start),
            DocumentPosition::at(path, end),
        );
        self.delete_selection()?;
        Ok(true)
    }

    pub fn delete_forward(&mut self) -> EditResult {
        if self.leaf_count() == 0 {
            return Err(EditError::EmptyDocument);
        }
        if self.selection.is_some() {
            return self.delete_selection();
        }

        // Reveal codes: deleting into an inline-style tag removes the style.
        if self.remove_reveal_tag(false) {
            return Ok(());
        }

        let path = self.cursor.path.clone();
        let offset = self.cursor.offset;
        let len = self.leaf_text_len(&path);

        if self.is_table_leaf(&path) {
            return Ok(());
        }
        if offset >= len {
            // Merge the following leaf into this one (cursor stays at the join point).
            if let Some(next_path) = tree_walk::next_leaf_path(&self.tdoc, &path)
                && !self.is_table_leaf(&next_path)
            {
                tree_edit::merge_with_previous(&mut self.tdoc, &next_path);
                self.cursor = DocumentPosition::at(path, len);
                self.normalize_cursor();
            }
            return Ok(());
        }

        let next = tree_walk::next_grapheme_position(
            &self.tdoc,
            &DocumentPosition::at(path.clone(), offset),
        )
        .offset;
        if next > offset {
            self.edit_leaf(&path, |content| {
                let mut block = Block::paragraph();
                block.content = std::mem::take(content);
                block.delete_text_range(offset, next);
                *content = block.content;
            });
        }
        self.normalize_cursor();
        Ok(())
    }

    pub fn delete_word_backward(&mut self) -> EditResult {
        if self.leaf_count() == 0 {
            return Err(EditError::EmptyDocument);
        }
        if self.selection.is_some() {
            return self.delete_selection();
        }
        if self.is_table_leaf(&self.cursor.path.clone()) {
            return self.delete_backward();
        }
        let from = self.cursor.clone();
        let to = self.word_left_position(&from);
        if to == from || to.path != from.path {
            // Cross-leaf word delete is handled as a plain backspace in Phase 1.
            return self.delete_backward();
        }
        self.delete_intra_leaf_range(&to.path, to.offset, from.offset);
        self.cursor = to;
        self.normalize_cursor();
        self.selection = None;
        Ok(())
    }

    pub fn delete_word_forward(&mut self) -> EditResult {
        if self.leaf_count() == 0 {
            return Err(EditError::EmptyDocument);
        }
        if self.selection.is_some() {
            return self.delete_selection();
        }
        if self.is_table_leaf(&self.cursor.path.clone()) {
            return self.delete_forward();
        }
        let from = self.cursor.clone();
        let to = self.word_right_position(&from);
        if to == from || to.path != from.path {
            return self.delete_forward();
        }
        self.delete_intra_leaf_range(&from.path, from.offset, to.offset);
        self.normalize_cursor();
        self.selection = None;
        Ok(())
    }

    pub fn delete_selection(&mut self) -> EditResult {
        let Some((a, b)) = self.selection.clone() else {
            return Ok(());
        };
        let (start, end) = if a <= b { (a, b) } else { (b, a) };

        if start.path == end.path {
            self.delete_intra_leaf_range(&start.path, start.offset, end.offset);
            self.cursor = start.clone();
        } else if let (Some(s_idx), Some(e_idx)) =
            (self.leaf_index(&start.path), self.leaf_index(&end.path))
        {
            // Truncate the start leaf to its head, append the end leaf's tail, then remove
            // every leaf from start+1 through end (re-querying the next leaf each time so
            // container pruning can't invalidate stale paths).
            let end_len = self.leaf_text_len(&end.path);
            let end_runs = tree_walk::leaf_inline(&self.tdoc, &end.path);
            let end_tail = extract_inline_range(&end_runs, end.offset, end_len);
            let start_off = start.offset;
            self.edit_leaf(&start.path, |content| {
                let mut block = Block::paragraph();
                block.content = std::mem::take(content);
                let len = block.text_len();
                block.delete_text_range(start_off, len);
                block.content.extend(end_tail);
                block.normalize_content();
                *content = block.content;
            });
            for _ in 0..(e_idx.saturating_sub(s_idx)) {
                if let Some(next) = tree_walk::next_leaf_path(&self.tdoc, &start.path) {
                    tree_edit::remove_node_at(&mut self.tdoc, &next);
                } else {
                    break;
                }
            }
            self.cursor = start.clone();
        } else {
            self.cursor = start.clone();
        }
        self.selection = None;
        self.normalize_cursor();
        Ok(())
    }

    fn delete_intra_leaf_range(&mut self, path: &TreePath, start: usize, end: usize) {
        if start >= end {
            return;
        }
        self.edit_leaf(path, |content| {
            let mut block = Block::paragraph();
            block.content = std::mem::take(content);
            block.delete_text_range(start, end);
            *content = block.content;
        });
    }

    // ----- Movement ------------------------------------------------------------------

    fn prev_leaf(&self, path: &TreePath) -> Option<TreePath> {
        tree_walk::prev_leaf_path(&self.tdoc, path)
    }

    fn next_leaf(&self, path: &TreePath) -> Option<TreePath> {
        tree_walk::next_leaf_path(&self.tdoc, path)
    }

    pub fn move_cursor_left(&mut self) {
        self.break_undo_coalescing();
        if self.reveal_codes {
            let (new, stop) = self.reveal_position_left();
            self.cursor = new;
            self.normalize_cursor();
            self.selection = None;
            // normalize_cursor reset the reveal-stop; restore the stepped value.
            self.cursor_reveal_stop = stop;
        } else if self.affinity_active() {
            let (new, affinity) = self.affinity_position_left();
            self.cursor = new;
            self.normalize_cursor();
            self.selection = None;
            // normalize_cursor reset the affinity; restore the stepped value.
            self.cursor_affinity = affinity;
        } else {
            self.cursor = self.position_left(&self.cursor);
            self.normalize_cursor();
            self.selection = None;
        }
    }

    pub fn move_cursor_right(&mut self) {
        self.break_undo_coalescing();
        if self.reveal_codes {
            let (new, stop) = self.reveal_position_right();
            self.cursor = new;
            self.normalize_cursor();
            self.selection = None;
            self.cursor_reveal_stop = stop;
        } else if self.affinity_active() {
            let (new, affinity) = self.affinity_position_right();
            self.cursor = new;
            self.normalize_cursor();
            self.selection = None;
            self.cursor_affinity = affinity;
        } else {
            self.cursor = self.position_right(&self.cursor);
            self.normalize_cursor();
            self.selection = None;
        }
    }

    fn position_left(&self, pos: &DocumentPosition) -> DocumentPosition {
        if pos.offset > 0 {
            tree_walk::previous_grapheme_position(&self.tdoc, pos)
        } else if let Some(prev) = self.prev_leaf(&pos.path) {
            let len = self.leaf_text_len(&prev);
            DocumentPosition::at(prev, len)
        } else {
            pos.clone()
        }
    }

    fn position_right(&self, pos: &DocumentPosition) -> DocumentPosition {
        let len = self.leaf_text_len(&pos.path);
        if pos.offset < len {
            tree_walk::next_grapheme_position(&self.tdoc, pos)
        } else if let Some(next) = self.next_leaf(&pos.path) {
            DocumentPosition::at(next, 0)
        } else {
            pos.clone()
        }
    }

    pub fn move_cursor_up(&mut self) {
        self.break_undo_coalescing();
        if let Some(prev) = self.prev_leaf(&self.cursor.path) {
            let len = self.leaf_text_len(&prev);
            self.cursor = DocumentPosition::at(prev, self.cursor.offset.min(len));
            self.normalize_cursor();
        }
        self.selection = None;
    }

    pub fn move_cursor_down(&mut self) {
        self.break_undo_coalescing();
        if let Some(next) = self.next_leaf(&self.cursor.path) {
            let len = self.leaf_text_len(&next);
            self.cursor = DocumentPosition::at(next, self.cursor.offset.min(len));
            self.normalize_cursor();
        }
        self.selection = None;
    }

    pub fn move_cursor_to_line_start(&mut self) {
        self.break_undo_coalescing();
        self.cursor.offset = 0;
        self.normalize_cursor();
        self.selection = None;
    }

    pub fn move_cursor_to_line_end(&mut self) {
        self.break_undo_coalescing();
        self.cursor.offset = self.leaf_text_len(&self.cursor.path.clone());
        self.normalize_cursor();
        self.selection = None;
        // Land behind any trailing reveal tags (e.g. a closing `<Bold]`).
        self.cursor_reveal_stop = self.reveal_stops_at(&self.cursor.path, self.cursor.offset);
    }

    /// Place the caret at `pos`, positioned *after* all reveal tags rendered there
    /// (the rightmost reveal-stop). Used when jumping to the end of a visual line
    /// so the caret sits behind a trailing `<Bold]` rather than before it.
    pub fn set_cursor_after_reveal_tags(&mut self, pos: DocumentPosition) {
        self.set_cursor(pos);
        self.cursor_reveal_stop = self.reveal_stops_at(&self.cursor.path, self.cursor.offset);
    }

    pub fn move_word_right(&mut self) {
        self.break_undo_coalescing();
        let (new, stop) = self.reveal_word_right();
        self.cursor = new;
        self.selection = None;
        self.cursor_reveal_stop = stop;
    }

    pub fn move_word_left(&mut self) {
        self.break_undo_coalescing();
        let (new, stop) = self.reveal_word_left();
        self.cursor = new;
        self.selection = None;
        self.cursor_reveal_stop = stop;
    }

    pub fn move_word_right_extend(&mut self) {
        let new = self.word_right_position(&self.cursor.clone());
        if new != self.cursor {
            self.extend_selection_to(new);
        }
    }

    pub fn move_word_left_extend(&mut self) {
        let new = self.word_left_position(&self.cursor.clone());
        if new != self.cursor {
            self.extend_selection_to(new);
        }
    }

    fn word_right_position(&self, pos: &DocumentPosition) -> DocumentPosition {
        let text = self.leaf_plain_text(&pos.path);
        let mut i = pos.offset.min(text.len());
        if i >= text.len() {
            if let Some(next) = self.next_leaf(&pos.path) {
                return DocumentPosition::at(next, 0);
            }
            return tree_walk::clamp_position(&self.tdoc, pos);
        }
        while i < text.len() {
            let ch = text[i..].chars().next().unwrap();
            if ch.is_whitespace() || ch.is_ascii_punctuation() {
                i += ch.len_utf8();
            } else {
                break;
            }
        }
        while i < text.len() {
            let ch = text[i..].chars().next().unwrap();
            if !(ch.is_whitespace() || ch.is_ascii_punctuation()) {
                i += ch.len_utf8();
            } else {
                break;
            }
        }
        tree_walk::clamp_position_forward(&self.tdoc, &DocumentPosition::at(pos.path.clone(), i))
    }

    fn word_left_position(&self, pos: &DocumentPosition) -> DocumentPosition {
        let text = self.leaf_plain_text(&pos.path);
        let mut i = pos.offset.min(text.len());
        if i == 0 {
            if let Some(prev) = self.prev_leaf(&pos.path) {
                let len = self.leaf_text_len(&prev);
                return DocumentPosition::at(prev, len);
            }
            return tree_walk::clamp_position(&self.tdoc, pos);
        }
        while i > 0 {
            let (prev_i, ch) = text[..i].char_indices().next_back().unwrap();
            if ch.is_whitespace() || ch.is_ascii_punctuation() {
                i = prev_i;
            } else {
                break;
            }
        }
        while i > 0 {
            let (prev_i, ch) = text[..i].char_indices().next_back().unwrap();
            if !(ch.is_whitespace() || ch.is_ascii_punctuation()) {
                i = prev_i;
            } else {
                break;
            }
        }
        tree_walk::clamp_position(&self.tdoc, &DocumentPosition::at(pos.path.clone(), i))
    }

    pub fn move_cursor_left_extend(&mut self) {
        self.normalize_cursor();
        let new = self.position_left(&self.cursor.clone());
        if new != self.cursor {
            self.extend_selection_to(new);
        }
    }

    pub fn move_cursor_right_extend(&mut self) {
        self.normalize_cursor();
        let new = self.position_right(&self.cursor.clone());
        if new != self.cursor {
            self.extend_selection_to(new);
        }
    }

    pub fn move_cursor_up_extend(&mut self) {
        if let Some(prev) = self.prev_leaf(&self.cursor.path) {
            let len = self.leaf_text_len(&prev);
            let new = DocumentPosition::at(prev, self.cursor.offset.min(len));
            self.extend_selection_to(new);
        }
    }

    pub fn move_cursor_down_extend(&mut self) {
        if let Some(next) = self.next_leaf(&self.cursor.path) {
            let len = self.leaf_text_len(&next);
            let new = DocumentPosition::at(next, self.cursor.offset.min(len));
            self.extend_selection_to(new);
        }
    }

    pub fn move_cursor_to_line_start_extend(&mut self) {
        let new = DocumentPosition::at(self.cursor.path.clone(), 0);
        if new != self.cursor {
            self.extend_selection_to(new);
        }
    }

    pub fn move_cursor_to_line_end_extend(&mut self) {
        let new = DocumentPosition::at(
            self.cursor.path.clone(),
            self.leaf_text_len(&self.cursor.path.clone()),
        );
        if new != self.cursor {
            self.extend_selection_to(new);
        }
    }

    // ----- Text extraction -----------------------------------------------------------

    pub fn text_in_range(&self, start: DocumentPosition, end: DocumentPosition) -> String {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let paths = self.leaf_paths();
        let mut out = String::new();
        let mut started = false;
        for path in &paths {
            if *path < start.path || *path > end.path {
                continue;
            }
            let text = self.leaf_plain_text(path);
            let from = if *path == start.path {
                start.offset.min(text.len())
            } else {
                0
            };
            let to = if *path == end.path {
                end.offset.min(text.len())
            } else {
                text.len()
            };
            if from < to {
                if started {
                    out.push_str("\n\n");
                }
                out.push_str(&text[from..to]);
                started = true;
            }
        }
        out
    }

    pub fn get_selection_text(&self) -> String {
        let Some((start, end)) = self.selection.clone() else {
            return String::new();
        };
        self.text_in_range(start, end)
    }

    /// The current selection as a standalone document.
    ///
    /// Block-level *types* are preserved where they have a self-contained
    /// representation — headings keep their level and code blocks stay code
    /// blocks — so copying e.g. a heading no longer degrades it to body text
    /// (this also improves the GUI's markdown/HTML clipboard fidelity).
    ///
    /// TODO(phase2): reconstruct list/checklist/quote *grouping* across a
    /// multi-leaf selection. Such leaves are still emitted as plain paragraphs
    /// here because faithfully regrouping them into standalone lists/quotes is a
    /// larger structural operation (this matches the gap with Pure's
    /// `selection_fragment`, which clones whole root paragraphs).
    pub fn get_selection_document(&self) -> Option<Document> {
        let (a, b) = self.selection.clone()?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let paths = self.leaf_paths();
        let mut paragraphs: Vec<Paragraph> = Vec::new();
        for path in &paths {
            if *path < start.path || *path > end.path {
                continue;
            }
            let content = tree_walk::leaf_inline(&self.tdoc, path);
            let len: usize = content.iter().map(|c| c.text_len()).sum();
            let from = if *path == start.path {
                start.offset.min(len)
            } else {
                0
            };
            let to = if *path == end.path {
                end.offset.min(len)
            } else {
                len
            };
            if from >= to {
                continue;
            }
            let runs = extract_inline_range(&content, from, to);
            let spans = inline_to_spans(&runs);
            let paragraph = match self.block_type_at(path) {
                BlockType::Heading { level: 1 } => Paragraph::new_header1(),
                BlockType::Heading { level: 2 } => Paragraph::new_header2(),
                BlockType::Heading { .. } => Paragraph::new_header3(),
                BlockType::CodeBlock { .. } => Paragraph::new_code_block(),
                // Paragraph, BlockQuote, ListItem, Table: emit as plain text for
                // now (see phase2 note above).
                _ => Paragraph::new_text(),
            };
            paragraphs.push(paragraph.with_content(spans));
        }
        if paragraphs.is_empty() {
            None
        } else {
            Some(Document::new().with_paragraphs(paragraphs))
        }
    }

    pub fn cut(&mut self) -> Result<String, EditError> {
        let text = self.get_selection_text();
        if !text.is_empty() {
            self.delete_selection()?;
        }
        Ok(text)
    }

    pub fn copy(&self) -> String {
        self.get_selection_text()
    }

    pub fn paste(&mut self, text: &str) -> EditResult {
        let normalized = normalize_plain_text(text);
        if normalized.is_empty() {
            return Ok(());
        }
        if !normalized.contains('\n') {
            return self.insert_text(&normalized);
        }
        // Multi-line plain text: each line becomes its own paragraph.
        let paragraphs = normalized
            .split('\n')
            .map(|line| Paragraph::new_text().with_content(vec![Span::new_text(line)]))
            .collect();
        let doc = Document::new().with_paragraphs(paragraphs);
        self.insert_document(&doc)
    }

    // ----- Structural ops (TODO(phase2/3)) -------------------------------------------

    /// Cycle the current line: Paragraph → H1 → H2 → H3 → Paragraph.
    pub fn toggle_heading(&mut self) -> EditResult {
        let next = match self.current_block_type() {
            BlockType::Heading { level: 1 } => BlockType::Heading { level: 2 },
            BlockType::Heading { level: 2 } => BlockType::Heading { level: 3 },
            BlockType::Heading { .. } => BlockType::Paragraph,
            _ => BlockType::Heading { level: 1 },
        };
        self.set_block_type(next)
    }

    /// Insert an arbitrary inline element at the cursor (within the current leaf).
    pub fn insert_inline_at_cursor(&mut self, inline: InlineContent) -> EditResult {
        if self.leaf_count() == 0 {
            let len = inline.text_len();
            self.tdoc
                .add_paragraph(Paragraph::new_text().with_content(inline_to_spans(&[inline])));
            self.cursor = DocumentPosition::at(TreePath::root(0), len);
            return Ok(());
        }
        if self.selection.is_some() {
            self.delete_selection()?;
        }
        let path = self.cursor.path.clone();
        if self.is_table_leaf(&path) {
            return Ok(());
        }
        let offset = self.cursor.offset;
        let inserted_len = inline.text_len();
        self.edit_leaf(&path, |content| {
            let mut block = Block::paragraph();
            block.content = std::mem::take(content);
            let right = block.split_content_at(offset);
            block.content.push(inline);
            block.content.extend(right);
            block.normalize_content();
            *content = block.content;
        });
        self.cursor.offset = offset + inserted_len;
        self.selection = None;
        Ok(())
    }

    /// Replace the current selection with a link.
    pub fn replace_selection_with_link(&mut self, destination: &str, text: &str) -> EditResult {
        self.delete_selection()?;
        self.insert_link_at_cursor(destination, text)
    }

    /// Insert a link at the cursor.
    pub fn insert_link_at_cursor(&mut self, destination: &str, text: &str) -> EditResult {
        let link_inline = InlineContent::Link {
            link: super::structured_document::Link {
                destination: destination.to_string(),
                title: None,
            },
            content: vec![InlineContent::Text(TextRun::plain(text))],
        };
        self.insert_inline_at_cursor(link_inline)
    }

    /// Wrap the current selection in a link, **preserving** the selected runs'
    /// inline styling (a bold selection stays bold inside the link). Any links
    /// already inside the selection are flattened so links never nest. Falls back
    /// to a plain-text link for a cross-leaf selection (rare) or no selection.
    pub fn wrap_selection_in_link(&mut self, destination: &str) -> EditResult {
        let Some((a, b)) = self.selection.clone() else {
            return self.insert_link_at_cursor(destination, destination);
        };
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        if start.path != end.path {
            let text = self.get_selection_text();
            return self.replace_selection_with_link(destination, &text);
        }
        let path = start.path.clone();
        if self.is_table_leaf(&path) {
            return Ok(());
        }
        let (from, to) = (start.offset, end.offset);
        let dest = destination.to_string();
        self.edit_leaf(&path, |content| {
            let (before, selected, after) = split_content_for_style(content, from, to);
            // Flatten any links already in the range so the new link can't nest.
            let inner = unwrap_links(selected);
            let mut out = before;
            if !inner.is_empty() {
                out.push(InlineContent::Link {
                    link: super::structured_document::Link {
                        destination: dest.clone(),
                        title: None,
                    },
                    content: inner,
                });
            }
            out.extend(after);
            *content = out;
        });
        self.cursor = DocumentPosition::at(path, to);
        self.selection = None;
        self.normalize_cursor();
        Ok(())
    }

    /// Edit an existing link at the given leaf path + inline index.
    pub fn edit_link_at(
        &mut self,
        path: TreePath,
        inline_index: usize,
        destination: &str,
        text: &str,
    ) -> EditResult {
        let dest = destination.to_string();
        let text = text.to_string();
        let ok = self.edit_leaf(&path, |content| {
            if let Some(InlineContent::Link {
                link,
                content: inner,
            }) = content.get_mut(inline_index)
            {
                link.destination = dest.clone();
                *inner = vec![InlineContent::Text(TextRun::plain(text.clone()))];
                true
            } else {
                false
            }
        });
        if ok {
            Ok(())
        } else {
            Err(EditError::InvalidPosition)
        }
    }

    /// Remove (unwrap) a link at the given leaf path + inline index, keeping its text.
    pub fn remove_link_at(&mut self, path: TreePath, inline_index: usize) -> EditResult {
        let ok = self.edit_leaf(&path, |content| {
            if inline_index >= content.len() {
                return false;
            }
            if let InlineContent::Link { content: inner, .. } = content.remove(inline_index) {
                for (i, item) in inner.into_iter().enumerate() {
                    content.insert(inline_index + i, item);
                }
                true
            } else {
                false
            }
        });
        if ok {
            Ok(())
        } else {
            Err(EditError::InvalidPosition)
        }
    }

    pub fn toggle_bold(&mut self) -> EditResult {
        self.apply_style_to_selection(|s| s.bold = !s.bold)
    }
    pub fn toggle_italic(&mut self) -> EditResult {
        self.apply_style_to_selection(|s| s.italic = !s.italic)
    }
    pub fn toggle_code(&mut self) -> EditResult {
        self.apply_style_to_selection(|s| s.code = !s.code)
    }
    pub fn toggle_strikethrough(&mut self) -> EditResult {
        self.apply_style_to_selection(|s| s.strikethrough = !s.strikethrough)
    }
    pub fn toggle_underline(&mut self) -> EditResult {
        self.apply_style_to_selection(|s| s.underline = !s.underline)
    }
    pub fn toggle_highlight(&mut self) -> EditResult {
        self.apply_style_to_selection(|s| s.highlight = !s.highlight)
    }
    pub fn clear_formatting(&mut self) -> EditResult {
        self.apply_style_to_selection(|s| *s = TextStyle::default())
    }

    /// Apply a style mutation to every run within the current selection, across leaves.
    fn apply_style_to_selection(&mut self, mut apply: impl FnMut(&mut TextStyle)) -> EditResult {
        let Some((a, b)) = self.selection.clone() else {
            return Ok(());
        };
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        for path in self.leaf_paths() {
            if path < start.path || path > end.path {
                continue;
            }
            let len = self.leaf_text_len(&path);
            let from = if path == start.path {
                start.offset.min(len)
            } else {
                0
            };
            let to = if path == end.path {
                end.offset.min(len)
            } else {
                len
            };
            if from >= to {
                continue;
            }
            self.edit_leaf(&path, |content| {
                let (before, selected, after) = split_content_for_style(content, from, to);
                let styled = map_style_on_runs(selected, &mut apply);
                let combined: Vec<InlineContent> =
                    before.into_iter().chain(styled).chain(after).collect();
                // Re-merge runs (and link pieces) the split produced.
                let mut block = Block::paragraph();
                block.content = combined;
                block.normalize_content();
                *content = block.content;
            });
        }
        Ok(())
    }

    pub fn toggle_list(&mut self) -> EditResult {
        self.toggle_list_kind(false, false)
    }
    pub fn toggle_checklist(&mut self) -> EditResult {
        self.toggle_list_kind(false, true)
    }
    pub fn toggle_ordered_list(&mut self) -> EditResult {
        self.toggle_list_kind(true, false)
    }

    pub fn toggle_quote(&mut self) -> EditResult {
        // In a quote already → unwrap; otherwise convert the cursor's top-level block(s) to
        // a quote (flattening to plain text, mirroring how lists convert).
        if matches!(self.cursor.path.last(), Some(PathSegment::QuoteChild(_))) {
            self.unwrap_quote()
        } else {
            self.convert_selection_to_quote()
        }
    }

    pub fn toggle_code_block(&mut self) -> EditResult {
        if matches!(self.current_block_type(), BlockType::CodeBlock { .. }) {
            self.apply_variant_over_selection(|s| Paragraph::new_text().with_content(s))
        } else {
            self.apply_variant_over_selection(|s| Paragraph::new_code_block().with_content(s))
        }
    }

    /// Set the block type of the cursor's *pseudo-leaf* (the effective block per the block
    /// model). A single-text-child container behaves like a leaf of its own type, so a
    /// block-type change there acts on the container; otherwise the change acts on the
    /// genuine leaf. See `tree_walk::effective_block_type`.
    pub fn set_block_type(&mut self, block_type: BlockType) -> EditResult {
        // A selection spanning several items of one list, converted to a *different* list
        // kind, carves those items out into their own list (splitting the original around
        // them) rather than converting the whole list.
        if let BlockType::ListItem {
            ordered, checkbox, ..
        } = block_type
            && let Some((list_path, s, e)) = self.selection_list_item_range()
        {
            let target = tree_edit::ListKind::from_flags(ordered, checkbox.is_some());
            if self.convert_list_item_range_at(&list_path, s, e, target) {
                return Ok(());
            }
        }
        let path = self.cursor.path.clone();
        if tree_walk::cursor_in_collapsed_container(&self.tdoc, &path) {
            return self.set_collapsed_container_block_type(&path, block_type);
        }
        match block_type {
            BlockType::Paragraph => {
                // "Paragraph" also exits a block quote (mirrors the flat block-type model).
                if matches!(path.last(), Some(PathSegment::QuoteChild(_))) {
                    self.unwrap_quote()
                } else {
                    self.apply_variant_over_selection(|s| Paragraph::new_text().with_content(s))
                }
            }
            BlockType::Heading { level } => {
                let level = level.clamp(1, 3);
                self.apply_variant_over_selection(move |s| make_header(level, s))
            }
            BlockType::CodeBlock { .. } => {
                self.apply_variant_over_selection(|s| Paragraph::new_code_block().with_content(s))
            }
            BlockType::BlockQuote => self.toggle_quote(),
            BlockType::ListItem {
                ordered, checkbox, ..
            } => self.toggle_list_kind(ordered, checkbox.is_some()),
            BlockType::Table { .. } => Ok(()),
        }
    }

    /// Apply a block-type change when the cursor sits in a collapsed single-text container
    /// (the container behaves like a leaf). Leaf targets leave the container as a top-level
    /// leaf; container targets convert the container's kind.
    fn set_collapsed_container_block_type(
        &mut self,
        path: &TreePath,
        block_type: BlockType,
    ) -> EditResult {
        // The container node is the parent of the cursor's leaf.
        let container_path = TreePath(path.segments()[..path.len().saturating_sub(1)].to_vec());
        let is_quote = matches!(path.last(), Some(PathSegment::QuoteChild(_)));
        match block_type {
            // Leaf targets: the collapsed unit leaves its container as a plain paragraph.
            BlockType::Paragraph | BlockType::Heading { .. } | BlockType::CodeBlock { .. } => {
                if is_quote {
                    if !self.dissolve_container_at(&container_path) {
                        return Ok(());
                    }
                } else if !self.delist_cursor_item() {
                    // Delist the item from its immediate list into that list's container
                    // (the document, a quote, or a parent list item's paragraphs), so a
                    // nested item becomes a continuation paragraph of its parent item.
                    return Ok(());
                }
                match block_type {
                    BlockType::Heading { level } => {
                        let level = level.clamp(1, 3);
                        self.apply_variant_over_selection(move |s| make_header(level, s))
                    }
                    BlockType::CodeBlock { .. } => self.apply_variant_over_selection(|s| {
                        Paragraph::new_code_block().with_content(s)
                    }),
                    _ => Ok(()),
                }
            }
            // Quote target. A quote is a single unit, so a collapsed quote is already
            // done. A list/checklist *item*, however, is one of several siblings — convert
            // only that item: lift it out of the list (splitting the list around it) and
            // wrap it in a quote, leaving its siblings as list items.
            BlockType::BlockQuote => {
                if is_quote {
                    Ok(())
                } else if self.delist_cursor_item() {
                    // The delisted paragraph becomes a quote in the item's slot (its
                    // siblings stay in the list; a nested item stays inside its parent).
                    self.wrap_cursor_leaf_in_quote()
                } else {
                    Ok(())
                }
            }
            // List-kind target: quotes and lists alike convert as a whole (a quote's single
            // child becomes one list item; a list changes its kind across all items).
            BlockType::ListItem {
                ordered, checkbox, ..
            } => {
                let kind = if checkbox.is_some() {
                    tree_edit::ContainerKind::Checklist
                } else if ordered {
                    tree_edit::ContainerKind::Ordered
                } else {
                    tree_edit::ContainerKind::Unordered
                };
                self.convert_container_at(&container_path, kind);
                Ok(())
            }
            BlockType::Table { .. } => Ok(()),
        }
    }

    /// If the selection spans two or more items of the *same* immediate list (a list
    /// `Paragraph` node, not a checklist nested inside a checklist item), the list's path and
    /// the covered item index range `[start, end]`. Used to carve a run of items out into
    /// their own list on a list-kind change.
    fn selection_list_item_range(&self) -> Option<(TreePath, usize, usize)> {
        let (a, b) = self.selection.clone()?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let item_of = |p: &TreePath| -> Option<(TreePath, usize)> {
            let list_path = TreePath(p.segments()[..p.len().saturating_sub(1)].to_vec());
            match p.last()? {
                PathSegment::ListEntry { entry, .. } => Some((list_path, *entry)),
                PathSegment::ChecklistItem(c) => Some((list_path, *c)),
                _ => None,
            }
        };
        let (lp_a, i_a) = item_of(&start.path)?;
        let (lp_b, i_b) = item_of(&end.path)?;
        if lp_a != lp_b || i_a == i_b {
            return None;
        }
        Some((lp_a, i_a.min(i_b), i_a.max(i_b)))
    }

    /// Carve items `[s, e]` out of the list at `list_path` into a new list of `target`,
    /// splitting the original around them. Preserves the cursor/selection by flat leaf index
    /// (the split keeps document order) and merges with an adjacent same-kind list. Returns
    /// whether the tree changed.
    fn convert_list_item_range_at(
        &mut self,
        list_path: &TreePath,
        s: usize,
        e: usize,
        target: tree_edit::ListKind,
    ) -> bool {
        let sel_idx = self.selection.clone().map(|(a, b)| {
            (
                self.leaf_index(&a.path),
                a.offset,
                self.leaf_index(&b.path),
                b.offset,
            )
        });
        let cur_idx = self.leaf_index(&self.cursor.path);
        let cur_off = self.cursor.offset;
        if tree_edit::convert_list_item_range(&mut self.tdoc, list_path, s, e, target).is_none() {
            return false;
        }
        self.restore_cursor_by_leaf_index(cur_idx, cur_off);
        self.merge_lists_at_cursor();
        if let Some((sa, soff, sb, eoff)) = sel_idx {
            let leaves = self.leaf_paths();
            if let (Some(a), Some(b)) = (
                sa.and_then(|i| leaves.get(i)).cloned(),
                sb.and_then(|i| leaves.get(i)).cloned(),
            ) {
                self.selection =
                    Some((DocumentPosition::at(a, soff), DocumentPosition::at(b, eoff)));
            }
        }
        self.trigger_paragraph_change();
        true
    }

    /// Delist the cursor's list/checklist item: remove it from its immediate list and drop
    /// its paragraph into the list's enclosing container (document, quote, or a parent list
    /// item's paragraphs), splitting the list around it. The cursor follows the paragraph.
    /// Returns whether the tree changed.
    fn delist_cursor_item(&mut self) -> bool {
        let path = self.cursor.path.clone();
        let offset = self.cursor.offset;
        if let Some(new_path) = tree_edit::delist_item(&mut self.tdoc, &path) {
            self.cursor = DocumentPosition::at(new_path, offset);
            self.normalize_cursor();
            self.trigger_paragraph_change();
            true
        } else {
            false
        }
    }

    /// Replace the cursor's leaf paragraph with a quote wrapping its text, in place, and
    /// move the cursor into the quote's child.
    fn wrap_cursor_leaf_in_quote(&mut self) -> EditResult {
        let path = self.cursor.path.clone();
        let offset = self.cursor.offset;
        let changed = tree_edit::replace_leaf_variant(&mut self.tdoc, &path, |s| {
            Paragraph::new_quote().with_children(vec![Paragraph::new_text().with_content(s)])
        });
        if changed {
            self.cursor = DocumentPosition::at(path.child(PathSegment::QuoteChild(0)), offset);
            self.normalize_cursor();
            self.trigger_paragraph_change();
        }
        Ok(())
    }

    // ----- Structural helpers (top-level focus) --------------------------------------

    /// The normalized (start, end) of the selection, or the collapsed cursor.
    fn selection_or_cursor_range(&self) -> (DocumentPosition, DocumentPosition) {
        match self.selection.clone() {
            Some((a, b)) => {
                if a <= b {
                    (a, b)
                } else {
                    (b, a)
                }
            }
            None => (self.cursor.clone(), self.cursor.clone()),
        }
    }

    /// Apply an in-place leaf-variant change to every leaf in the selection/cursor range.
    fn apply_variant_over_selection(
        &mut self,
        make: impl Fn(Vec<Span>) -> Paragraph,
    ) -> EditResult {
        let (start, end) = self.selection_or_cursor_range();
        for path in self.leaf_paths() {
            if path < start.path || path > end.path {
                continue;
            }
            tree_edit::replace_leaf_variant(&mut self.tdoc, &path, &make);
        }
        self.normalize_cursor();
        self.trigger_paragraph_change();
        Ok(())
    }

    /// The cursor's top-level paragraph index, if the cursor is at the top level.
    fn cursor_top_index(&self) -> Option<usize> {
        match self.cursor.path.segments() {
            [PathSegment::Paragraph(i)] => Some(*i),
            _ => None,
        }
    }

    /// Convert the selected top-level paragraph(s) into one quote, flattening each to plain
    /// text (dropping heading/code type) — mirrors how lists convert. Top-level only.
    fn convert_selection_to_quote(&mut self) -> EditResult {
        let (start, end) = self.selection_or_cursor_range();
        let (Some(s), Some(e)) = (top_index(&start.path), top_index(&end.path)) else {
            return Ok(());
        };
        if s > e || e >= self.tdoc.paragraphs.len() {
            return Ok(());
        }
        let cursor_rel = self
            .cursor_top_index()
            .map(|ci| ci.saturating_sub(s))
            .unwrap_or(0);
        let offset = self.cursor.offset;
        let drained: Vec<Paragraph> = self.tdoc.paragraphs.drain(s..=e).collect();
        let children: Vec<Paragraph> = drained
            .into_iter()
            .map(|p| Paragraph::new_text().with_content(p.content().to_vec()))
            .collect();
        self.tdoc
            .paragraphs
            .insert(s, Paragraph::new_quote().with_children(children));
        self.cursor = DocumentPosition::at(
            TreePath::root(s).child(PathSegment::QuoteChild(cursor_rel)),
            offset,
        );
        self.normalize_cursor();
        self.trigger_paragraph_change();
        Ok(())
    }

    /// Wrap the selected top-level range in a single new container of `kind` as one unit,
    /// preserving inner paragraph types (the "wrap inside…" action). A multi-paragraph
    /// selection becomes one quote / one list item / one checklist item. Top-level only.
    pub fn wrap_selection(&mut self, kind: tree_edit::ContainerKind) -> EditResult {
        let (start, end) = self.selection_or_cursor_range();
        let (Some(s), Some(e)) = (top_index(&start.path), top_index(&end.path)) else {
            return Ok(());
        };
        if s > e || e >= self.tdoc.paragraphs.len() {
            return Ok(());
        }
        let cursor_rel = self
            .cursor_top_index()
            .map(|ci| ci.saturating_sub(s))
            .unwrap_or(0);
        let offset = self.cursor.offset;
        let drained: Vec<Paragraph> = self.tdoc.paragraphs.drain(s..=e).collect();
        let (node, leaf_seg) = match kind {
            tree_edit::ContainerKind::Quote => (
                Paragraph::new_quote().with_children(drained),
                PathSegment::QuoteChild(cursor_rel),
            ),
            tree_edit::ContainerKind::Ordered => (
                Paragraph::new_ordered_list().with_entries(vec![drained]),
                PathSegment::ListEntry {
                    entry: 0,
                    para: cursor_rel,
                },
            ),
            tree_edit::ContainerKind::Unordered => (
                Paragraph::new_unordered_list().with_entries(vec![drained]),
                PathSegment::ListEntry {
                    entry: 0,
                    para: cursor_rel,
                },
            ),
            tree_edit::ContainerKind::Checklist => {
                // Checklist items are span-only: concatenate the paragraphs' spans.
                let mut content: Vec<Span> = Vec::new();
                for p in &drained {
                    content.extend(p.content().iter().cloned());
                }
                (
                    Paragraph::new_checklist().with_checklist_items(vec![
                        ChecklistItem::new(false).with_content(content),
                    ]),
                    PathSegment::ChecklistItem(0),
                )
            }
        };
        self.tdoc.paragraphs.insert(s, node);
        self.cursor = DocumentPosition::at(TreePath::root(s).child(leaf_seg), offset);
        self.normalize_cursor();
        self.trigger_paragraph_change();
        Ok(())
    }

    /// Convert the container node at `container_path` to `target`, preserving the cursor by
    /// flat leaf index (conversion keeps text order). Merges adjacent same-kind lists when
    /// the result is a list. Returns whether the tree changed.
    fn convert_container_at(
        &mut self,
        container_path: &TreePath,
        target: tree_edit::ContainerKind,
    ) -> bool {
        let idx = self.leaf_index(&self.cursor.path);
        let offset = self.cursor.offset;
        let changed =
            tree_edit::convert_container(&mut self.tdoc, container_path, target).is_some();
        if changed {
            self.restore_cursor_by_leaf_index(idx, offset);
            self.merge_lists_at_cursor();
            self.trigger_paragraph_change();
        }
        changed
    }

    /// Dissolve the container node at `container_path`, lifting its children up one level.
    /// Returns whether the tree changed.
    fn dissolve_container_at(&mut self, container_path: &TreePath) -> bool {
        let idx = self.leaf_index(&self.cursor.path);
        let offset = self.cursor.offset;
        let changed = tree_edit::dissolve_container(&mut self.tdoc, container_path).is_some();
        if changed {
            self.restore_cursor_by_leaf_index(idx, offset);
            self.trigger_paragraph_change();
        }
        changed
    }

    /// Re-anchor the cursor onto the leaf at flat index `idx` (stable across structural ops
    /// that keep text order), preserving `offset`.
    fn restore_cursor_by_leaf_index(&mut self, idx: Option<usize>, offset: usize) {
        if let Some(idx) = idx {
            let leaves = self.leaf_paths();
            if let Some(p) = leaves.get(idx) {
                self.cursor = DocumentPosition::at(p.clone(), offset);
            }
        }
        self.normalize_cursor();
    }

    /// If the cursor sits in a list/checklist item, merge its list with adjacent same-kind
    /// sibling lists (a no-op otherwise). Preserves the cursor by flat leaf index.
    fn merge_lists_at_cursor(&mut self) {
        let path = self.cursor.path.clone();
        if !matches!(
            path.last(),
            Some(PathSegment::ListEntry { .. } | PathSegment::ChecklistItem(_))
        ) {
            return;
        }
        let list_path = TreePath(path.segments()[..path.len().saturating_sub(1)].to_vec());
        let idx = self.leaf_index(&self.cursor.path);
        let offset = self.cursor.offset;
        tree_edit::merge_adjacent_lists(&mut self.tdoc, &list_path);
        self.restore_cursor_by_leaf_index(idx, offset);
    }

    // ----- Block-model queries / container ops (menu-facing) -------------------------

    /// The effective (pseudo-leaf) block type at the cursor — the type `set_block_type`
    /// acts on and the status bar shows as the rightmost crumb.
    pub fn cursor_effective_block_type(&self) -> BlockType {
        tree_walk::effective_block_type(&self.tdoc, &self.cursor.path)
    }

    /// The block-type breadcrumb from outermost container to the pseudo-leaf.
    pub fn cursor_block_breadcrumb(&self) -> Vec<BlockType> {
        tree_walk::block_breadcrumb(&self.tdoc, &self.cursor.path)
    }

    /// Whether the cursor's leaf can be lifted out of its container one level (`[`).
    pub fn cursor_can_unnest(&self) -> bool {
        matches!(
            self.cursor.path.last(),
            Some(
                PathSegment::ListEntry { .. }
                    | PathSegment::ChecklistItem(_)
                    | PathSegment::QuoteChild(_)
            )
        )
    }

    /// Whether `]` / Tab can indent here: a list/checklist item (nest deeper), or a
    /// top-level paragraph or selection adjacent to a container (nest into it).
    pub fn cursor_can_indent(&self) -> bool {
        self.cursor_is_list_item() || self.can_nest_selection_into_adjacent()
    }

    /// The sibling paragraph range `[s, e]` covered by the selection (or the cursor) when both
    /// endpoints are paragraphs sharing one parent container — the document top level or a
    /// single quote's children. Returned as `(first_path, s, e)` (`first_path` locates the
    /// sibling vec). `None` if the endpoints are in different containers or inside a list item.
    fn selection_sibling_range(&self) -> Option<(TreePath, usize, usize)> {
        let (start, end) = self.selection_or_cursor_range();
        // Both endpoints must live directly in the same parent container.
        let sp = start.path.segments();
        let ep = end.path.segments();
        if sp.len() != ep.len() || sp[..sp.len() - 1] != ep[..ep.len() - 1] {
            return None;
        }
        Some((
            start.path.clone(),
            sibling_index(&start.path)?,
            sibling_index(&end.path)?,
        ))
    }

    /// Whether the selected paragraph(s) sit next to a container they can nest into — a
    /// container immediately before (append) or immediately after (prepend), among their
    /// siblings. Works at the document top level and inside a quote. Preceding takes priority.
    pub fn can_nest_selection_into_adjacent(&self) -> bool {
        let Some((path, s, e)) = self.selection_sibling_range() else {
            return false;
        };
        tree_edit::has_adjacent_container(&self.tdoc, &path, s, e)
    }

    /// Indent (`]` / Tab): nest a list/checklist item one level deeper, or nest the selected
    /// top-level paragraph(s) into an adjacent container.
    pub fn indent(&mut self) -> EditResult {
        if self.cursor_is_list_item() {
            self.indent_list_item()
        } else if self.can_nest_selection_into_adjacent() {
            self.nest_selection_into_adjacent()
        } else {
            Ok(())
        }
    }

    /// Move the selected paragraph(s) into an adjacent container: appended to a container
    /// immediately before them (as new items / children at the end), or, failing that,
    /// prepended to a container immediately after them (at the start). Each paragraph becomes
    /// its own list/checklist item (or quote child). Works at the document top level or among
    /// a quote's children. The inverse of `[`. Selection and cursor are preserved (by flat
    /// leaf index, which the move keeps stable).
    fn nest_selection_into_adjacent(&mut self) -> EditResult {
        let Some((path, s, e)) = self.selection_sibling_range() else {
            return Ok(());
        };

        // Capture positions to restore by flat leaf index (the move preserves document order).
        let sel_idx = self.selection.clone().map(|(a, b)| {
            (
                self.leaf_index(&a.path),
                a.offset,
                self.leaf_index(&b.path),
                b.offset,
            )
        });
        let cur_idx = self.leaf_index(&self.cursor.path);
        let cur_off = self.cursor.offset;

        if !tree_edit::nest_paragraphs_into_adjacent(&mut self.tdoc, &path, s, e) {
            return Ok(());
        }

        // Restore the cursor (needed for the merge), merge same-kind neighbours, then
        // restore the selection over the now-nested items.
        self.restore_cursor_by_leaf_index(cur_idx, cur_off);
        self.merge_lists_at_cursor();
        if let Some((sa, soff, sb, eoff)) = sel_idx {
            let leaves = self.leaf_paths();
            if let (Some(a), Some(b)) = (
                sa.and_then(|i| leaves.get(i)).cloned(),
                sb.and_then(|i| leaves.get(i)).cloned(),
            ) {
                self.selection =
                    Some((DocumentPosition::at(a, soff), DocumentPosition::at(b, eoff)));
            }
        }
        self.trigger_paragraph_change();
        Ok(())
    }

    /// The cursor path length (number of segments); used by the "select parent" menu.
    pub fn cursor_depth(&self) -> usize {
        self.cursor.path.len()
    }

    /// Whether the cursor's innermost level is a collapsed single-text container.
    pub fn cursor_in_collapsed_container(&self) -> bool {
        tree_walk::cursor_in_collapsed_container(&self.tdoc, &self.cursor.path)
    }

    /// The container-kind block type of the ancestor container at path `depth`
    /// (1..=len-1), for labelling the "select parent" menu.
    pub fn container_block_at_depth(&self, depth: usize) -> Option<BlockType> {
        let segs = self.cursor.path.segments();
        if depth == 0 || depth >= segs.len() {
            return None;
        }
        let path = TreePath(segs[..depth].to_vec());
        tree_walk::container_block_at(&self.tdoc, &path)
    }

    /// Convert the ancestor container at path `depth` to `target` (the "select parent →
    /// convert" action). Returns whether the tree changed.
    pub fn convert_container_at_depth(
        &mut self,
        depth: usize,
        target: tree_edit::ContainerKind,
    ) -> bool {
        let segs = self.cursor.path.segments();
        if depth == 0 || depth >= segs.len() {
            return false;
        }
        let path = TreePath(segs[..depth].to_vec());
        self.convert_container_at(&path, target)
    }

    /// Dissolve the ancestor container at path `depth` (the "select parent → unwrap"
    /// action). Returns whether the tree changed.
    pub fn dissolve_container_at_depth(&mut self, depth: usize) -> bool {
        let segs = self.cursor.path.segments();
        if depth == 0 || depth >= segs.len() {
            return false;
        }
        let path = TreePath(segs[..depth].to_vec());
        self.dissolve_container_at(&path)
    }

    /// Whether the container at path `depth` can be dissolved (its parent is the document or
    /// a quote — the containers `container_splice` supports). Used to gate the "unwrap"
    /// item in the "select parent" menu.
    pub fn container_dissolvable_at_depth(&self, depth: usize) -> bool {
        let segs = self.cursor.path.segments();
        if depth == 0 || depth >= segs.len() {
            return false;
        }
        matches!(
            segs[depth - 1],
            PathSegment::Paragraph(_) | PathSegment::QuoteChild(_)
        )
    }

    fn unwrap_quote(&mut self) -> EditResult {
        let segs = self.cursor.path.segments().to_vec();
        let [PathSegment::Paragraph(i), PathSegment::QuoteChild(c)] = segs.as_slice() else {
            return Ok(());
        };
        let (i, c) = (*i, *c);
        let offset = self.cursor.offset;
        let Some(Paragraph::Quote { children }) = self.tdoc.paragraphs.get(i).cloned() else {
            return Ok(());
        };
        self.tdoc.paragraphs.remove(i);
        let count = children.len();
        for (k, child) in children.into_iter().enumerate() {
            self.tdoc.paragraphs.insert(i + k, child);
        }
        let target = (i + c.min(count.saturating_sub(1))).min(self.tdoc.paragraphs.len());
        self.cursor = DocumentPosition::at(TreePath::root(target), offset);
        self.normalize_cursor();
        self.trigger_paragraph_change();
        Ok(())
    }

    /// Toggle a list of the given kind over the contiguous top-level selection. If the
    /// cursor is already in a list, act on the list that *directly* contains it: requesting
    /// a different kind converts that list in place (preserving nesting); requesting the
    /// same kind toggles a top-level list off (unwrapping it to paragraphs) but is a no-op
    /// for a nested item — re-selecting the kind it already has should not move it.
    fn toggle_list_kind(&mut self, ordered: bool, checklist: bool) -> EditResult {
        let target = tree_edit::ListKind::from_flags(ordered, checklist);
        // A selection spanning several top-level paragraphs must convert the same way no matter
        // where the cursor sits within it or how the block kinds are mixed — otherwise the
        // outcome depends on the selection's direction. Handle that whole-range case here;
        // a selection confined to one top-level block keeps the cursor-aware path below (which
        // handles nested items, in-place kind swaps, and merging with adjacent lists).
        if let Some((s, e)) = self.selected_top_level_span()
            && s < e
        {
            return self.toggle_list_kind_over_range(s, e, target);
        }
        if let Some(current) = tree_edit::containing_list_kind(&self.tdoc, &self.cursor.path) {
            if current == target {
                // Already this kind. A nested item stays put; a top-level list toggles off.
                if self.cursor_is_nested_list_item() {
                    return Ok(());
                }
                return match self.cursor_top_level_list_index() {
                    Some(i) => self.unwrap_list_at(i),
                    // Toggling a list off means "no longer a list", so delist (a list in a
                    // quote becomes plain quote paragraphs) rather than lifting it out.
                    None => self.outdent_list_item_delisting(),
                };
            }
            // Different kind → convert just the containing list, keeping the nesting intact.
            let path = self.cursor.path.clone();
            let offset = self.cursor.offset;
            if let Some(new_path) = tree_edit::change_list_kind(&mut self.tdoc, &path, target) {
                self.cursor = DocumentPosition::at(new_path, offset);
                self.normalize_cursor();
                self.trigger_paragraph_change();
                // Converting the list may leave it adjacent to a same-kind sibling.
                self.merge_lists_at_cursor();
            }
            return Ok(());
        }

        // Otherwise wrap the selected top-level paragraphs into one list.
        let (start, end) = self.selection_or_cursor_range();
        let (Some(s), Some(e)) = (top_index(&start.path), top_index(&end.path)) else {
            return Ok(());
        };
        if s > e || e >= self.tdoc.paragraphs.len() {
            return Ok(());
        }
        let cursor_rel = self
            .cursor_top_index()
            .map(|ci| ci.saturating_sub(s))
            .unwrap_or(0);
        let offset = self.cursor.offset;
        let drained: Vec<Paragraph> = self.tdoc.paragraphs.drain(s..=e).collect();

        let new_node = if checklist {
            let items = drained
                .into_iter()
                .map(|p| ChecklistItem::new(false).with_content(p.content().to_vec()))
                .collect();
            Paragraph::new_checklist().with_checklist_items(items)
        } else {
            let entries: Vec<Vec<Paragraph>> = drained
                .into_iter()
                .map(|p| vec![Paragraph::new_text().with_content(p.content().to_vec())])
                .collect();
            if ordered {
                Paragraph::new_ordered_list().with_entries(entries)
            } else {
                Paragraph::new_unordered_list().with_entries(entries)
            }
        };
        self.tdoc.paragraphs.insert(s, new_node);

        let leaf_seg = if checklist {
            PathSegment::ChecklistItem(cursor_rel)
        } else {
            PathSegment::ListEntry {
                entry: cursor_rel,
                para: 0,
            }
        };
        self.cursor = DocumentPosition::at(TreePath::root(s).child(leaf_seg), offset);
        self.normalize_cursor();
        self.trigger_paragraph_change();
        // The freshly wrapped list may abut a same-kind sibling (e.g. a paragraph turned
        // into a checklist item next to an existing checklist) — fold them into one list.
        self.merge_lists_at_cursor();
        Ok(())
    }

    /// The inclusive range of top-level paragraph indices the current selection covers, or
    /// `None` when there is no active selection. The endpoints may sit at any depth (inside a
    /// list item, quote, …); only their owning top-level paragraph matters here.
    fn selected_top_level_span(&self) -> Option<(usize, usize)> {
        let (a, b) = self.selection.clone()?;
        let s = top_para_index(&a.path)?;
        let e = top_para_index(&b.path)?;
        Some((s.min(e), s.max(e)))
    }

    /// Convert the top-level paragraphs in `s..=e` as one unit. If every paragraph in the range
    /// is already a list of `target` kind, toggle it off (delist to plain paragraphs);
    /// otherwise fold the whole range into a single list of `target` kind, flattening plain
    /// paragraphs to items and remapping any other-kind lists. The result is independent of
    /// where the cursor sits in the range. Afterwards the transformed region is re-selected so
    /// a follow-up toggle acts on the same span.
    fn toggle_list_kind_over_range(
        &mut self,
        mut s: usize,
        mut e: usize,
        target: tree_edit::ListKind,
    ) -> EditResult {
        if s >= e || e >= self.tdoc.paragraphs.len() {
            return Ok(());
        }
        let all_target = self.tdoc.paragraphs[s..=e]
            .iter()
            .all(|p| tree_edit::list_node_kind(p) == Some(target));

        let (first, last) = if all_target {
            // Toggle off: expand every list in the range back into plain paragraphs.
            let drained: Vec<Paragraph> = self.tdoc.paragraphs.drain(s..=e).collect();
            let expanded = tree_edit::lists_into_paragraphs(drained);
            let last = s + expanded.len().saturating_sub(1);
            self.tdoc.paragraphs.splice(s..s, expanded);
            (TreePath::root(s), TreePath::root(last))
        } else {
            // Apply: also absorb any same-kind list sibling immediately adjacent to the range,
            // so the outcome is one contiguous list (adjacent same-kind lists are a single node
            // everywhere else — e.g. after a Markdown round-trip — so we keep that invariant).
            while s > 0 && tree_edit::list_node_kind(&self.tdoc.paragraphs[s - 1]) == Some(target) {
                s -= 1;
            }
            while e + 1 < self.tdoc.paragraphs.len()
                && tree_edit::list_node_kind(&self.tdoc.paragraphs[e + 1]) == Some(target)
            {
                e += 1;
            }
            let drained: Vec<Paragraph> = self.tdoc.paragraphs.drain(s..=e).collect();
            let new_node = tree_edit::paragraphs_into_list(drained, target);
            let item_count = list_item_count(&new_node);
            self.tdoc.paragraphs.insert(s, new_node);
            let leaf = |item: usize| match target {
                tree_edit::ListKind::Checklist => {
                    TreePath::root(s).child(PathSegment::ChecklistItem(item))
                }
                _ => TreePath::root(s).child(PathSegment::ListEntry {
                    entry: item,
                    para: 0,
                }),
            };
            (leaf(0), leaf(item_count.saturating_sub(1)))
        };

        // Select the whole transformed region; the cursor rides its end. Endpoints are clamped
        // to real leaves (`last` may address a list node in the delist case).
        let start = tree_walk::clamp_position(&self.tdoc, &DocumentPosition::at(first, 0));
        let end =
            tree_walk::clamp_position_forward(&self.tdoc, &DocumentPosition::at(last, usize::MAX));
        self.cursor = end.clone();
        self.selection = Some((start, end));
        self.normalize_cursor();
        self.trigger_paragraph_change();
        Ok(())
    }

    /// If the cursor's *immediate* containing list is a top-level Document list, its index.
    fn cursor_top_level_list_index(&self) -> Option<usize> {
        match self.cursor.path.segments() {
            [
                PathSegment::Paragraph(i),
                PathSegment::ListEntry { .. } | PathSegment::ChecklistItem(_),
            ] => matches!(
                self.tdoc.paragraphs.get(*i),
                Some(
                    Paragraph::OrderedList { .. }
                        | Paragraph::UnorderedList { .. }
                        | Paragraph::Checklist { .. }
                )
            )
            .then_some(*i),
            _ => None,
        }
    }

    /// Replace the top-level list/checklist at index `i` with its items as paragraphs. Each
    /// entry's first paragraph becomes a plain paragraph (losing the bullet); continuation
    /// paragraphs and nested sublists are lifted out alongside it rather than discarded.
    fn unwrap_list_at(&mut self, i: usize) -> EditResult {
        let offset = self.cursor.offset;
        // Which item does the cursor sit in?
        let cursor_item = match self.cursor.path.segments() {
            [_, PathSegment::ListEntry { entry, .. }] => *entry,
            [_, PathSegment::ChecklistItem(c)] => *c,
            _ => 0,
        };
        let Some(node) = self.tdoc.paragraphs.get(i).cloned() else {
            return Ok(());
        };
        let mut paragraphs: Vec<Paragraph> = Vec::new();
        // The top-level index of the paragraph the cursor's item maps to.
        let mut cursor_target = i;
        match node {
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
                for (e, entry) in entries.into_iter().enumerate() {
                    if e == cursor_item {
                        cursor_target = i + paragraphs.len();
                    }
                    let mut paras = entry.into_iter();
                    if let Some(first) = paras.next() {
                        paragraphs
                            .push(Paragraph::new_text().with_content(first.content().to_vec()));
                    }
                    paragraphs.extend(paras);
                }
            }
            Paragraph::Checklist { items } => {
                for (c, item) in items.into_iter().enumerate() {
                    if c == cursor_item {
                        cursor_target = i + paragraphs.len();
                    }
                    paragraphs.push(Paragraph::new_text().with_content(item.content));
                    if !item.children.is_empty() {
                        paragraphs
                            .push(Paragraph::new_checklist().with_checklist_items(item.children));
                    }
                }
            }
            _ => return Ok(()),
        }
        self.tdoc.paragraphs.splice(i..=i, paragraphs);
        self.cursor = DocumentPosition::at(TreePath::root(cursor_target), offset);
        self.normalize_cursor();
        self.trigger_paragraph_change();
        Ok(())
    }

    pub fn toggle_checkmark_at(&mut self, path: TreePath) -> Result<bool, EditError> {
        tree_edit::toggle_checkmark(&mut self.tdoc, &path).ok_or(EditError::InvalidPosition)
    }

    pub fn toggle_current_checkmark(&mut self) -> Result<bool, EditError> {
        let path = self.cursor.path.clone();
        tree_edit::toggle_checkmark(&mut self.tdoc, &path).ok_or(EditError::InvalidPosition)
    }

    /// Move the block at the cursor one step up in reading order, crossing container
    /// boundaries. Within a list it reorders siblings (the whole entry moves, carrying its
    /// continuation paragraphs and sublists); at a list's edge the item leaves the list as a
    /// same-kind single-item list and moves past the block above it in one step, then keeps
    /// hopping and merges into the next same-kind list it reaches — a plain paragraph that
    /// meets a list is drawn into it, and a quote child at the quote's edge is lifted out.
    /// No-op only for a block already at the document's start (a first list item with nothing
    /// above its list, or a nested sublist item at its edge).
    pub fn move_blocks_up(&mut self) -> Result<bool, EditError> {
        self.move_current_block(true)
    }

    /// Move the block at the cursor one step down in reading order; the counterpart to
    /// [`Self::move_blocks_up`]. No-op at the document's end.
    pub fn move_blocks_down(&mut self) -> Result<bool, EditError> {
        self.move_current_block(false)
    }

    /// Move the cursor's block one step up/down in reading order, following it with the
    /// cursor. Crosses container boundaries (see [`tree_edit::move_block`]). A selection that
    /// spans several sibling blocks moves them together, keeping them selected. Returns whether
    /// the tree changed.
    fn move_current_block(&mut self, up: bool) -> Result<bool, EditError> {
        if self.selection.is_some()
            && let Some(changed) = self.move_selected_blocks(up)
        {
            return Ok(changed);
        }
        let Some(new_path) = tree_edit::move_block(&mut self.tdoc, &self.cursor.path, up) else {
            return Ok(false);
        };
        self.cursor = DocumentPosition::at(new_path, self.cursor.offset);
        self.selection = None;
        self.normalize_cursor();
        self.trigger_paragraph_change();
        Ok(true)
    }

    /// Move a selection that spans several sibling blocks together, keeping the run selected.
    /// Returns `Some(changed)` when it handled a multi-block selection, or `None` to fall back
    /// to a single-block move (the selection sits within one block, or spans blocks that aren't
    /// siblings of one container).
    fn move_selected_blocks(&mut self, up: bool) -> Option<bool> {
        let (start, end) = self.selection_or_cursor_range();
        let (container, first, last) = common_child_run(&start.path, &end.path)?;
        if first == last {
            return None; // a single block: let the single-block path handle it
        }
        let (new_container, nf, nl) =
            tree_edit::move_block_range(&mut self.tdoc, &container, first, last, up)?;
        self.reselect_child_run(&new_container, nf, nl);
        self.trigger_paragraph_change();
        Some(true)
    }

    /// Select the run of children `[first, last]` of the container at `container_path`: the
    /// selection spans from the first child's first leaf to the last child's last leaf, with the
    /// cursor at the end. Used to keep a moved block group selected.
    fn reselect_child_run(&mut self, container_path: &TreePath, first: usize, last: usize) {
        let level = container_path.len();
        let mut min: Option<TreePath> = None;
        let mut max: Option<TreePath> = None;
        for p in self.leaf_paths() {
            let segs = p.segments();
            if segs.len() <= level || &segs[..level] != container_path.segments() {
                continue;
            }
            let ci = child_index(&segs[level]);
            if ci < first || ci > last {
                continue;
            }
            if min.is_none() {
                min = Some(p.clone());
            }
            max = Some(p);
        }
        if let (Some(mn), Some(mx)) = (min, max) {
            let end_len = self.leaf_text_len(&mx);
            self.selection = Some((
                DocumentPosition::at(mn, 0),
                DocumentPosition::at(mx.clone(), end_len),
            ));
            self.cursor = DocumentPosition::at(mx, end_len);
            self.normalize_cursor();
        }
    }

    /// Nest the current list item — or every list item in the selection — beneath its
    /// previous sibling (Tab).
    pub fn indent_list_item(&mut self) -> EditResult {
        // Top-down: each selected item nests under the same previous sibling, so they stay
        // side by side one level deeper rather than stacking into a staircase. The first
        // item of a list that follows another list merges into that preceding list.
        self.shift_list_items(tree_edit::indent_list_item_or_merge, false)
    }

    /// Move the current list item — or every list item in the selection — out one nesting
    /// level, or out of the list entirely (Shift-Tab). A list item directly inside a quote is
    /// lifted out of the quote *keeping its bullet* (the inverse of Tab nesting it in).
    pub fn outdent_list_item(&mut self) -> EditResult {
        // Bottom-up: each item's selected followers are lifted out before the earlier items
        // are processed, so they land side by side rather than being re-adopted as children.
        self.shift_list_items(tree_edit::outdent_list_item, true)
    }

    /// Like [`Self::outdent_list_item`], but delisting: a list item inside a quote drops into
    /// the quote as a plain paragraph rather than being lifted out as a list. Used where the
    /// intent is to stop being a list item — Enter on an empty item, and toggling a list off.
    fn outdent_list_item_delisting(&mut self) -> EditResult {
        self.shift_list_items(tree_edit::outdent_list_item_delisting, true)
    }

    /// Apply a single-item list move (`op`, e.g. indent/outdent) to every list item in the
    /// selection (or just the cursor's item). Items are addressed by their flat leaf index,
    /// which is stable across these moves (they re-nest without changing document order), so
    /// each item is re-resolved as the tree mutates and the selection is restored exactly.
    fn shift_list_items(
        &mut self,
        op: impl Fn(&mut Document, &TreePath) -> Option<TreePath>,
        bottom_up: bool,
    ) -> EditResult {
        let (start, end) = self.selection_or_cursor_range();
        let (Some(start_idx), Some(end_idx)) =
            (self.leaf_index(&start.path), self.leaf_index(&end.path))
        else {
            return Ok(());
        };
        let had_selection = self.selection.is_some() && end_idx > start_idx;
        let cursor_offset = self.cursor.offset;

        let mut order: Vec<usize> = (start_idx..=end_idx).collect();
        if bottom_up {
            order.reverse();
        }
        let mut changed = false;
        for idx in order {
            let leaves = self.leaf_paths();
            let Some(path) = leaves.get(idx).cloned() else {
                continue;
            };
            if matches!(
                path.last(),
                Some(
                    PathSegment::ListEntry { .. }
                        | PathSegment::ChecklistItem(_)
                        | PathSegment::QuoteChild(_)
                )
            ) && op(&mut self.tdoc, &path).is_some()
            {
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }

        // The moved items keep their flat indices, so the selection (or cursor) maps back
        // directly to the same range.
        let leaves = self.leaf_paths();
        if had_selection {
            let start_path = leaves.get(start_idx).cloned().unwrap_or_default();
            let end_path = leaves.get(end_idx).cloned().unwrap_or_default();
            let end_len = self.leaf_text_len(&end_path);
            self.selection = Some((
                DocumentPosition::at(start_path, 0),
                DocumentPosition::at(end_path.clone(), end_len),
            ));
            self.cursor = DocumentPosition::at(end_path, end_len);
        } else {
            let path = leaves.get(start_idx).cloned().unwrap_or_default();
            self.cursor = DocumentPosition::at(path, cursor_offset);
            self.selection = None;
        }
        self.normalize_cursor();
        self.trigger_paragraph_change();
        Ok(())
    }

    /// Whether the cursor's leaf is a list/checklist item (for routing Tab/Backspace).
    fn cursor_is_list_item(&self) -> bool {
        matches!(
            self.cursor.path.last(),
            Some(PathSegment::ListEntry { .. } | PathSegment::ChecklistItem(_))
        )
    }

    /// Whether the cursor's list/checklist item is nested inside a parent list item (i.e.
    /// its containing list sits within an outer list entry / checklist item), as opposed to
    /// a top-level item of a list in a document or quote.
    fn cursor_is_nested_list_item(&self) -> bool {
        let segs = self.cursor.path.segments();
        segs.len() >= 2
            && matches!(
                segs[segs.len() - 2],
                PathSegment::ListEntry { .. } | PathSegment::ChecklistItem(_)
            )
    }

    /// The list/checklist nesting depth of the cursor's leaf (0 = top-level item).
    fn cursor_list_depth(&self) -> usize {
        self.leaves()
            .iter()
            .find(|l| l.path == self.cursor.path)
            .map(|l| l.depth)
            .unwrap_or(0)
    }

    /// Insert a document fragment (e.g. from the clipboard) at the cursor. Splits the
    /// current top-level paragraph and splices the fragment's paragraphs between the
    /// halves. TODO(phase3): splice within lists/quotes (currently only at top level).
    pub fn insert_document(&mut self, document: &Document) -> EditResult {
        if document.paragraphs.is_empty() {
            return Ok(());
        }
        if self.selection.is_some() {
            self.delete_selection()?;
        }
        if self.leaf_count() == 0 {
            for p in &document.paragraphs {
                self.tdoc.add_paragraph(p.clone());
            }
            let last = self.tdoc.paragraphs.len().saturating_sub(1);
            let path = TreePath::root(last);
            let len = self.leaf_text_len(&path);
            self.cursor = DocumentPosition::at(path, len);
            self.selection = None;
            self.trigger_paragraph_change();
            return Ok(());
        }
        let Some(i) = self.cursor_top_index() else {
            // Non-top-level cursor (inside a quote child or list item). Full structural
            // splicing into nested containers isn't supported yet, but each fragment
            // paragraph can be inserted run-by-run so its inline styling
            // (bold/italic/links/…) survives. Between paragraphs, break the current leaf
            // into a new sibling (a fresh list item / quote paragraph) so a multi-line
            // paste mirrors the source's block structure instead of collapsing into raw
            // Markdown text.
            for (idx, p) in document.paragraphs.iter().enumerate() {
                if idx > 0 {
                    self.insert_newline()?;
                }
                for run in spans_to_inline(p.content()) {
                    self.insert_inline_at_cursor(run)?;
                }
            }
            self.trigger_paragraph_change();
            return Ok(());
        };

        let offset = self.cursor.offset;
        let path = TreePath::root(i);
        let runs = tree_walk::leaf_inline(&self.tdoc, &path);
        let (left, right) = {
            let mut block = Block::paragraph();
            block.content = runs;
            let r = block.split_content_at(offset);
            (block.content, r)
        };
        tree_walk::set_leaf_inline(&mut self.tdoc, &path, &left);

        let mut frag = document.paragraphs.clone();

        // If the current block and the first fragment paragraph are both plain text, merge
        // the first fragment into the current block (so "A|B" + paste "X\nY" → "AX", "YB").
        if matches!(self.tdoc.paragraphs.get(i), Some(Paragraph::Text { .. }))
            && matches!(frag.first(), Some(Paragraph::Text { .. }))
        {
            let first = frag.remove(0);
            let mut merged = left.clone();
            merged.extend(spans_to_inline(first.content()));
            tree_walk::set_leaf_inline(&mut self.tdoc, &path, &merged);
        }

        // Insert the remaining fragment paragraphs after the current block.
        let mut last_idx = i;
        for (k, p) in frag.into_iter().enumerate() {
            self.tdoc.paragraphs.insert(i + 1 + k, p);
            last_idx = i + 1 + k;
        }

        // The cursor lands at the join point: the end of the last block's own content,
        // before the right-hand remainder is appended.
        let last_path = TreePath::root(last_idx);
        let cursor_off = self.leaf_text_len(&last_path);

        // Append the right-hand remainder to the last block (or as a new paragraph if that
        // block can't hold inline content directly).
        if right.iter().map(|c| c.text_len()).sum::<usize>() > 0 {
            if tree_walk::leaf_spans(&self.tdoc, &last_path).is_some()
                && !matches!(
                    self.tdoc.paragraphs.get(last_idx),
                    Some(Paragraph::Table { .. })
                )
            {
                let mut content = tree_walk::leaf_inline(&self.tdoc, &last_path);
                content.extend(right);
                let mut block = Block::paragraph();
                block.content = content;
                block.normalize_content();
                tree_walk::set_leaf_inline(&mut self.tdoc, &last_path, &block.content);
            } else {
                self.tdoc.paragraphs.insert(
                    last_idx + 1,
                    Paragraph::new_text().with_content(inline_to_spans(&right)),
                );
            }
        }

        self.cursor = DocumentPosition::at(last_path, cursor_off);
        self.selection = None;
        self.normalize_cursor();
        self.trigger_paragraph_change();
        Ok(())
    }
}

/// Build a header paragraph of the given level (1-3) carrying `spans`.
fn make_header(level: u8, spans: Vec<Span>) -> Paragraph {
    match level {
        1 => Paragraph::new_header1(),
        2 => Paragraph::new_header2(),
        _ => Paragraph::new_header3(),
    }
    .with_content(spans)
}

/// The top-level paragraph index a path addresses, if it is a top-level leaf.
fn top_index(path: &TreePath) -> Option<usize> {
    match path.segments() {
        [PathSegment::Paragraph(i)] => Some(*i),
        _ => None,
    }
}

/// The top-level paragraph index a path descends into, regardless of how deep the path goes
/// (into a list item, quote child, …). Unlike [`top_index`], it does not require the path to
/// address a bare top-level paragraph.
fn top_para_index(path: &TreePath) -> Option<usize> {
    match path.segments().first()? {
        PathSegment::Paragraph(i) => Some(*i),
        _ => None,
    }
}

/// The number of items (list entries or checklist items) in a list/checklist node; `0` for any
/// other paragraph.
fn list_item_count(p: &Paragraph) -> usize {
    match p {
        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => entries.len(),
        Paragraph::Checklist { items } => items.len(),
        _ => 0,
    }
}

/// The index a path segment addresses within its container (the entry for a list segment,
/// ignoring which paragraph of the entry it points at).
fn child_index(seg: &PathSegment) -> usize {
    match seg {
        PathSegment::Paragraph(i) => *i,
        PathSegment::QuoteChild(c) => *c,
        PathSegment::ListEntry { entry, .. } => *entry,
        PathSegment::ChecklistItem(c) => *c,
    }
}

/// The common container of two leaf paths and the run of its direct children the paths span:
/// the longest shared prefix is the container, and the two paths' next segments give the first
/// and last child indices (`first <= last` since `a <= b`). `None` if one path is a prefix of
/// the other (e.g. the same leaf), which has no sibling run.
fn common_child_run(a: &TreePath, b: &TreePath) -> Option<(TreePath, usize, usize)> {
    let (sa, sb) = (a.segments(), b.segments());
    let mut cp = 0;
    while cp < sa.len() && cp < sb.len() && sa[cp] == sb[cp] {
        cp += 1;
    }
    if cp >= sa.len() || cp >= sb.len() {
        return None;
    }
    let container = TreePath(sa[..cp].to_vec());
    let first = child_index(&sa[cp]);
    let last = child_index(&sb[cp]);
    if first > last {
        return None;
    }
    Some((container, first, last))
}

/// The index a path addresses within its parent container, when that parent is a paragraph
/// vec whose children can be nested into an adjacent sibling — the document top level
/// (`Paragraph`) or a quote's children (`QuoteChild`). `None` for list/checklist items.
fn sibling_index(path: &TreePath) -> Option<usize> {
    match path.last()? {
        PathSegment::Paragraph(i) | PathSegment::QuoteChild(i) => Some(*i),
        _ => None,
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

/// Insert `text` into a flat inline-content vector at byte `offset`, preserving the style
/// of the run the cursor sits in and keeping insertions at link edges outside the link.
fn insert_into_content(
    content: &mut Vec<InlineContent>,
    offset: usize,
    text: &str,
    bias_right: bool,
) {
    let (idx, content_offset) = if bias_right {
        find_content_at_offset_right(content, offset)
    } else {
        find_content_at_offset(content, offset)
    };
    if idx >= content.len() {
        content.push(InlineContent::Text(TextRun::plain(text)));
        return;
    }
    match &mut content[idx] {
        InlineContent::Text(run) => run.insert_text(content_offset, text),
        InlineContent::Link { content: inner, .. } => {
            let link_len: usize = inner.iter().map(|c| c.text_len()).sum();
            if content_offset == 0 {
                if idx > 0
                    && let InlineContent::Text(prev) = &mut content[idx - 1]
                {
                    let prev_len = prev.len();
                    prev.insert_text(prev_len, text);
                } else {
                    content.insert(idx, InlineContent::Text(TextRun::plain(text)));
                }
            } else if content_offset >= link_len {
                if idx + 1 < content.len()
                    && let InlineContent::Text(next) = &mut content[idx + 1]
                {
                    next.insert_text(0, text);
                } else {
                    content.insert(idx + 1, InlineContent::Text(TextRun::plain(text)));
                }
            } else {
                let (inner_idx, inner_off) = find_content_at_offset(inner, content_offset);
                if inner_idx >= inner.len() {
                    inner.push(InlineContent::Text(TextRun::plain(text)));
                } else if let InlineContent::Text(run) = &mut inner[inner_idx] {
                    run.insert_text(inner_off, text);
                } else {
                    inner.insert(inner_idx, InlineContent::Text(TextRun::plain(text)));
                }
            }
        }
        InlineContent::HardBreak => {
            if content_offset == 0 {
                content.insert(idx, InlineContent::Text(TextRun::plain(text)));
            } else if idx + 1 < content.len()
                && let InlineContent::Text(run) = &mut content[idx + 1]
            {
                run.insert_text(0, text);
            } else {
                content.insert(idx + 1, InlineContent::Text(TextRun::plain(text)));
            }
        }
    }
}

/// Find the inline element index and the offset within it for a flattened byte offset.
/// Left-biased: at an exact run boundary this targets the run *ending* at `offset`.
fn find_content_at_offset(content: &[InlineContent], offset: usize) -> (usize, usize) {
    let mut current = 0;
    for (idx, item) in content.iter().enumerate() {
        let len = item.text_len();
        if current + len >= offset {
            return (idx, offset - current);
        }
        current += len;
    }
    (content.len(), 0)
}

/// Right-biased variant of [`find_content_at_offset`]: at an exact run boundary it
/// targets the run *beginning* at `offset` rather than the one ending there, so text
/// inserted with `Right` affinity inherits the following run's style. Past the last run
/// (a trailing style edge) it falls through to a fresh plain run at the end.
fn find_content_at_offset_right(content: &[InlineContent], offset: usize) -> (usize, usize) {
    let mut current = 0;
    for (idx, item) in content.iter().enumerate() {
        let len = item.text_len();
        if current + len > offset {
            return (idx, offset - current);
        }
        current += len;
    }
    (content.len(), 0)
}

/// Split content into (before, within, after) the `[start, end)` byte range, splitting
/// the runs that straddle a boundary.
fn split_content_for_style(
    content: &[InlineContent],
    start_offset: usize,
    end_offset: usize,
) -> (Vec<InlineContent>, Vec<InlineContent>, Vec<InlineContent>) {
    let mut before = Vec::new();
    let mut selected = Vec::new();
    let mut after = Vec::new();
    let mut current_offset = 0;

    for item in content {
        let item_len = item.text_len();
        let item_start = current_offset;
        let item_end = current_offset + item_len;

        if item_end <= start_offset {
            before.push(item.clone());
        } else if item_start >= end_offset {
            after.push(item.clone());
        } else if item_start >= start_offset && item_end <= end_offset {
            selected.push(item.clone());
        } else {
            match item {
                InlineContent::Text(run) => {
                    let text = &run.text;
                    let sel_start_in_run = start_offset.saturating_sub(item_start);
                    let sel_end_in_run = end_offset.saturating_sub(item_start).min(item_len);
                    if sel_start_in_run > 0 {
                        let mut before_run = run.clone();
                        before_run.text = text[..sel_start_in_run].to_string();
                        before.push(InlineContent::Text(before_run));
                    }
                    if sel_end_in_run > sel_start_in_run {
                        let mut selected_run = run.clone();
                        selected_run.text = text[sel_start_in_run..sel_end_in_run].to_string();
                        selected.push(InlineContent::Text(selected_run));
                    }
                    if sel_end_in_run < item_len {
                        let mut after_run = run.clone();
                        after_run.text = text[sel_end_in_run..].to_string();
                        after.push(InlineContent::Text(after_run));
                    }
                }
                InlineContent::Link {
                    link,
                    content: inner,
                } => {
                    // Recurse so a selection that lands *inside* a link styles the
                    // link's own runs (rather than the whole link being dropped
                    // wholesale into one region). Each non-empty region keeps the
                    // link wrapper with the matching slice of its content.
                    let sel_start_in_run = start_offset.saturating_sub(item_start);
                    let sel_end_in_run = end_offset.saturating_sub(item_start).min(item_len);
                    let (b, s, a) =
                        split_content_for_style(inner, sel_start_in_run, sel_end_in_run);
                    if !b.is_empty() {
                        before.push(InlineContent::Link {
                            link: link.clone(),
                            content: b,
                        });
                    }
                    if !s.is_empty() {
                        selected.push(InlineContent::Link {
                            link: link.clone(),
                            content: s,
                        });
                    }
                    if !a.is_empty() {
                        after.push(InlineContent::Link {
                            link: link.clone(),
                            content: a,
                        });
                    }
                }
                InlineContent::HardBreak => {
                    if item_start < start_offset {
                        before.push(item.clone());
                    } else if item_start < end_offset {
                        selected.push(item.clone());
                    } else {
                        after.push(item.clone());
                    }
                }
            }
        }
        current_offset += item_len;
    }

    (before, selected, after)
}

/// Recursively apply a style mutation to every text run (descending into links).
fn map_style_on_runs<F>(items: Vec<InlineContent>, apply: &mut F) -> Vec<InlineContent>
where
    F: FnMut(&mut TextStyle),
{
    items
        .into_iter()
        .map(|item| match item {
            InlineContent::Text(mut run) => {
                apply(&mut run.style);
                InlineContent::Text(run)
            }
            InlineContent::Link { link, content } => InlineContent::Link {
                link,
                content: map_style_on_runs(content, apply),
            },
            other => other,
        })
        .collect()
}

/// Extract the inline runs covering `[start, end)` (flattened byte offsets).
fn extract_inline_range(content: &[InlineContent], start: usize, end: usize) -> Vec<InlineContent> {
    let mut head = Block::paragraph();
    head.content = content.to_vec();
    let tail = head.split_content_at(start);
    let mut result = Block::paragraph();
    result.content = tail;
    let _ = result.split_content_at(end.saturating_sub(start));
    result.content
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown_converter::{document_to_markdown, markdown_to_document};

    #[test]
    fn insert_text_into_empty_document() {
        let mut editor = Editor::new();
        editor.insert_text("Hello").unwrap();
        assert_eq!(editor.leaf_plain_text(&TreePath::root(0)), "Hello");
        assert_eq!(editor.cursor().offset, 5);
    }

    #[test]
    fn typing_continues_run_style() {
        // Loading bold markdown then typing inside keeps it one styled leaf.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("**bold**"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 2));
        editor.insert_text("X").unwrap();
        assert_eq!(editor.leaf_plain_text(&TreePath::root(0)), "boXld");
    }

    #[test]
    fn affinity_pauses_at_style_boundary_moving_right() {
        // "Hello " (plain) + "World!" (bold); the style boundary is at byte offset 6.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("Hello **World!**"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 5));
        assert!(!editor.cursor_at_style_boundary());

        // Crossing into the boundary lands on its Left (earlier) stop.
        editor.move_cursor_right();
        assert_eq!(editor.cursor().offset, 6);
        assert!(editor.cursor_at_style_boundary());
        assert_eq!(editor.cursor_affinity(), Affinity::Left);

        // Same offset — flip to the Right (later) stop.
        editor.move_cursor_right();
        assert_eq!(editor.cursor().offset, 6);
        assert_eq!(editor.cursor_affinity(), Affinity::Right);

        // Only now does the caret cross the grapheme.
        editor.move_cursor_right();
        assert_eq!(editor.cursor().offset, 7);
        assert_eq!(editor.cursor_affinity(), Affinity::Left);
    }

    #[test]
    fn affinity_pauses_at_style_boundary_moving_left() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("Hello **World!**"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 7));

        // Crossing left into the boundary lands on its Right (later) stop.
        editor.move_cursor_left();
        assert_eq!(editor.cursor().offset, 6);
        assert_eq!(editor.cursor_affinity(), Affinity::Right);

        // Flip to the Left stop in place.
        editor.move_cursor_left();
        assert_eq!(editor.cursor().offset, 6);
        assert_eq!(editor.cursor_affinity(), Affinity::Left);

        // Then cross the grapheme.
        editor.move_cursor_left();
        assert_eq!(editor.cursor().offset, 5);
        assert!(!editor.cursor_at_style_boundary());
    }

    #[test]
    fn plain_text_has_no_extra_affinity_stops() {
        // Without a style boundary, Left/Right step a grapheme each — no pause.
        let mut editor = Editor::new();
        editor.insert_text("abc").unwrap();
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 1));
        editor.move_cursor_right();
        assert_eq!(editor.cursor().offset, 2);
        assert!(!editor.cursor_at_style_boundary());
    }

    #[test]
    fn insert_at_boundary_respects_affinity() {
        // Left affinity keeps typed text outside the bold run.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("Hello **World!**"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 6));
        assert_eq!(editor.cursor_affinity(), Affinity::Left);
        editor.insert_text("X").unwrap();
        assert_eq!(md(&editor), "Hello X**World!**");

        // Right affinity makes the same keystroke join the bold run.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("Hello **World!**"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 6));
        editor.move_cursor_right(); // flip to Right affinity at the boundary
        assert_eq!(editor.cursor_affinity(), Affinity::Right);
        editor.insert_text("X").unwrap();
        assert_eq!(md(&editor), "Hello **XWorld!**");
    }

    #[test]
    fn cursor_inline_labels_follow_affinity() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("Hello **World!**"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 6));
        // Left stop reads the plain run to the left.
        assert!(editor.cursor_inline_labels().is_empty());
        // Right stop reads the bold run to the right.
        editor.move_cursor_right();
        assert_eq!(editor.cursor_inline_labels(), vec!["Bold"]);
    }

    #[test]
    fn disabling_style_boundary_stops_restores_single_caret() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("Hello **World!**"));
        editor.set_style_boundary_stops(false);
        assert!(!editor.style_boundary_stops());

        // No extra stop at the boundary: 5 -> 6 -> 7 with single presses.
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 5));
        editor.move_cursor_right();
        assert_eq!(editor.cursor().offset, 6);
        assert_eq!(editor.cursor_affinity(), Affinity::Left);
        editor.move_cursor_right();
        assert_eq!(editor.cursor().offset, 7);

        // Insertion at the boundary stays left-biased (outside the bold run).
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 6));
        editor.insert_text("X").unwrap();
        assert_eq!(md(&editor), "Hello X**World!**");
    }

    #[test]
    fn unsupported_backend_collapses_affinity() {
        // A cell backend reports no lean support: affinity must be inert even
        // though `style_boundary_stops` is on by default — the guarantee a cell
        // renderer relies on. Mirrors disabling the toggle, but via the backend.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("Hello **World!**"));
        assert!(editor.style_boundary_stops());

        // Establish Right affinity first, then pull backend support: it resets.
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 6));
        editor.move_cursor_right();
        assert_eq!(editor.cursor_affinity(), Affinity::Right);
        editor.set_affinity_supported(false);
        assert!(!editor.affinity_active());
        assert_eq!(editor.cursor_affinity(), Affinity::Left);

        // No extra stop at the boundary: 5 -> 6 -> 7 with single presses.
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 5));
        editor.move_cursor_right();
        assert_eq!(editor.cursor().offset, 6);
        editor.move_cursor_right();
        assert_eq!(editor.cursor().offset, 7);

        // Insertion at the boundary stays left-biased (outside the bold run).
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 6));
        editor.insert_text("X").unwrap();
        assert_eq!(md(&editor), "Hello X**World!**");

        // Restoring support brings the extra stop back. Reset the document so the
        // boundary is at offset 6 again (the insert above shifted it).
        editor.set_affinity_supported(true);
        editor.set_document(markdown_to_document("Hello **World!**"));
        assert!(editor.affinity_active());
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 6));
        editor.move_cursor_right(); // flips to Right affinity in place
        assert_eq!(editor.cursor().offset, 6);
        assert_eq!(editor.cursor_affinity(), Affinity::Right);
    }

    #[test]
    fn intra_leaf_delete_backward() {
        let mut editor = Editor::new();
        editor.insert_text("Hello").unwrap();
        editor.delete_backward().unwrap();
        assert_eq!(editor.leaf_plain_text(&TreePath::root(0)), "Hell");
        assert_eq!(editor.cursor().offset, 4);
    }

    #[test]
    fn word_navigation_within_leaf() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("alpha beta gamma"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 0));
        editor.move_word_right();
        assert_eq!(editor.cursor().offset, 5); // end of "alpha" (word-right stops after the word)
    }

    #[test]
    fn undo_redo_round_trips_typing() {
        let mut editor = Editor::new();
        editor.insert_text("Hello").unwrap();
        editor.commit_undo_step(UndoKind::Typing, Instant::now());
        assert!(editor.undo());
        assert_eq!(editor.leaf_count(), 0);
        assert!(editor.redo());
        assert_eq!(editor.leaf_plain_text(&TreePath::root(0)), "Hello");
    }

    #[test]
    fn top_level_newline_splits_paragraph() {
        let mut editor = Editor::new();
        editor.insert_text("HelloWorld").unwrap();
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 5));
        editor.insert_newline().unwrap();
        assert_eq!(editor.leaf_count(), 2);
        assert_eq!(editor.leaf_plain_text(&TreePath::root(0)), "Hello");
        assert_eq!(editor.leaf_plain_text(&TreePath::root(1)), "World");
    }

    fn md(editor: &Editor) -> String {
        document_to_markdown(editor.document()).trim().to_string()
    }

    // ----- Block model: pseudo-leaf convert / wrap / unwrap -------------------------

    fn cursor_at(editor: &mut Editor, path: TreePath) {
        editor.set_cursor(DocumentPosition::at(path, 0));
    }

    #[test]
    fn convert_heading_to_quote_flattens_not_nests() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("## Title"));
        cursor_at(&mut editor, TreePath::root(0));
        editor.set_block_type(BlockType::BlockQuote).unwrap();
        // A plain quote, not a quoted heading.
        assert_eq!(md(&editor), "> Title");
        if let Paragraph::Quote { children } = &editor.document().paragraphs[0] {
            assert!(matches!(children[0], Paragraph::Text { .. }));
        } else {
            panic!("expected a quote");
        }
    }

    #[test]
    fn single_text_quote_pseudo_leaf_to_text_unwraps() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("> quoted"));
        cursor_at(
            &mut editor,
            TreePath::root(0).child(PathSegment::QuoteChild(0)),
        );
        editor.set_block_type(BlockType::Paragraph).unwrap();
        assert_eq!(md(&editor), "quoted");
        assert!(matches!(
            editor.document().paragraphs[0],
            Paragraph::Text { .. }
        ));
    }

    #[test]
    fn single_text_quote_pseudo_leaf_to_heading_lifts_out() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("> quoted"));
        cursor_at(
            &mut editor,
            TreePath::root(0).child(PathSegment::QuoteChild(0)),
        );
        editor
            .set_block_type(BlockType::Heading { level: 2 })
            .unwrap();
        assert_eq!(md(&editor), "## quoted");
    }

    #[test]
    fn single_text_quote_pseudo_leaf_to_bullet_converts_container() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("> quoted"));
        cursor_at(
            &mut editor,
            TreePath::root(0).child(PathSegment::QuoteChild(0)),
        );
        editor
            .set_block_type(BlockType::ListItem {
                ordered: false,
                number: None,
                checkbox: None,
                depth: 0,
            })
            .unwrap();
        assert!(matches!(
            editor.document().paragraphs[0],
            Paragraph::UnorderedList { .. }
        ));
        assert_eq!(md(&editor), "- quoted");
    }

    #[test]
    fn paragraph_to_quote_and_back_round_trips_heading() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("## Title"));
        cursor_at(&mut editor, TreePath::root(0));
        editor.set_block_type(BlockType::BlockQuote).unwrap();
        editor
            .set_block_type(BlockType::Heading { level: 2 })
            .unwrap();
        assert_eq!(md(&editor), "## Title");
    }

    #[test]
    fn single_text_bullet_in_multi_item_list_is_pseudo_leaf() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b\n- c"));
        cursor_at(
            &mut editor,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 }),
        );
        assert!(matches!(
            editor.cursor_effective_block_type(),
            BlockType::ListItem {
                ordered: false,
                checkbox: None,
                ..
            }
        ));
        // ESC-7 converts the whole containing list to numbered.
        editor
            .set_block_type(BlockType::ListItem {
                ordered: true,
                number: None,
                checkbox: None,
                depth: 0,
            })
            .unwrap();
        if let Paragraph::OrderedList { entries } = &editor.document().paragraphs[0] {
            assert_eq!(entries.len(), 3);
        } else {
            panic!("expected an ordered list");
        }
    }

    #[test]
    fn wrap_selection_quote_preserves_heading_and_round_trips() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("## Title"));
        cursor_at(&mut editor, TreePath::root(0));
        editor
            .wrap_selection(tree_edit::ContainerKind::Quote)
            .unwrap();
        assert_eq!(md(&editor), "> ## Title");
        if let Paragraph::Quote { children } = &editor.document().paragraphs[0] {
            assert!(matches!(children[0], Paragraph::Header2 { .. }));
        } else {
            panic!("expected a quote");
        }
    }

    #[test]
    fn wrap_selection_bullet_holds_whole_selection_as_one_item() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("first\n\nsecond"));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(0), 0),
            DocumentPosition::at(TreePath::root(1), 6),
        );
        editor
            .wrap_selection(tree_edit::ContainerKind::Unordered)
            .unwrap();
        if let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] {
            assert_eq!(entries.len(), 1, "one list item");
            assert_eq!(entries[0].len(), 2, "holding both paragraphs");
        } else {
            panic!("expected a list");
        }
    }

    #[test]
    fn outdent_lifts_quote_child_out() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("> quoted"));
        cursor_at(
            &mut editor,
            TreePath::root(0).child(PathSegment::QuoteChild(0)),
        );
        editor.outdent_list_item().unwrap();
        assert_eq!(md(&editor), "quoted");
        assert!(matches!(
            editor.document().paragraphs[0],
            Paragraph::Text { .. }
        ));
    }

    #[test]
    fn indent_first_ordered_item_merges_into_preceding_bullet_sublist() {
        // A bullet item with a nested bullet sublist, then a separate top-level ordered
        // list; indenting the ordered item joins the nested bullet sublist.
        let mut editor = Editor::new();
        let mut doc = markdown_to_document("x");
        let text = |s: &str| Paragraph::Text {
            content: vec![Span::new_text(s)],
        };
        doc.paragraphs = vec![
            Paragraph::new_unordered_list().with_entries(vec![vec![
                text("bullet item"),
                Paragraph::new_unordered_list()
                    .with_entries(vec![vec![text("a")], vec![text("b")]]),
            ]]),
            Paragraph::new_ordered_list().with_entries(vec![vec![text("ordered item")]]),
        ];
        editor.set_document(doc);
        // Cursor on "ordered item" (the ordered list's first/only item).
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(1).child(PathSegment::ListEntry { entry: 0, para: 0 }),
            0,
        ));
        editor.indent_list_item().unwrap();
        // The whole document collapses to one bullet list; the ordered item joined the
        // nested bullet sublist as a third item.
        assert_eq!(
            editor.document().paragraphs.len(),
            1,
            "the ordered list is gone"
        );
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected the outer bullet list");
        };
        assert_eq!(entries.len(), 1);
        let Paragraph::UnorderedList { entries: sub } = &entries[0][1] else {
            panic!("expected the nested bullet sublist");
        };
        assert_eq!(sub.len(), 3, "ordered item joined the sublist");
        assert_eq!(
            editor.leaf_plain_text(
                &TreePath::root(0)
                    .child(PathSegment::ListEntry { entry: 0, para: 1 })
                    .child(PathSegment::ListEntry { entry: 2, para: 0 })
            ),
            "ordered item"
        );
    }

    #[test]
    fn convert_list_item_to_quote_leaves_siblings_alone() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b\n- c"));
        cursor_at(
            &mut editor,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 }),
        );
        editor.set_block_type(BlockType::BlockQuote).unwrap();
        // "b" becomes a quote; "a" and "c" stay list items (the list splits around it).
        let paras = &editor.document().paragraphs;
        assert_eq!(paras.len(), 3);
        assert!(matches!(paras[0], Paragraph::UnorderedList { .. }));
        assert!(matches!(paras[1], Paragraph::Quote { .. }));
        assert!(matches!(paras[2], Paragraph::UnorderedList { .. }));
        if let Paragraph::Quote { children } = &paras[1] {
            assert!(matches!(children[0], Paragraph::Text { .. }));
        } else {
            panic!("expected the middle item to become a quote");
        }
    }

    fn bt_ordered() -> BlockType {
        BlockType::ListItem {
            ordered: true,
            number: None,
            checkbox: None,
            depth: 0,
        }
    }
    fn bt_checklist() -> BlockType {
        BlockType::ListItem {
            ordered: false,
            number: None,
            checkbox: Some(false),
            depth: 0,
        }
    }

    #[test]
    fn convert_selected_checklist_items_carves_them_out() {
        // A four-item checklist; selecting the middle two and converting to a numbered list
        // carves them out into their own ordered list, splitting the checklist into three.
        let mut editor = Editor::new();
        let mut doc = markdown_to_document("x");
        let check = |s: &str| ChecklistItem::new(false).with_content(vec![Span::new_text(s)]);
        doc.paragraphs = vec![Paragraph::new_checklist().with_checklist_items(vec![
            check("c1"),
            check("c2"),
            check("c3"),
            check("c4"),
        ])];
        editor.set_document(doc);
        let item = |c| TreePath::root(0).child(PathSegment::ChecklistItem(c));
        editor.set_cursor(DocumentPosition::at(item(2), 2));
        editor.set_selection(
            DocumentPosition::at(item(1), 0),
            DocumentPosition::at(item(2), 2),
        );
        editor.set_block_type(bt_ordered()).unwrap();
        let paras = &editor.document().paragraphs;
        assert_eq!(paras.len(), 3, "checklist split into three siblings");
        let Paragraph::Checklist { items } = &paras[0] else {
            panic!("first stays a checklist");
        };
        assert_eq!(items.len(), 1, "c1");
        let Paragraph::OrderedList { entries } = &paras[1] else {
            panic!("middle became an ordered list");
        };
        assert_eq!(entries.len(), 2, "c2, c3 carved out");
        let Paragraph::Checklist { items } = &paras[2] else {
            panic!("last stays a checklist");
        };
        assert_eq!(items.len(), 1, "c4");
        // The selection still covers the two carved-out items.
        let (a, b) = editor.selection().expect("selection preserved");
        assert_eq!(editor.leaf_plain_text(&a.path), "c2");
        assert_eq!(editor.leaf_plain_text(&b.path), "c3");
    }

    #[test]
    fn convert_leading_selected_items_splits_into_two() {
        // Selecting from the first item leaves no "before" half — only two lists result.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b\n- c\n- d"));
        let item = |e| TreePath::root(0).child(PathSegment::ListEntry { entry: e, para: 0 });
        editor.set_cursor(DocumentPosition::at(item(1), 1));
        editor.set_selection(
            DocumentPosition::at(item(0), 0),
            DocumentPosition::at(item(1), 1),
        );
        editor.set_block_type(bt_checklist()).unwrap();
        let paras = &editor.document().paragraphs;
        assert_eq!(paras.len(), 2, "no before-half, so two lists");
        let Paragraph::Checklist { items } = &paras[0] else {
            panic!("leading items became a checklist");
        };
        assert_eq!(items.len(), 2, "a, b");
        let Paragraph::UnorderedList { entries } = &paras[1] else {
            panic!("trailing items stay a bullet list");
        };
        assert_eq!(entries.len(), 2, "c, d");
    }

    #[test]
    fn convert_whole_selection_converts_the_whole_list() {
        // Selecting *every* item is a plain whole-list conversion, not a split.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b\n- c"));
        let item = |e| TreePath::root(0).child(PathSegment::ListEntry { entry: e, para: 0 });
        editor.set_cursor(DocumentPosition::at(item(2), 1));
        editor.set_selection(
            DocumentPosition::at(item(0), 0),
            DocumentPosition::at(item(2), 1),
        );
        editor.set_block_type(bt_ordered()).unwrap();
        let paras = &editor.document().paragraphs;
        assert_eq!(paras.len(), 1, "still one list");
        assert!(matches!(paras[0], Paragraph::OrderedList { .. }));
    }

    #[test]
    fn convert_carved_out_items_merge_with_adjacent_same_kind_list() {
        // A numbered list, then a bullet list; carving out the bullet's leading items to
        // numbered lets them merge into the preceding numbered list.
        let mut editor = Editor::new();
        let mut doc = markdown_to_document("x");
        let entry = |s: &str| {
            vec![Paragraph::Text {
                content: vec![Span::new_text(s)],
            }]
        };
        doc.paragraphs = vec![
            Paragraph::new_ordered_list().with_entries(vec![entry("n1")]),
            Paragraph::new_unordered_list().with_entries(vec![
                entry("b1"),
                entry("b2"),
                entry("b3"),
            ]),
        ];
        editor.set_document(doc);
        let bullet = |e| TreePath::root(1).child(PathSegment::ListEntry { entry: e, para: 0 });
        editor.set_cursor(DocumentPosition::at(bullet(1), 1));
        editor.set_selection(
            DocumentPosition::at(bullet(0), 0),
            DocumentPosition::at(bullet(1), 1),
        );
        editor.set_block_type(bt_ordered()).unwrap();
        let paras = &editor.document().paragraphs;
        assert_eq!(
            paras.len(),
            2,
            "merged numbered list + remaining bullet list"
        );
        let Paragraph::OrderedList { entries } = &paras[0] else {
            panic!("expected the merged numbered list");
        };
        assert_eq!(entries.len(), 3, "n1, b1, b2 merged");
        let Paragraph::UnorderedList { entries } = &paras[1] else {
            panic!("expected the remaining bullet list");
        };
        assert_eq!(entries.len(), 1, "b3");
    }

    #[test]
    fn convert_nested_item_to_text_stays_inside_parent_item() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document(
            "- first\n- second\n- third\n    - indented",
        ));
        // "indented" is nested under "third".
        let path = TreePath::root(0)
            .child(PathSegment::ListEntry { entry: 2, para: 1 })
            .child(PathSegment::ListEntry { entry: 0, para: 0 });
        editor.set_cursor(DocumentPosition::at(path, 0));
        editor.set_block_type(BlockType::Paragraph).unwrap();
        // "indented" becomes the second paragraph *inside* third's list item (a
        // continuation of "third"), not a top-level paragraph or an outdented bullet.
        let paras = &editor.document().paragraphs;
        assert_eq!(paras.len(), 1, "still a single top-level list");
        let Paragraph::UnorderedList { entries } = &paras[0] else {
            panic!("expected the list to remain");
        };
        assert_eq!(entries.len(), 3, "first/second/third");
        assert_eq!(entries[2].len(), 2, "third's item now holds two paragraphs");
        assert!(matches!(entries[2][0], Paragraph::Text { .. }));
        assert!(matches!(entries[2][1], Paragraph::Text { .. }));
        // The pseudo-leaf breadcrumb is now "Unordered List > Text".
        let leaf = TreePath::root(0).child(PathSegment::ListEntry { entry: 2, para: 1 });
        editor.set_cursor(DocumentPosition::at(leaf, 0));
        assert!(matches!(
            editor.cursor_effective_block_type(),
            BlockType::Paragraph
        ));
    }

    #[test]
    fn indent_appends_paragraph_after_list_as_new_item() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b\n\nfollow"));
        // "follow" is a top-level paragraph after the list.
        editor.set_cursor(DocumentPosition::at(TreePath::root(1), 0));
        assert!(
            editor.cursor_can_indent(),
            "a paragraph after a list can indent"
        );
        editor.indent().unwrap();
        // It joins the list as a new sibling item.
        assert_eq!(
            editor.document().paragraphs.len(),
            1,
            "merged into the list"
        );
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected a list");
        };
        assert_eq!(entries.len(), 3, "a new list item was appended");
        let follow = TreePath::root(0).child(PathSegment::ListEntry { entry: 2, para: 0 });
        assert_eq!(editor.leaf_plain_text(&follow), "follow");
        // It round-trips back out via outdent (`[`).
        editor.set_cursor(DocumentPosition::at(follow, 0));
        editor.outdent_list_item().unwrap();
        assert_eq!(
            editor.document().paragraphs.len(),
            2,
            "lifted back out to a paragraph"
        );
    }

    #[test]
    fn indent_paragraph_between_two_lists_merges_all_into_one() {
        let mut editor = Editor::new();
        let mut doc = markdown_to_document("x");
        let item = |s: &str| {
            vec![Paragraph::Text {
                content: vec![Span::new_text(s)],
            }]
        };
        doc.paragraphs = vec![
            Paragraph::new_unordered_list().with_entries(vec![item("a")]),
            Paragraph::Text {
                content: vec![Span::new_text("mid")],
            },
            Paragraph::new_unordered_list().with_entries(vec![item("b")]),
        ];
        editor.set_document(doc);
        editor.set_cursor(DocumentPosition::at(TreePath::root(1), 0)); // "mid"
        editor.indent().unwrap();
        // The paragraph joins the preceding list, which then absorbs the following list.
        assert_eq!(editor.document().paragraphs.len(), 1, "one merged list");
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected a list");
        };
        assert_eq!(entries.len(), 3, "a, mid, b");
    }

    #[test]
    fn indent_selection_of_paragraphs_after_list_appends_all() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n\np1\n\np2"));
        // Cursor at the far end of the selection, as a shift-selection leaves it.
        editor.set_cursor(DocumentPosition::at(TreePath::root(2), 2));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(1), 0),
            DocumentPosition::at(TreePath::root(2), 2),
        );
        editor.indent().unwrap();
        assert_eq!(editor.document().paragraphs.len(), 1);
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected one list");
        };
        assert_eq!(entries.len(), 3, "a, p1, p2 as items");
    }

    #[test]
    fn indent_selection_of_paragraphs_before_list_prepends_all() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("p1\n\np2\n\n- a"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(1), 2));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(0), 0),
            DocumentPosition::at(TreePath::root(1), 2),
        );
        editor.indent().unwrap();
        assert_eq!(editor.document().paragraphs.len(), 1);
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected one list");
        };
        assert_eq!(entries.len(), 3, "p1, p2, a in order");
        let item = |e| TreePath::root(0).child(PathSegment::ListEntry { entry: e, para: 0 });
        assert_eq!(editor.leaf_plain_text(&item(0)), "p1");
        assert_eq!(editor.leaf_plain_text(&item(2)), "a");
    }

    #[test]
    fn indent_checklist_items_nest_under_preceding_bullet_item() {
        // A bullet item followed by a separate checklist; selecting all the checklist items
        // and indenting nests them under the bullet item as a sub-checklist (checkboxes kept).
        let mut editor = Editor::new();
        let mut doc = markdown_to_document("x");
        let text = |s: &str| Paragraph::Text {
            content: vec![Span::new_text(s)],
        };
        let check = |s: &str| ChecklistItem::new(false).with_content(vec![Span::new_text(s)]);
        doc.paragraphs = vec![
            Paragraph::new_unordered_list().with_entries(vec![vec![text("bullet")]]),
            Paragraph::new_checklist().with_checklist_items(vec![
                check("c1"),
                check("c2"),
                check("c3"),
            ]),
        ];
        editor.set_document(doc);
        // Select all three checklist items (cursor at the far end, as a shift-selection leaves it).
        let item = |c| TreePath::root(1).child(PathSegment::ChecklistItem(c));
        editor.set_cursor(DocumentPosition::at(item(2), 2));
        editor.set_selection(
            DocumentPosition::at(item(0), 0),
            DocumentPosition::at(item(2), 2),
        );
        editor.indent().unwrap();
        // The top-level checklist is gone; the three items are a sub-checklist under "bullet".
        assert_eq!(editor.document().paragraphs.len(), 1, "checklist merged in");
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected the bullet list");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].len(),
            2,
            "bullet item now holds text + sub-checklist"
        );
        let Paragraph::Checklist { items } = &entries[0][1] else {
            panic!("expected a nested checklist (checkboxes preserved)");
        };
        assert_eq!(
            items.len(),
            3,
            "all three items nested together, not staircased"
        );
        let nested = |c| {
            TreePath::root(0)
                .child(PathSegment::ListEntry { entry: 0, para: 1 })
                .child(PathSegment::ChecklistItem(c))
        };
        assert_eq!(editor.leaf_plain_text(&nested(0)), "c1");
        assert_eq!(editor.leaf_plain_text(&nested(2)), "c3");
    }

    #[test]
    fn indent_single_checklist_item_nests_under_preceding_bullet_item() {
        // Even a single first checklist item nests under a preceding bullet item.
        let mut editor = Editor::new();
        let mut doc = markdown_to_document("x");
        let text = |s: &str| Paragraph::Text {
            content: vec![Span::new_text(s)],
        };
        let check = |s: &str| ChecklistItem::new(false).with_content(vec![Span::new_text(s)]);
        doc.paragraphs = vec![
            Paragraph::new_unordered_list().with_entries(vec![vec![text("bullet")]]),
            Paragraph::new_checklist().with_checklist_items(vec![check("c1"), check("c2")]),
        ];
        editor.set_document(doc);
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(1).child(PathSegment::ChecklistItem(0)),
            0,
        ));
        editor.indent().unwrap();
        // "c1" nested under "bullet"; "c2" stays behind in the top-level checklist.
        assert_eq!(editor.document().paragraphs.len(), 2);
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected the bullet list");
        };
        assert!(matches!(entries[0][1], Paragraph::Checklist { .. }));
        let Paragraph::Checklist { items } = &editor.document().paragraphs[1] else {
            panic!("expected the remaining checklist");
        };
        assert_eq!(items.len(), 1, "c2 remains at top level");
    }

    #[test]
    fn outdent_nested_checklist_items_lifts_back_out() {
        // The inverse of indent: checklist items nested under a bullet item, when outdented,
        // lift back out to a top-level checklist (keeping their checkboxes), not text.
        let mut editor = Editor::new();
        let mut doc = markdown_to_document("x");
        let text = |s: &str| Paragraph::Text {
            content: vec![Span::new_text(s)],
        };
        let check = |s: &str| ChecklistItem::new(false).with_content(vec![Span::new_text(s)]);
        doc.paragraphs = vec![Paragraph::new_unordered_list().with_entries(vec![vec![
            text("bullet"),
            Paragraph::new_checklist().with_checklist_items(vec![
                check("c1"),
                check("c2"),
                check("c3"),
            ]),
        ]])];
        editor.set_document(doc);
        let item = |c| {
            TreePath::root(0)
                .child(PathSegment::ListEntry { entry: 0, para: 1 })
                .child(PathSegment::ChecklistItem(c))
        };
        // Select all three nested checklist items and outdent.
        editor.set_cursor(DocumentPosition::at(item(2), 2));
        editor.set_selection(
            DocumentPosition::at(item(0), 0),
            DocumentPosition::at(item(2), 2),
        );
        editor.outdent_list_item().unwrap();
        // The bullet item no longer holds a sub-checklist; the items are a top-level checklist.
        assert_eq!(
            editor.document().paragraphs.len(),
            2,
            "bullet list + checklist"
        );
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected the bullet list");
        };
        assert_eq!(entries[0].len(), 1, "bullet item is just its text again");
        let Paragraph::Checklist { items } = &editor.document().paragraphs[1] else {
            panic!("expected a top-level checklist (not text paragraphs)");
        };
        assert_eq!(items.len(), 3, "all three lifted out together");
        assert_eq!(items[0].content[0].text, "c1");
        assert_eq!(items[2].content[0].text, "c3");
    }

    #[test]
    fn indent_then_outdent_checklist_under_bullet_round_trips() {
        let mut editor = Editor::new();
        let mut doc = markdown_to_document("x");
        let text = |s: &str| Paragraph::Text {
            content: vec![Span::new_text(s)],
        };
        let check = |s: &str| ChecklistItem::new(false).with_content(vec![Span::new_text(s)]);
        doc.paragraphs = vec![
            Paragraph::new_unordered_list().with_entries(vec![vec![text("bullet")]]),
            Paragraph::new_checklist().with_checklist_items(vec![
                check("c1"),
                check("c2"),
                check("c3"),
            ]),
        ];
        let before = format!("{:?}", doc.paragraphs);
        editor.set_document(doc);
        let top = |c| TreePath::root(1).child(PathSegment::ChecklistItem(c));
        editor.set_cursor(DocumentPosition::at(top(2), 2));
        editor.set_selection(
            DocumentPosition::at(top(0), 0),
            DocumentPosition::at(top(2), 2),
        );
        editor.indent().unwrap(); // nest under the bullet
        editor.outdent_list_item().unwrap(); // lift back out
        assert_eq!(
            format!("{:?}", editor.document().paragraphs),
            before,
            "indent then outdent restores the original structure"
        );
    }

    #[test]
    fn indent_nests_paragraph_into_preceding_quote() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("> quoted\n\nfollow"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(1), 0));
        editor.indent().unwrap();
        assert_eq!(editor.document().paragraphs.len(), 1);
        let Paragraph::Quote { children } = &editor.document().paragraphs[0] else {
            panic!("expected a quote");
        };
        assert_eq!(children.len(), 2, "paragraph became a quote child");
    }

    #[test]
    fn indent_nests_quote_child_into_preceding_list_in_quote() {
        // A quote holding a bullet list followed by a plain paragraph (both quote children):
        // Tab on the paragraph nests it into that preceding list, as it does at top level.
        let mut editor = Editor::new();
        let text = |s: &str| Paragraph::new_text().with_content(vec![Span::new_text(s)]);
        let mut doc = markdown_to_document("x");
        doc.paragraphs = vec![Paragraph::new_quote().with_children(vec![
            text("quote lead"),
            Paragraph::new_unordered_list().with_entries(vec![vec![text("item")]]),
            text("make me an item"),
        ])];
        editor.set_document(doc);
        // Cursor in the trailing paragraph (quote child 2).
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(0).child(PathSegment::QuoteChild(2)),
            0,
        ));
        assert!(editor.can_nest_selection_into_adjacent());

        editor.indent().unwrap();
        let Paragraph::Quote { children } = &editor.document().paragraphs[0] else {
            panic!("outer quote should survive");
        };
        assert_eq!(
            children.len(),
            2,
            "the paragraph left the quote's child list"
        );
        let Paragraph::UnorderedList { entries } = &children[1] else {
            panic!("expected the bullet list to remain");
        };
        assert_eq!(entries.len(), 2, "the paragraph became a second list item");
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0)
                .child(PathSegment::QuoteChild(1))
                .child(PathSegment::ListEntry { entry: 1, para: 0 }),
            "cursor follows the paragraph into the list"
        );
    }

    #[test]
    fn indent_second_item_in_quote_nests_under_previous_item() {
        // A list inside a quote, whose first item ends in an inner quote, followed by a second
        // item: Tab nests the second item one level deeper under the first (as it does at the
        // top level), landing it in a new sublist after the inner quote.
        let mut editor = Editor::new();
        let text = |s: &str| Paragraph::new_text().with_content(vec![Span::new_text(s)]);
        let mut doc = markdown_to_document("x");
        doc.paragraphs = vec![Paragraph::new_quote().with_children(vec![
            text("q lead"),
            Paragraph::new_unordered_list().with_entries(vec![
                vec![
                    text("item one"),
                    Paragraph::new_quote().with_children(vec![text("inner quote")]),
                ],
                vec![text("tab me")],
            ]),
        ])];
        editor.set_document(doc);
        let second_item = TreePath::root(0)
            .child(PathSegment::QuoteChild(1))
            .child(PathSegment::ListEntry { entry: 1, para: 0 });
        editor.set_cursor(DocumentPosition::at(second_item, 0));
        assert!(editor.cursor_can_indent());

        editor.indent().unwrap();
        let Paragraph::Quote { children } = &editor.document().paragraphs[0] else {
            panic!("outer quote should survive");
        };
        let Paragraph::UnorderedList { entries } = &children[1] else {
            panic!("expected the bullet list");
        };
        assert_eq!(
            entries.len(),
            1,
            "the second item left the outer list level"
        );
        assert_eq!(
            entries[0].len(),
            3,
            "item one now holds its text, the inner quote, and the nested sublist"
        );
        assert!(
            matches!(entries[0][1], Paragraph::Quote { .. }),
            "the inner quote is untouched"
        );
        let Paragraph::UnorderedList { entries: sub } = &entries[0][2] else {
            panic!("the second item nested into a new sublist after the quote");
        };
        assert_eq!(sub.len(), 1, "the nested sublist holds the indented item");
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0)
                .child(PathSegment::QuoteChild(1))
                .child(PathSegment::ListEntry { entry: 0, para: 2 })
                .child(PathSegment::ListEntry { entry: 0, para: 0 }),
            "cursor follows the item into the sublist"
        );
    }

    #[test]
    fn indent_first_list_item_after_quote_nests_into_quote() {
        // A top-level quote directly followed by a top-level bullet list: Tab on the list's
        // (first) item moves it *into* the quote but keeps it a list item — as an entry of a
        // list child of the quote — and the emptied outer list is pruned.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("> This is a quote\n\n- tab me"));
        let item = TreePath::root(1).child(PathSegment::ListEntry { entry: 0, para: 0 });
        editor.set_cursor(DocumentPosition::at(item, 0));
        assert!(editor.cursor_can_indent());

        editor.indent().unwrap();
        assert_eq!(
            editor.document().paragraphs.len(),
            1,
            "the emptied outer list was removed; only the quote remains"
        );
        let Paragraph::Quote { children } = &editor.document().paragraphs[0] else {
            panic!("expected the quote to absorb the item");
        };
        assert_eq!(
            children.len(),
            2,
            "quote now holds its text plus a list child"
        );
        let Paragraph::UnorderedList { entries } = &children[1] else {
            panic!("the item stays a bullet list, now inside the quote");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0][0].content()[0].text, "tab me");
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0)
                .child(PathSegment::QuoteChild(1))
                .child(PathSegment::ListEntry { entry: 0, para: 0 }),
            "cursor follows the item into the quote's list"
        );
    }

    #[test]
    fn indent_first_list_item_after_quote_keeps_remaining_items() {
        // Only the first item moves into the quote; later items stay in the outer list.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("> quote\n\n- one\n- two"));
        let first = TreePath::root(1).child(PathSegment::ListEntry { entry: 0, para: 0 });
        editor.set_cursor(DocumentPosition::at(first, 0));
        editor.indent().unwrap();
        assert_eq!(
            editor.document().paragraphs.len(),
            2,
            "quote + the leftover list"
        );
        let Paragraph::Quote { children } = &editor.document().paragraphs[0] else {
            panic!("expected the quote");
        };
        assert_eq!(
            children.len(),
            2,
            "the first item joined the quote as a list child"
        );
        let Paragraph::UnorderedList { entries } = &children[1] else {
            panic!("the moved item stays a bullet list inside the quote");
        };
        assert_eq!(entries[0][0].content()[0].text, "one");
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[1] else {
            panic!("expected the remaining list");
        };
        assert_eq!(entries.len(), 1, "one item left in the outer list");
        assert_eq!(entries[0][0].content()[0].text, "two");
    }

    #[test]
    fn outdent_list_item_in_quote_lifts_out_keeping_bullet() {
        // The inverse of Tab: a bullet list nested inside a quote — Shift-Tab on its item
        // lifts it out of the quote as a top-level list item (not a plain text paragraph).
        let mut editor = Editor::new();
        let mut doc = markdown_to_document("x");
        doc.paragraphs = vec![Paragraph::new_quote().with_children(vec![
            Paragraph::new_text().with_content(vec![Span::new_text("This is a quote")]),
            Paragraph::new_unordered_list().with_entries(vec![vec![
                Paragraph::new_text().with_content(vec![Span::new_text("tab me")]),
            ]]),
        ])];
        editor.set_document(doc);
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(0)
                .child(PathSegment::QuoteChild(1))
                .child(PathSegment::ListEntry { entry: 0, para: 0 }),
            0,
        ));

        editor.outdent_list_item().unwrap();
        assert_eq!(
            editor.document().paragraphs.len(),
            2,
            "the item left the quote as a sibling list"
        );
        let Paragraph::Quote { children } = &editor.document().paragraphs[0] else {
            panic!("the quote keeps its own text");
        };
        assert_eq!(children.len(), 1, "only the quote's text remains inside it");
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[1] else {
            panic!("the item stays a bullet list, now outside the quote");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0][0].content()[0].text, "tab me");
        assert_eq!(
            editor.cursor().path,
            TreePath::root(1).child(PathSegment::ListEntry { entry: 0, para: 0 }),
        );
    }

    #[test]
    fn indent_then_outdent_list_item_across_quote_round_trips() {
        // Tab into the preceding quote, then Shift-Tab back out, restores the original tree.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("> quote\n\n- tab me"));
        let before = format!("{:?}", editor.document().paragraphs);
        let item = TreePath::root(1).child(PathSegment::ListEntry { entry: 0, para: 0 });
        editor.set_cursor(DocumentPosition::at(item, 0));

        editor.indent().unwrap(); // into the quote
        editor.outdent_list_item().unwrap(); // back out
        assert_eq!(
            format!("{:?}", editor.document().paragraphs),
            before,
            "indent into the quote then outdent restores the original structure"
        );
    }

    #[test]
    fn continuation_paragraph_stays_in_list_item() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- item"));
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 0 }),
            4,
        ));
        editor.insert_continuation().unwrap();
        editor.insert_text("cont").unwrap();
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected a list");
        };
        assert_eq!(entries.len(), 1, "still one list item");
        assert_eq!(entries[0].len(), 2, "the item holds two paragraphs");
        // Contrast: plain Enter (insert_newline) would make a second list item instead.
    }

    #[test]
    fn newline_in_list_item_makes_a_new_item() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- item"));
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 0 }),
            4,
        ));
        editor.insert_newline().unwrap();
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected a list");
        };
        assert_eq!(entries.len(), 2, "Enter starts a new list item");
    }

    #[test]
    fn paste_multi_paragraph_into_list_item_keeps_styling() {
        // Regression: pasting a multi-paragraph fragment (e.g. several copied list
        // items) into an empty list item used to dump raw Markdown text, losing all
        // inline styling. It should instead splice each paragraph run-by-run into its
        // own list item, preserving links/emphasis.
        // An empty bullet, as the live editor represents it: one list entry holding a
        // single empty text paragraph (not the degenerate empty entry that parsing bare
        // "- " yields).
        let mut editor = Editor::new();
        let mut doc = Document::new();
        doc.add_paragraph(Paragraph::UnorderedList {
            entries: vec![vec![Paragraph::new_text()]],
        });
        editor.set_document(doc);
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 0 }),
            0,
        ));

        let fragment = markdown_to_document(
            "[erster](https://example.net) — one\n\n[zweiter](https://example.net) — two",
        );
        editor.insert_document(&fragment).unwrap();

        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("expected a list, got {:?}", editor.document().paragraphs);
        };
        assert_eq!(
            entries.len(),
            2,
            "each pasted paragraph becomes a list item"
        );

        // The link survives as a real link run rather than literal "[erster](…)" text.
        let md = document_to_markdown(editor.document());
        assert!(
            md.contains("[erster](https://example.net)")
                && md.contains("[zweiter](https://example.net)"),
            "links preserved, not flattened to text: {md}"
        );
        assert!(
            !md.contains("\\["),
            "no escaped literal Markdown brackets: {md}"
        );
    }

    #[test]
    fn cursor_can_unnest_truth_table() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("plain"));
        cursor_at(&mut editor, TreePath::root(0));
        assert!(!editor.cursor_can_unnest());

        editor.set_document(markdown_to_document("> quoted"));
        cursor_at(
            &mut editor,
            TreePath::root(0).child(PathSegment::QuoteChild(0)),
        );
        assert!(editor.cursor_can_unnest());

        editor.set_document(markdown_to_document("- item"));
        cursor_at(
            &mut editor,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 0 }),
        );
        assert!(editor.cursor_can_unnest());
        assert!(editor.cursor_can_indent());
    }

    #[test]
    fn toggle_bold_over_selection() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("hello world"));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(0), 0),
            DocumentPosition::at(TreePath::root(0), 5),
        );
        editor.toggle_bold().unwrap();
        assert_eq!(md(&editor), "**hello** world");
        // Toggling again removes it.
        editor.toggle_bold().unwrap();
        assert_eq!(md(&editor), "hello world");
    }

    #[test]
    fn insert_and_remove_link() {
        let mut editor = Editor::new();
        editor.insert_text("see ").unwrap();
        editor
            .insert_link_at_cursor("https://example.test", "here")
            .unwrap();
        assert_eq!(md(&editor), "see [here](https://example.test)");
        editor.remove_link_at(TreePath::root(0), 1).unwrap();
        assert_eq!(md(&editor), "see here");
    }

    #[test]
    fn heading_and_paragraph_conversion() {
        let mut editor = Editor::new();
        editor.insert_text("Title").unwrap();
        editor
            .set_block_type(BlockType::Heading { level: 2 })
            .unwrap();
        assert_eq!(md(&editor), "## Title");
        editor.set_block_type(BlockType::Paragraph).unwrap();
        assert_eq!(md(&editor), "Title");
    }

    #[test]
    fn toggle_list_wraps_and_unwraps() {
        let mut editor = Editor::new();
        editor.insert_text("item").unwrap();
        editor.toggle_list().unwrap();
        assert_eq!(md(&editor), "- item");
        assert!(matches!(
            editor.current_block_type(),
            BlockType::ListItem { ordered: false, .. }
        ));
        editor.toggle_list().unwrap();
        assert_eq!(md(&editor), "item");
    }

    /// Toggle a list kind over a selection spanning a plain paragraph and a list, driving the
    /// selection from either end (i.e. cursor in the text vs. cursor in the list). The result
    /// must be identical either way — the whole range becomes one list of the target kind.
    fn assert_range_toggle_reproducible(
        markdown: &str,
        a: TreePath,
        b: TreePath,
        toggle: impl Fn(&mut Editor),
        expected: &str,
    ) {
        for (cursor_end, other) in [(a.clone(), b.clone()), (b.clone(), a.clone())] {
            let mut editor = Editor::new();
            editor.set_document(markdown_to_document(markdown));
            editor.set_cursor(DocumentPosition::at(cursor_end.clone(), 0));
            editor.set_selection(
                DocumentPosition::at(other, 0),
                DocumentPosition::at(cursor_end, 0),
            );
            toggle(&mut editor);
            assert_eq!(
                md(&editor),
                expected,
                "toggling over the range should not depend on selection direction"
            );
        }
    }

    #[test]
    fn toggle_ordered_over_text_and_list_is_reproducible() {
        // "first" (text) followed by an ordered list; selecting both and choosing Numbered
        // List makes the whole range one ordered list — no matter which way the selection runs.
        assert_range_toggle_reproducible(
            "first\n\n1. second\n2. third",
            TreePath::root(0),
            list_item_path_at(1, 1),
            |e| e.toggle_ordered_list().unwrap(),
            "1. first\n2. second\n3. third",
        );
    }

    #[test]
    fn toggle_checklist_over_text_and_ordered_is_reproducible() {
        assert_range_toggle_reproducible(
            "first\n\n1. second\n2. third",
            TreePath::root(0),
            list_item_path_at(1, 1),
            |e| e.toggle_checklist().unwrap(),
            "- [ ] first\n- [ ] second\n- [ ] third",
        );
    }

    #[test]
    fn toggle_ordered_over_list_then_text_is_reproducible() {
        // The mirror arrangement: list first, plain paragraph last.
        assert_range_toggle_reproducible(
            "1. first\n2. second\n\nthird",
            list_item_path_at(0, 0),
            TreePath::root(1),
            |e| e.toggle_ordered_list().unwrap(),
            "1. first\n2. second\n3. third",
        );
    }

    #[test]
    fn toggle_bullet_over_three_plain_paragraphs_is_one_list() {
        assert_range_toggle_reproducible(
            "one\n\ntwo\n\nthree",
            TreePath::root(0),
            TreePath::root(2),
            |e| e.toggle_list().unwrap(),
            "- one\n- two\n- three",
        );
        // ...and the resulting bullet list is a single node.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("one\n\ntwo\n\nthree"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 0));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(0), 0),
            DocumentPosition::at(TreePath::root(2), 0),
        );
        editor.toggle_list().unwrap();
        assert_eq!(editor.document().paragraphs.len(), 1);
        assert!(matches!(
            editor.document().paragraphs[0],
            Paragraph::UnorderedList { .. }
        ));
    }

    #[test]
    fn toggle_ordered_over_mixed_list_kinds_unifies_them() {
        // A bullet list, a plain paragraph, and a checklist, all selected and toggled ordered,
        // collapse into one ordered list preserving every item's text in order.
        assert_range_toggle_reproducible(
            "- a\n- b\n\nmid\n\n- [ ] c\n- [ ] d",
            list_item_path_at(0, 0),
            TreePath::root(2).child(PathSegment::ChecklistItem(1)),
            |e| e.toggle_ordered_list().unwrap(),
            "1. a\n2. b\n3. mid\n4. c\n5. d",
        );
    }

    #[test]
    fn toggle_ordered_over_full_ordered_list_toggles_off() {
        // Whole range already the target kind → toggling delists it back to plain paragraphs.
        // (Two adjacent ordered-list nodes only arise programmatically; exercise it directly.)
        let mut editor = Editor::new();
        editor.set_document(Document {
            paragraphs: vec![
                Paragraph::new_ordered_list().with_entries(vec![vec![
                    Paragraph::new_text().with_content(vec![Span::new_text("a")]),
                ]]),
                Paragraph::new_ordered_list().with_entries(vec![vec![
                    Paragraph::new_text().with_content(vec![Span::new_text("b")]),
                ]]),
            ],
            ..Default::default()
        });
        editor.set_cursor(DocumentPosition::at(list_item_path_at(1, 0), 0));
        editor.set_selection(
            DocumentPosition::at(list_item_path_at(0, 0), 0),
            DocumentPosition::at(list_item_path_at(1, 0), 0),
        );
        editor.toggle_ordered_list().unwrap();
        assert_eq!(md(&editor), "a\n\nb");
    }

    #[test]
    fn range_toggle_leaves_selection_over_result() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("first\n\n1. second\n2. third"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 0));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(0), 0),
            DocumentPosition::at(list_item_path_at(1, 1), 0),
        );
        editor.toggle_ordered_list().unwrap();
        // A follow-up toggle sees the whole new list selected and delists all of it.
        assert!(editor.selection().is_some());
        editor.toggle_ordered_list().unwrap();
        assert_eq!(md(&editor), "first\n\nsecond\n\nthird");
    }

    #[test]
    fn enter_at_end_of_heading_starts_plain_paragraph() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("# Title"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 5));
        editor.insert_newline().unwrap();
        // The heading stays; Enter at its end opens a normal paragraph below.
        assert!(matches!(
            editor.document().paragraphs[0],
            Paragraph::Header1 { .. }
        ));
        assert!(matches!(
            editor.document().paragraphs[1],
            Paragraph::Text { .. }
        ));
        assert_eq!(editor.cursor().path, TreePath::root(1));
        assert!(matches!(editor.current_block_type(), BlockType::Paragraph));
    }

    #[test]
    fn enter_at_start_of_heading_keeps_heading_style() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("# Title"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 0));
        editor.insert_newline().unwrap();
        assert_eq!(editor.leaf_count(), 2);
        // Both halves keep the heading block type — the content is not demoted to a plain
        // paragraph. The cursor stays with the text below.
        assert!(matches!(
            editor.document().paragraphs[0],
            Paragraph::Header1 { .. }
        ));
        assert_eq!(editor.leaf_plain_text(&TreePath::root(0)), "");
        assert!(matches!(
            editor.document().paragraphs[1],
            Paragraph::Header1 { .. }
        ));
        assert_eq!(editor.leaf_plain_text(&TreePath::root(1)), "Title");
        assert_eq!(editor.cursor().path, TreePath::root(1));
        assert_eq!(editor.cursor().offset, 0);
    }

    #[test]
    fn same_kind_toggle_on_nested_item_is_noop() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("1. one\n2. two"));
        editor.set_cursor(DocumentPosition::at(list_item_path(1), 0));
        editor.indent_list_item().unwrap(); // "two" nested under "one" (ordered)
        let doc_before = editor.document().clone();
        let cursor_before = editor.cursor();
        // Already an ordered nested item → toggling ordered list does nothing.
        editor.toggle_ordered_list().unwrap();
        assert_eq!(*editor.document(), doc_before);
        assert_eq!(editor.cursor(), cursor_before);
    }

    #[test]
    fn changing_nested_list_kind_preserves_nesting() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("1. one\n2. two"));
        editor.set_cursor(DocumentPosition::at(list_item_path(1), 0));
        editor.indent_list_item().unwrap(); // "two" nested under "one" (ordered)
        assert_eq!(leaf_depths(&editor), vec![0, 1]);
        // Cursor is on the nested item; switch it to a bullet list.
        editor.toggle_list().unwrap();
        // Nesting is intact and the outer level is untouched.
        assert_eq!(leaf_depths(&editor), vec![0, 1]);
        assert!(matches!(
            editor.document().paragraphs[0],
            Paragraph::OrderedList { .. }
        ));
        assert!(matches!(
            editor.current_block_type(),
            BlockType::ListItem { ordered: false, .. }
        ));
        // The tree still round-trips through markdown.
        let reparsed = markdown_to_document(&md(&editor));
        assert_eq!(*editor.document(), reparsed);
    }

    #[test]
    fn enter_in_list_creates_new_item() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- one"));
        let item = TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 0 });
        editor.set_cursor(DocumentPosition::at(item, 3));
        editor.insert_newline().unwrap();
        editor.insert_text("two").unwrap();
        assert_eq!(md(&editor), "- one\n- two");
    }

    #[test]
    fn backspace_merges_list_item_into_previous() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- one\n- two"));
        let second = TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 });
        editor.set_cursor(DocumentPosition::at(second, 0));
        editor.delete_backward().unwrap();
        assert_eq!(md(&editor), "- onetwo");
    }

    #[test]
    fn ordered_list_renumbers_automatically() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("1. one\n2. two\n3. three"));
        // Delete the middle item by merging it into the first.
        let second = TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 });
        editor.set_cursor(DocumentPosition::at(second, 0));
        editor.delete_backward().unwrap();
        // "onetwo" then "three" → renumbered 1, 2.
        assert_eq!(md(&editor), "1. onetwo\n2. three");
    }

    #[test]
    fn toggle_quote_wraps_and_unwraps() {
        let mut editor = Editor::new();
        editor.insert_text("quoted").unwrap();
        editor.toggle_quote().unwrap();
        assert_eq!(md(&editor), "> quoted");
        editor.toggle_quote().unwrap();
        assert_eq!(md(&editor), "quoted");
    }

    #[test]
    fn toggle_checkmark_round_trips() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- [ ] task"));
        let item = TreePath::root(0).child(PathSegment::ChecklistItem(0));
        editor.set_cursor(DocumentPosition::at(item, 0));
        assert_eq!(editor.toggle_current_checkmark(), Ok(true));
        assert_eq!(md(&editor), "- [x] task");
    }

    #[test]
    fn convert_paragraph_above_checklist_merges_into_it() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("new\n\n- [ ] task"));
        // Cursor in the fresh plain paragraph at the top.
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 3));
        editor.toggle_checklist().unwrap();
        // The new item should merge into the following checklist, not form a second one.
        assert_eq!(md(&editor), "- [ ] new\n- [ ] task");
        assert!(matches!(
            editor.document().paragraphs[0],
            Paragraph::Checklist { .. }
        ));
        assert_eq!(editor.document().paragraphs.len(), 1);
        // Cursor stays on the new (first) item.
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0).child(PathSegment::ChecklistItem(0))
        );
    }

    #[test]
    fn convert_paragraph_below_checklist_merges_into_it() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- [ ] task\n\nnew"));
        // Cursor in the fresh plain paragraph below the checklist.
        editor.set_cursor(DocumentPosition::at(TreePath::root(1), 3));
        editor.toggle_checklist().unwrap();
        assert_eq!(md(&editor), "- [ ] task\n- [ ] new");
        assert_eq!(editor.document().paragraphs.len(), 1);
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0).child(PathSegment::ChecklistItem(1))
        );
    }

    #[test]
    fn convert_paragraph_above_bullet_list_merges_into_it() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("new\n\n- task"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 3));
        editor.toggle_list().unwrap();
        assert_eq!(md(&editor), "- new\n- task");
        assert_eq!(editor.document().paragraphs.len(), 1);
        assert_eq!(editor.cursor().path, list_item_path(0));
    }

    #[test]
    fn convert_paragraph_below_bullet_list_merges_into_it() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- task\n\nnew"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(1), 3));
        editor.toggle_list().unwrap();
        assert_eq!(md(&editor), "- task\n- new");
        assert_eq!(editor.document().paragraphs.len(), 1);
        assert_eq!(editor.cursor().path, list_item_path(1));
    }

    #[test]
    fn convert_paragraph_above_ordered_list_merges_and_renumbers() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("new\n\n1. task"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 3));
        editor.toggle_ordered_list().unwrap();
        assert_eq!(md(&editor), "1. new\n2. task");
        assert_eq!(editor.document().paragraphs.len(), 1);
        assert_eq!(editor.cursor().path, list_item_path(0));
    }

    #[test]
    fn convert_paragraph_below_ordered_list_merges_and_renumbers() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("1. task\n\nnew"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(1), 3));
        editor.toggle_ordered_list().unwrap();
        assert_eq!(md(&editor), "1. task\n2. new");
        assert_eq!(editor.document().paragraphs.len(), 1);
        assert_eq!(editor.cursor().path, list_item_path(1));
    }

    #[test]
    fn convert_paragraph_between_bullet_and_checklist_only_merges_own_kind() {
        // A plain paragraph sitting between a bullet list and a checklist, turned into a
        // bullet item, joins the bullet list above and leaves the checklist below intact.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n\nnew\n\n- [ ] task"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(1), 3));
        editor.toggle_list().unwrap();
        assert_eq!(md(&editor), "- a\n- new\n\n- [ ] task");
        assert_eq!(editor.document().paragraphs.len(), 2);
    }

    #[test]
    fn move_block_down_swaps_with_next() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("first\n\nsecond"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 0));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "second\n\nfirst");
    }

    #[test]
    fn move_block_reorders_list_items() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b\n- c"));
        editor.set_cursor(DocumentPosition::at(list_item_path(0), 0));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "- b\n- a\n- c");
        // The cursor follows the moved item.
        assert_eq!(editor.cursor().path, list_item_path(1));
        assert_eq!(editor.move_blocks_up(), Ok(true));
        assert_eq!(md(&editor), "- a\n- b\n- c");
        assert_eq!(editor.cursor().path, list_item_path(0));
    }

    #[test]
    fn move_block_stays_inside_nested_sublist() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n  - x\n  - y\n  - z"));
        let x = TreePath::root(0)
            .child(PathSegment::ListEntry { entry: 0, para: 1 })
            .child(PathSegment::ListEntry { entry: 0, para: 0 });
        editor.set_cursor(DocumentPosition::at(x, 0));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        // Reordered within the sublist; nesting depths are untouched.
        assert_eq!(leaf_texts(&editor), vec!["a", "y", "x", "z"]);
        assert_eq!(leaf_depths(&editor), vec![0, 1, 1, 1]);
        let moved = TreePath::root(0)
            .child(PathSegment::ListEntry { entry: 0, para: 1 })
            .child(PathSegment::ListEntry { entry: 1, para: 0 });
        assert_eq!(editor.cursor().path, moved);
    }

    #[test]
    fn move_block_carries_whole_list_item_subtree() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n  - x\n- b"));
        editor.set_cursor(DocumentPosition::at(list_item_path(0), 0));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        // The whole "a" item — its nested "x" included — moves below "b".
        assert_eq!(leaf_texts(&editor), vec!["b", "a", "x"]);
        assert_eq!(leaf_depths(&editor), vec![0, 0, 1]);
    }

    #[test]
    fn move_block_reorders_quote_children() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("> a\n>\n> b"));
        // Precondition: the quote really has two children.
        assert_eq!(leaf_texts(&editor), vec!["a", "b"]);
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(0).child(PathSegment::QuoteChild(0)),
            0,
        ));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(leaf_texts(&editor), vec!["b", "a"]);
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0).child(PathSegment::QuoteChild(1))
        );
    }

    #[test]
    fn move_block_reorders_checklist_items() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- [ ] a\n- [ ] b"));
        assert_eq!(leaf_texts(&editor), vec!["a", "b"]);
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(0).child(PathSegment::ChecklistItem(0)),
            0,
        ));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(leaf_texts(&editor), vec!["b", "a"]);
    }

    // ----- Cross-boundary block moves (Alt-Up/Down) -----

    /// Checkbox state of every leaf in document order (`None` = not a checklist item).
    fn leaf_checks(editor: &Editor) -> Vec<Option<bool>> {
        tree_walk::enumerate_leaves(editor.document())
            .iter()
            .map(|l| l.marker.as_ref().and_then(|m| m.checkbox))
            .collect()
    }

    /// Assert the selection spans exactly leaves `texts` (in order), by their plain text.
    fn assert_selection_texts(editor: &Editor, texts: &[&str]) {
        let (s, e) = editor.selection().expect("a selection");
        let selected: Vec<String> = tree_walk::leaf_paths(editor.document())
            .into_iter()
            .filter(|p| *p >= s.path && *p <= e.path)
            .map(|p| editor.leaf_plain_text(&p))
            .collect();
        assert_eq!(selected, texts);
    }

    #[test]
    fn move_selected_paragraphs_down_together() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("A\n\nB\n\nC\n\nD"));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(1), 0),
            DocumentPosition::at(TreePath::root(2), 1),
        );
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "A\n\nD\n\nB\n\nC");
        assert_selection_texts(&editor, &["B", "C"]);
    }

    #[test]
    fn move_selected_paragraphs_up_together() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("A\n\nB\n\nC\n\nD"));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(1), 0),
            DocumentPosition::at(TreePath::root(2), 1),
        );
        assert_eq!(editor.move_blocks_up(), Ok(true));
        assert_eq!(md(&editor), "B\n\nC\n\nA\n\nD");
        assert_selection_texts(&editor, &["B", "C"]);
    }

    #[test]
    fn move_selected_list_items_reorder_together() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b\n- c\n- d\n- e"));
        editor.set_selection(
            DocumentPosition::at(list_item_path(1), 0),
            DocumentPosition::at(list_item_path(2), 1),
        );
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "- a\n- d\n- b\n- c\n- e");
        assert_selection_texts(&editor, &["b", "c"]);
    }

    #[test]
    fn move_selected_paragraphs_at_document_edge_is_noop() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("A\n\nB\n\nC"));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(0), 0),
            DocumentPosition::at(TreePath::root(1), 1),
        );
        assert_eq!(editor.move_blocks_up(), Ok(false));
        assert_eq!(md(&editor), "A\n\nB\n\nC");
    }

    #[test]
    fn move_selected_checklist_items_down_across_heading_and_merge() {
        // Two checked/unchecked items leave their checklist together, cross a heading, and
        // merge into the next checklist as a group — checkboxes preserved, selection kept.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document(
            "- [ ] a\n- [x] b\n- [ ] c\n\n## H\n\n- [ ] z",
        ));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(0).child(PathSegment::ChecklistItem(1)), 0),
            DocumentPosition::at(TreePath::root(0).child(PathSegment::ChecklistItem(2)), 1),
        );
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "- [ ] a\n\n## H\n\n- [x] b\n- [ ] c\n- [ ] z");
        assert_selection_texts(&editor, &["b", "c"]);
    }

    #[test]
    fn move_selected_ordered_items_up_across_heading_together() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("# T\n\n1. one\n2. two\n3. three"));
        editor.set_selection(
            DocumentPosition::at(list_item_path_at(1, 0), 0),
            DocumentPosition::at(list_item_path_at(1, 1), 1),
        );
        assert_eq!(editor.move_blocks_up(), Ok(true));
        assert_eq!(md(&editor), "1. one\n2. two\n\n# T\n\n1. three");
        assert_selection_texts(&editor, &["one", "two"]);
    }

    #[test]
    fn move_first_ordered_item_up_hops_over_heading() {
        // The reported scenario: moving the first item up carries it *past* the heading in one
        // step (as its own numbered list), rather than splitting off a same-position list that
        // just renumbers. The remaining list renumbers from 1.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document(
            "# Test 123\n\n1. erster\n2. zweiter\n3. dritter",
        ));
        editor.set_cursor(DocumentPosition::at(list_item_path_at(1, 0), 0));
        assert_eq!(editor.move_blocks_up(), Ok(true));
        assert_eq!(
            md(&editor),
            "1. erster\n\n# Test 123\n\n1. zweiter\n2. dritter"
        );
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 0 })
        );
    }

    #[test]
    fn move_last_ordered_item_down_hops_over_following_block() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("1. a\n2. b\n3. c\n\nafter"));
        editor.set_cursor(DocumentPosition::at(list_item_path(2), 0));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "1. a\n2. b\n\nafter\n\n1. c");
        assert_eq!(
            editor.cursor().path,
            TreePath::root(2).child(PathSegment::ListEntry { entry: 0, para: 0 })
        );
    }

    #[test]
    fn move_lone_list_down_into_following_list_merges() {
        // Inverse of `move_first_ordered_item_up_hops_over_heading`: a lone numbered item above
        // a heading moves down past it and merges into the list below (continuous numbering),
        // rather than landing as a separate list that restarts at 1.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document(
            "1. erster\n\n# Test 123\n\n1. zweiter\n2. dritter",
        ));
        editor.set_cursor(DocumentPosition::at(list_item_path_at(0, 0), 0));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(
            md(&editor),
            "# Test 123\n\n1. erster\n2. zweiter\n3. dritter"
        );
        assert_eq!(
            editor.cursor().path,
            TreePath::root(1).child(PathSegment::ListEntry { entry: 0, para: 0 })
        );
    }

    #[test]
    fn move_first_item_up_with_nothing_above_the_list_is_noop() {
        // Nothing to move past → no split, no renumber.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("1. a\n2. b"));
        editor.set_cursor(DocumentPosition::at(list_item_path(0), 0));
        assert_eq!(editor.move_blocks_up(), Ok(false));
        assert_eq!(md(&editor), "1. a\n2. b");
    }

    #[test]
    fn move_checklist_item_travels_down_across_heading_and_merges() {
        // A checked item leaves its checklist, hops the heading, and joins the next checklist —
        // keeping its checkbox the whole way.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document(
            "- [ ] a\n- [x] b\n\n## Notes\n\n- [ ] c",
        ));
        let b = TreePath::root(0).child(PathSegment::ChecklistItem(1));
        editor.set_cursor(DocumentPosition::at(b, 1));

        // One press: b leaves its checklist, crosses the heading, and merges into c's checklist
        // as its first item — one visual line down (b and the heading swap places).
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "- [ ] a\n\n## Notes\n\n- [x] b\n- [ ] c");
        // Checkbox preserved (the middle `None` is the heading); cursor and offset ride along.
        assert_eq!(
            leaf_checks(&editor),
            vec![Some(false), None, Some(true), Some(false)]
        );
        assert_eq!(
            editor.cursor().path,
            TreePath::root(2).child(PathSegment::ChecklistItem(0))
        );
        assert_eq!(editor.cursor().offset, 1);
    }

    #[test]
    fn move_checklist_item_travels_up_across_heading_and_merges() {
        // Mirror of the down case: the first item of the second checklist hops up over the
        // heading and merges into the first checklist at its end, in one press.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document(
            "- [ ] a\n\n## Notes\n\n- [x] b\n- [ ] c",
        ));
        let b = TreePath::root(2).child(PathSegment::ChecklistItem(0));
        editor.set_cursor(DocumentPosition::at(b, 0));
        assert_eq!(editor.move_blocks_up(), Ok(true));
        assert_eq!(md(&editor), "- [ ] a\n- [x] b\n\n## Notes\n\n- [ ] c");
        assert_eq!(
            leaf_checks(&editor),
            vec![Some(false), Some(true), None, Some(false)]
        );
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0).child(PathSegment::ChecklistItem(1))
        );
    }

    #[test]
    fn move_up_then_down_round_trips_across_heading() {
        // erster up (lands separate above the heading) then down (merges back) returns to start.
        let mut editor = Editor::new();
        let start = "# Test 123\n\n1. erster\n2. zweiter\n3. dritter";
        editor.set_document(markdown_to_document(start));
        editor.set_cursor(DocumentPosition::at(list_item_path_at(1, 0), 0));
        assert_eq!(editor.move_blocks_up(), Ok(true));
        assert_eq!(
            md(&editor),
            "1. erster\n\n# Test 123\n\n1. zweiter\n2. dritter"
        );
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), start);
    }

    #[test]
    fn move_interior_list_item_still_swaps_in_place() {
        // Reordering within a list is unchanged by the cross-boundary logic.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b\n- c"));
        editor.set_cursor(DocumentPosition::at(list_item_path(0), 0));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "- b\n- a\n- c");
        assert_eq!(editor.cursor().path, list_item_path(1));
    }

    #[test]
    fn move_paragraph_collapses_into_following_list() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("para\n\n- a\n- b"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 4));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "- para\n- a\n- b");
        assert_eq!(editor.cursor().path, list_item_path(0));
        assert_eq!(editor.cursor().offset, 4);
    }

    #[test]
    fn move_paragraph_collapses_into_preceding_list_at_its_end() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b\n\npara"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(1), 0));
        assert_eq!(editor.move_blocks_up(), Ok(true));
        assert_eq!(md(&editor), "- a\n- b\n- para");
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 2, para: 0 })
        );
    }

    #[test]
    fn move_paragraph_collapses_into_checklist() {
        // A plain paragraph drawn into a checklist becomes an (unchecked) checklist item.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("para\n\n- [ ] a"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 0));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "- [ ] para\n- [ ] a");
        assert_eq!(leaf_checks(&editor), vec![Some(false), Some(false)]);
    }

    #[test]
    fn move_heading_hops_over_a_list_rather_than_joining_it() {
        // A heading is a structural divider, not list content — it jumps past the whole list.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("# Title\n\n- a\n- b"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 0));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "- a\n- b\n\n# Title");
        assert_eq!(editor.cursor().path, TreePath::root(1));
    }

    #[test]
    fn move_quote_child_exits_quote_at_boundary() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("> a\n>\n> b\n\nafter"));
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(0).child(PathSegment::QuoteChild(1)),
            0,
        ));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(md(&editor), "> a\n\nb\n\nafter");
        assert_eq!(editor.cursor().path, TreePath::root(1));
    }

    #[test]
    fn move_top_level_block_at_document_edge_is_noop() {
        // A plain block already at the document's start/end cannot move further.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("first\n\nsecond"));
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 0));
        assert_eq!(editor.move_blocks_up(), Ok(false));
        editor.set_cursor(DocumentPosition::at(TreePath::root(1), 0));
        assert_eq!(editor.move_blocks_down(), Ok(false));
        assert_eq!(md(&editor), "first\n\nsecond");
    }

    #[test]
    fn move_lone_item_at_document_edge_is_noop() {
        // A single-item list that is the last block can't leave the document.
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("text\n\n- only"));
        editor.set_cursor(DocumentPosition::at(list_item_path_at(1, 0), 0));
        assert_eq!(editor.move_blocks_down(), Ok(false));
        assert_eq!(md(&editor), "text\n\n- only");
    }

    #[test]
    fn move_nested_sublist_item_at_edge_stays_put() {
        // Cross-boundary moves are scoped to top-level / quote lists; a sublist item at its
        // edge keeps the old no-op behavior (Shift-Tab is the way out of a sublist).
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n  - x\n  - y"));
        let x = TreePath::root(0)
            .child(PathSegment::ListEntry { entry: 0, para: 1 })
            .child(PathSegment::ListEntry { entry: 0, para: 0 });
        editor.set_cursor(DocumentPosition::at(x, 0));
        assert_eq!(editor.move_blocks_up(), Ok(false));
        assert_eq!(leaf_texts(&editor), vec!["a", "x", "y"]);
        assert_eq!(leaf_depths(&editor), vec![0, 1, 1]);
    }

    #[test]
    fn move_block_preserves_cursor_offset() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- apple\n- b"));
        editor.set_cursor(DocumentPosition::at(list_item_path(0), 3));
        assert_eq!(editor.move_blocks_down(), Ok(true));
        assert_eq!(editor.cursor().path, list_item_path(1));
        assert_eq!(editor.cursor().offset, 3);
    }

    #[test]
    fn multi_paragraph_paste_splits_blocks() {
        let mut editor = Editor::new();
        editor.insert_text("AB").unwrap();
        editor.set_cursor(DocumentPosition::at(TreePath::root(0), 1));
        editor.paste("X\nY").unwrap();
        // "A|B" + "X\nY": first line merges into the current block, last line takes the tail.
        assert_eq!(md(&editor), "AX\n\nYB");
    }

    #[test]
    fn cross_leaf_delete_selection_merges() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("hello\n\nworld"));
        editor.set_selection(
            DocumentPosition::at(TreePath::root(0), 3),
            DocumentPosition::at(TreePath::root(1), 2),
        );
        editor.delete_selection().unwrap();
        assert_eq!(md(&editor), "helrld");
    }

    fn leaf_depths(editor: &Editor) -> Vec<usize> {
        tree_walk::enumerate_leaves(editor.document())
            .iter()
            .map(|l| l.depth)
            .collect()
    }

    fn list_item_path(entry: usize) -> TreePath {
        TreePath::root(0).child(PathSegment::ListEntry { entry, para: 0 })
    }

    fn list_item_path_at(paragraph: usize, entry: usize) -> TreePath {
        TreePath::root(paragraph).child(PathSegment::ListEntry { entry, para: 0 })
    }

    #[test]
    fn indent_nests_under_previous_sibling() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b"));
        editor.set_cursor(DocumentPosition::at(list_item_path(1), 0));
        editor.indent_list_item().unwrap();
        assert_eq!(leaf_depths(&editor), vec![0, 1]);
        // The tree still round-trips through markdown.
        let reparsed = markdown_to_document(&md(&editor));
        assert_eq!(*editor.document(), reparsed);
    }

    #[test]
    fn indent_then_outdent_restores_flat_list() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b"));
        editor.set_cursor(DocumentPosition::at(list_item_path(1), 0));
        editor.indent_list_item().unwrap();
        assert_eq!(leaf_depths(&editor), vec![0, 1]);
        editor.outdent_list_item().unwrap();
        assert_eq!(leaf_depths(&editor), vec![0, 0]);
        assert_eq!(md(&editor), "- a\n- b");
    }

    #[test]
    fn outdent_nested_item_adopts_following_siblings() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n  - x\n  - y\n  - z"));
        assert_eq!(leaf_depths(&editor), vec![0, 1, 1, 1]);
        // Outdent the first nested item (x).
        let x = TreePath::root(0)
            .child(PathSegment::ListEntry { entry: 0, para: 1 })
            .child(PathSegment::ListEntry { entry: 0, para: 0 });
        editor.set_cursor(DocumentPosition::at(x, 0));
        editor.outdent_list_item().unwrap();
        // x sits beside a; y and z are now nested under x.
        assert_eq!(leaf_depths(&editor), vec![0, 0, 1, 1]);
        let texts: Vec<String> = tree_walk::leaf_paths(editor.document())
            .iter()
            .map(|p| editor.leaf_plain_text(p))
            .collect();
        assert_eq!(texts, vec!["a", "x", "y", "z"]);
        // The cursor follows the outdented item.
        assert_eq!(editor.cursor().path, list_item_path(1));
        // Still round-trips through markdown.
        let reparsed = markdown_to_document(&md(&editor));
        assert_eq!(*editor.document(), reparsed);
    }

    fn leaf_texts(editor: &Editor) -> Vec<String> {
        tree_walk::leaf_paths(editor.document())
            .iter()
            .map(|p| editor.leaf_plain_text(p))
            .collect()
    }

    #[test]
    fn indent_selection_nests_every_selected_item_together() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- x\n- y\n- z"));
        // Select x, y, z (leave a, the first item, out).
        editor.set_selection(
            DocumentPosition::at(list_item_path(1), 0),
            DocumentPosition::at(list_item_path(3), 1),
        );
        editor.indent_list_item().unwrap();
        // All three nest under a, side by side (not a staircase).
        assert_eq!(leaf_depths(&editor), vec![0, 1, 1, 1]);
        assert_eq!(leaf_texts(&editor), vec!["a", "x", "y", "z"]);
        // Selection still covers the three items so Tab can be repeated.
        let (s, e) = editor.selection().expect("selection retained");
        assert_eq!(tree_walk::leaf_paths(editor.document())[1], s.path);
        assert_eq!(tree_walk::leaf_paths(editor.document())[3], e.path);
        // Round-trips through markdown.
        let reparsed = markdown_to_document(&md(&editor));
        assert_eq!(*editor.document(), reparsed);
    }

    #[test]
    fn indent_selection_cannot_indent_first_item() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b"));
        // Select both; "a" is the first item and has no previous sibling to nest under.
        editor.set_selection(
            DocumentPosition::at(list_item_path(0), 0),
            DocumentPosition::at(list_item_path(1), 1),
        );
        editor.indent_list_item().unwrap();
        // a stays put; b nests under it.
        assert_eq!(leaf_depths(&editor), vec![0, 1]);
        assert_eq!(leaf_texts(&editor), vec!["a", "b"]);
    }

    #[test]
    fn indent_then_outdent_selection_round_trips() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- x\n- y"));
        let before = editor.document().clone();
        editor.set_selection(
            DocumentPosition::at(list_item_path(1), 0),
            DocumentPosition::at(list_item_path(2), 1),
        );
        editor.indent_list_item().unwrap();
        assert_eq!(leaf_depths(&editor), vec![0, 1, 1]);
        // The retained selection lets the inverse outdent restore the flat list.
        editor.outdent_list_item().unwrap();
        assert_eq!(leaf_depths(&editor), vec![0, 0, 0]);
        assert_eq!(*editor.document(), before);
    }

    #[test]
    fn outdent_selection_outdents_every_selected_item() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n  - x\n  - y\n  - z"));
        let inner = |entry| {
            TreePath::root(0)
                .child(PathSegment::ListEntry { entry: 0, para: 1 })
                .child(PathSegment::ListEntry { entry, para: 0 })
        };
        // Select x and y (leave z out).
        editor.set_selection(
            DocumentPosition::at(inner(0), 0),
            DocumentPosition::at(inner(1), 1),
        );
        editor.outdent_list_item().unwrap();
        // x and y move up beside a; z (the trailing unselected follower) nests under y.
        assert_eq!(leaf_depths(&editor), vec![0, 0, 0, 1]);
        assert_eq!(leaf_texts(&editor), vec!["a", "x", "y", "z"]);
        // The selection still covers the two outdented items so a second Shift-Tab repeats.
        let (s, e) = editor.selection().expect("selection retained");
        assert_eq!(
            s.path,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 })
        );
        assert_eq!(
            e.path,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 2, para: 0 })
        );
    }

    #[test]
    fn outdent_selection_of_all_nested_items_flattens_them() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n  - x\n  - y\n  - z"));
        let inner = |entry| {
            TreePath::root(0)
                .child(PathSegment::ListEntry { entry: 0, para: 1 })
                .child(PathSegment::ListEntry { entry, para: 0 })
        };
        editor.set_selection(
            DocumentPosition::at(inner(0), 0),
            DocumentPosition::at(inner(2), 1),
        );
        editor.outdent_list_item().unwrap();
        // All three become siblings of a.
        assert_eq!(leaf_depths(&editor), vec![0, 0, 0, 0]);
        assert_eq!(leaf_texts(&editor), vec!["a", "x", "y", "z"]);
        let reparsed = markdown_to_document(&md(&editor));
        assert_eq!(*editor.document(), reparsed);
    }

    #[test]
    fn outdent_top_level_item_exits_list() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- only"));
        editor.set_cursor(DocumentPosition::at(list_item_path(0), 2));
        editor.outdent_list_item().unwrap();
        assert_eq!(md(&editor), "only");
        assert!(matches!(editor.current_block_type(), BlockType::Paragraph));
    }

    #[test]
    fn backspace_at_nested_item_start_outdents() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a\n- b"));
        editor.set_cursor(DocumentPosition::at(list_item_path(1), 0));
        editor.indent_list_item().unwrap(); // b nested under a
        assert_eq!(leaf_depths(&editor), vec![0, 1]);
        // Cursor is on the nested item; Backspace at offset 0 outdents instead of merging.
        editor.delete_backward().unwrap();
        assert_eq!(leaf_depths(&editor), vec![0, 0]);
    }

    #[test]
    fn enter_on_empty_top_item_exits_list() {
        let mut editor = Editor::new();
        editor.set_document(markdown_to_document("- a"));
        editor.set_cursor(DocumentPosition::at(list_item_path(0), 1));
        editor.delete_backward().unwrap(); // delete "a" → empty item
        editor.insert_newline().unwrap(); // empty item + Enter → exit to paragraph
        assert!(matches!(editor.current_block_type(), BlockType::Paragraph));
    }

    #[test]
    fn enter_on_empty_continuation_para_makes_new_item_not_dissolve() {
        // A list item with real content plus an empty trailing (continuation) paragraph:
        // Enter must peel the empty paragraph off into a new item, not dissolve the item.
        let mut editor = Editor::new();
        let text = |s: &str| Paragraph::new_text().with_content(vec![Span::new_text(s)]);
        let mut doc = markdown_to_document("x");
        doc.paragraphs = vec![Paragraph::new_unordered_list().with_entries(vec![vec![
            text("lead"),
            text("second"),
            Paragraph::new_text(), // empty trailing continuation paragraph
        ]])];
        editor.set_document(doc);
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 2 }),
            0,
        ));

        editor.insert_newline().unwrap();
        let Paragraph::UnorderedList { entries } = &editor.document().paragraphs[0] else {
            panic!("the list must survive, not dissolve into paragraphs");
        };
        assert_eq!(entries.len(), 2, "the empty paragraph became a new item");
        assert_eq!(
            entries[0].len(),
            2,
            "the original item keeps its two real paragraphs"
        );
        assert_eq!(
            entries[1].len(),
            1,
            "the new item holds just the empty paragraph"
        );
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 }),
        );
    }

    #[test]
    fn enter_in_empty_nested_para_promotes_one_level_per_press() {
        // The reported nesting: an outer quote holding two paragraphs and a bullet list
        // whose single item holds a lead paragraph, a continuation paragraph, an inner
        // quote, and finally an empty trailing paragraph where the cursor sits.
        let mut editor = Editor::new();
        let text = |s: &str| Paragraph::new_text().with_content(vec![Span::new_text(s)]);
        let list = Paragraph::new_unordered_list().with_entries(vec![vec![
            text("item lead"),
            text("second para"),
            Paragraph::new_quote().with_children(vec![text("inner quote")]),
            Paragraph::new_text(), // empty trailing continuation paragraph
        ]]);
        let mut doc = markdown_to_document("x");
        doc.paragraphs = vec![Paragraph::new_quote().with_children(vec![
            text("quote lead"),
            text("quote second"),
            list,
        ])];
        editor.set_document(doc);

        // Cursor in the empty trailing paragraph: quote child 2 (the list) → entry 0, para 3.
        editor.set_cursor(DocumentPosition::at(
            TreePath::root(0)
                .child(PathSegment::QuoteChild(2))
                .child(PathSegment::ListEntry { entry: 0, para: 3 }),
            0,
        ));

        // Enter #1: empty continuation paragraph → a new (empty) list item; item survives.
        editor.insert_newline().unwrap();
        let Paragraph::Quote { children } = &editor.document().paragraphs[0] else {
            panic!("outer quote should survive");
        };
        assert_eq!(
            children.len(),
            3,
            "quote still holds its two paras + the list"
        );
        let Paragraph::UnorderedList { entries } = &children[2] else {
            panic!("the list should survive, not dissolve");
        };
        assert_eq!(entries.len(), 2, "a new list item was created");
        assert_eq!(
            entries[0].len(),
            3,
            "original item keeps its three real paragraphs"
        );
        assert_eq!(
            entries[1].len(),
            1,
            "the new item holds the single empty paragraph"
        );
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0)
                .child(PathSegment::QuoteChild(2))
                .child(PathSegment::ListEntry { entry: 1, para: 0 }),
        );

        // Enter #2: the empty item exits the list, landing as a quote child after the list.
        editor.insert_newline().unwrap();
        let Paragraph::Quote { children } = &editor.document().paragraphs[0] else {
            panic!("outer quote should survive");
        };
        assert_eq!(
            children.len(),
            4,
            "quote gained the lifted-out empty paragraph"
        );
        let Paragraph::UnorderedList { entries } = &children[2] else {
            panic!("the list remains");
        };
        assert_eq!(entries.len(), 1, "the empty item left the list");
        assert!(
            matches!(children[3], Paragraph::Text { .. }),
            "empty paragraph is now a quote child"
        );
        assert_eq!(
            editor.cursor().path,
            TreePath::root(0).child(PathSegment::QuoteChild(3)),
        );

        // Enter #3: the empty quote child exits the quote to the top level.
        editor.insert_newline().unwrap();
        assert_eq!(
            editor.document().paragraphs.len(),
            2,
            "quote + a top-level paragraph"
        );
        assert!(matches!(
            editor.document().paragraphs[0],
            Paragraph::Quote { .. }
        ));
        assert!(matches!(
            editor.document().paragraphs[1],
            Paragraph::Text { .. }
        ));
        assert_eq!(editor.cursor().path, TreePath::root(1));
    }

    #[test]
    fn bold_inside_link_styles_link_content() {
        let mut e = Editor::new();
        e.set_document(markdown_to_document("a [manual](u) b"));
        // select "anu" inside the link ("a " = 0..2, "manual" = 2..8)
        e.set_selection(
            DocumentPosition::at(TreePath::root(0), 3),
            DocumentPosition::at(TreePath::root(0), 6),
        );
        e.toggle_bold().unwrap();
        assert_eq!(md(&e), "a [m**anu**al](u) b");
    }

    #[test]
    fn wrap_selection_in_link_preserves_styles() {
        let mut e = Editor::new();
        e.set_document(markdown_to_document("hello world"));
        e.set_selection(
            DocumentPosition::at(TreePath::root(0), 0),
            DocumentPosition::at(TreePath::root(0), 5),
        );
        e.toggle_bold().unwrap(); // **hello** world
        e.set_selection(
            DocumentPosition::at(TreePath::root(0), 0),
            DocumentPosition::at(TreePath::root(0), 5),
        );
        e.wrap_selection_in_link("u").unwrap();
        assert_eq!(md(&e), "[**hello**](u) world");
    }

    #[test]
    fn wrap_selection_in_link_flattens_inner_links() {
        let mut e = Editor::new();
        e.set_document(markdown_to_document("a [b](v) c"));
        // select the whole paragraph and wrap in a new link
        e.set_selection(
            DocumentPosition::at(TreePath::root(0), 0),
            DocumentPosition::at(TreePath::root(0), 5),
        );
        e.wrap_selection_in_link("u").unwrap();
        // No nested links: the inner link is flattened, one outer link.
        let runs = super::super::tree_walk::leaf_inline(e.document(), &TreePath::root(0));
        fn has_nested(items: &[InlineContent]) -> bool {
            items.iter().any(|it| match it {
                InlineContent::Link { content, .. } => content
                    .iter()
                    .any(|c| matches!(c, InlineContent::Link { .. })),
                _ => false,
            })
        }
        assert!(!has_nested(&runs), "links must not nest: {:?}", runs);
        assert_eq!(md(&e), "[a b c](u)");
    }

    #[test]
    fn image_in_link_flattens_and_is_stylable() {
        let mut e = Editor::new();
        e.set_document(markdown_to_document(
            "[![Build Status](https://x/badge.svg)](https://x/actions)",
        ));
        let runs = super::super::tree_walk::leaf_inline(e.document(), &TreePath::root(0));
        assert_eq!(runs.len(), 1, "one flat link: {:?}", runs);
        match &runs[0] {
            InlineContent::Link { link, content } => {
                assert_eq!(
                    link.destination, "https://x/actions",
                    "outer link target kept"
                );
                assert!(
                    content.iter().all(|c| matches!(c, InlineContent::Text(_))),
                    "link content must be plain runs, no nested link: {:?}",
                    content
                );
            }
            other => panic!("expected a single link, got {:?}", other),
        }
        // Highlight "Status" (6..12) inside the link — applies cleanly, link intact.
        e.set_selection(
            DocumentPosition::at(TreePath::root(0), 6),
            DocumentPosition::at(TreePath::root(0), 12),
        );
        e.toggle_highlight().unwrap();
        assert_eq!(md(&e), "[Build <mark>Status</mark>](https://x/actions)");
    }
}
