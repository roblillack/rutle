// Traversal of a `tdoc::Document` as a sequence of leaves in document order.
//
// The tdoc tree is authoritative; this module is the read/navigate layer over it. It
// enumerates leaves (paragraphs, headings, code blocks, list-item paragraphs, checklist
// items, definition terms and definition paragraphs, read-only tables, and horizontal
// rules) in the order they render, computing for each its
// `TreePath`, intrinsic kind, list marker, and nesting depths. It also resolves a path
// back to the leaf's inline spans (immutably and mutably) and provides
// previous/next/first/last navigation that replaces the old flat `block_index ± 1`.

use tdoc::Document;
use tdoc::inline::Span;
use tdoc::paragraph::{ChecklistItem, DefinitionItem, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use super::inline_convert::{inline_to_spans, spans_to_inline};
use super::structured_document::{BlockType, InlineContent, TableCell, TableRow};
use super::tree_path::{DocumentPosition, PathSegment, TreePath};

/// The intrinsic block kind of a leaf (independent of any list marker around it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParaKind {
    Paragraph,
    Heading(u8),
    CodeBlock,
    /// A read-only table leaf; carries no editable spans.
    Table,
    /// A horizontal rule / thematic break; carries no spans at all.
    HorizontalRule,
    /// A definition list's term (`<dt>`); an editable inline-content leaf.
    DefinitionTerm,
}

/// Marker shown at the start of a leaf that begins a list/checklist entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListMarker {
    pub ordered: bool,
    /// 1-based ordinal for ordered lists (per nesting level); `None` for unordered.
    pub ordinal: Option<u64>,
    /// `Some` for checklist items; the checked state.
    pub checkbox: Option<bool>,
}

/// Everything the display/editor need about one leaf, plus where it lives in the tree.
#[derive(Debug, Clone, PartialEq)]
pub struct LeafInfo {
    pub path: TreePath,
    pub kind: ParaKind,
    /// `Some` when this leaf is the first paragraph of a list/checklist entry.
    pub marker: Option<ListMarker>,
    /// List/checklist indentation depth (0 = top-level item or not in a list).
    pub depth: usize,
    /// Number of enclosing lists/checklists (0 = not in any list). Unlike `depth`, this
    /// distinguishes a top-level paragraph (0) from a continuation paragraph or code block
    /// nested inside a top-level list item (1), which both have `depth == 0`.
    pub list_levels: usize,
    /// Number of enclosing block quotes (0 = not quoted).
    pub quote_depth: usize,
    /// Number of enclosing definition *bodies* (`<dd>`); 0 outside a definition list.
    /// A term is not inside the body it heads, so a top-level term is 0 and that item's
    /// definition paragraphs are 1 — which is exactly the indentation step that sets a
    /// definition off from its term.
    pub definition_depth: usize,
}

/// Enumerate every leaf in document order.
pub fn enumerate_leaves(doc: &Document) -> Vec<LeafInfo> {
    let mut out = Vec::new();
    for (i, para) in doc.paragraphs.iter().enumerate() {
        walk_para(para, TreePath::root(i), 0, 0, 0, None, &mut out);
    }
    out
}

/// Just the leaf paths, in document order.
pub fn leaf_paths(doc: &Document) -> Vec<TreePath> {
    enumerate_leaves(doc).into_iter().map(|l| l.path).collect()
}

fn walk_para(
    para: &Paragraph,
    path: TreePath,
    list_depth: usize,
    quote_depth: usize,
    def_depth: usize,
    marker: Option<ListMarker>,
    out: &mut Vec<LeafInfo>,
) {
    let mut leaf = |kind| {
        push_leaf(
            out,
            path.clone(),
            kind,
            marker.clone(),
            list_depth,
            quote_depth,
            def_depth,
        )
    };
    match para {
        Paragraph::Text { .. } => leaf(ParaKind::Paragraph),
        Paragraph::Header1 { .. } => leaf(ParaKind::Heading(1)),
        Paragraph::Header2 { .. } => leaf(ParaKind::Heading(2)),
        Paragraph::Header3 { .. } => leaf(ParaKind::Heading(3)),
        Paragraph::CodeBlock { .. } => leaf(ParaKind::CodeBlock),
        Paragraph::Table { .. } => leaf(ParaKind::Table),
        Paragraph::HorizontalRule => leaf(ParaKind::HorizontalRule),
        Paragraph::Quote { children } => {
            for (c, child) in children.iter().enumerate() {
                walk_para(
                    child,
                    path.child(PathSegment::QuoteChild(c)),
                    list_depth,
                    quote_depth + 1,
                    def_depth,
                    None,
                    out,
                );
            }
        }
        Paragraph::OrderedList { entries } => walk_list(
            entries,
            &path,
            true,
            list_depth,
            quote_depth,
            def_depth,
            out,
        ),
        Paragraph::UnorderedList { entries } => walk_list(
            entries,
            &path,
            false,
            list_depth,
            quote_depth,
            def_depth,
            out,
        ),
        Paragraph::Checklist { items } => {
            walk_checklist(items, &path, list_depth, quote_depth, def_depth, out)
        }
        Paragraph::DefinitionList { items } => {
            walk_definition_list(items, &path, list_depth, quote_depth, def_depth, out)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_list(
    entries: &[Vec<Paragraph>],
    path: &TreePath,
    ordered: bool,
    list_depth: usize,
    quote_depth: usize,
    def_depth: usize,
    out: &mut Vec<LeafInfo>,
) {
    for (e, entry) in entries.iter().enumerate() {
        for (pi, para) in entry.iter().enumerate() {
            let marker = if pi == 0 {
                Some(ListMarker {
                    ordered,
                    ordinal: ordered.then_some((e + 1) as u64),
                    checkbox: None,
                })
            } else {
                None
            };
            walk_para(
                para,
                path.child(PathSegment::ListEntry { entry: e, para: pi }),
                list_depth + 1,
                quote_depth,
                def_depth,
                marker,
                out,
            );
        }
    }
}

fn walk_checklist(
    items: &[ChecklistItem],
    path: &TreePath,
    list_depth: usize,
    quote_depth: usize,
    def_depth: usize,
    out: &mut Vec<LeafInfo>,
) {
    for (i, item) in items.iter().enumerate() {
        let item_path = path.child(PathSegment::ChecklistItem(i));
        push_leaf(
            out,
            item_path.clone(),
            ParaKind::Paragraph,
            Some(ListMarker {
                ordered: false,
                ordinal: None,
                checkbox: Some(item.checked),
            }),
            list_depth + 1,
            quote_depth,
            def_depth,
        );
        if !item.children.is_empty() {
            walk_checklist(
                &item.children,
                &item_path,
                list_depth + 1,
                quote_depth,
                def_depth,
                out,
            );
        }
    }
}

/// Enumerate a definition list: each item contributes its terms (leaves in their own
/// right, since a term owns inline content) followed by the paragraphs of its
/// definition, which are walked as ordinary blocks one `def_depth` step deeper.
///
/// A definition list is not a list: its items carry no bullet or ordinal, so no
/// `ListMarker` is produced and `list_depth` passes through untouched.
fn walk_definition_list(
    items: &[DefinitionItem],
    path: &TreePath,
    list_depth: usize,
    quote_depth: usize,
    def_depth: usize,
    out: &mut Vec<LeafInfo>,
) {
    for (i, item) in items.iter().enumerate() {
        for t in 0..item.terms.len() {
            push_leaf(
                out,
                path.child(PathSegment::DefinitionTerm { item: i, term: t }),
                ParaKind::DefinitionTerm,
                None,
                list_depth,
                quote_depth,
                def_depth,
            );
        }
        for (p, para) in item.definition.iter().enumerate() {
            walk_para(
                para,
                path.child(PathSegment::DefinitionPara { item: i, para: p }),
                list_depth,
                quote_depth,
                def_depth + 1,
                None,
                out,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_leaf(
    out: &mut Vec<LeafInfo>,
    path: TreePath,
    kind: ParaKind,
    marker: Option<ListMarker>,
    list_depth: usize,
    quote_depth: usize,
    definition_depth: usize,
) {
    out.push(LeafInfo {
        path,
        kind,
        marker,
        depth: list_depth.saturating_sub(1),
        list_levels: list_depth,
        quote_depth,
        definition_depth,
    });
}

// ---- Path resolution --------------------------------------------------------------

/// A reference to a resolved leaf node — a `Paragraph`, a `ChecklistItem`, or a
/// definition list's term (which owns its inline spans directly).
enum LeafRef<'a> {
    Para(&'a Paragraph),
    Check(&'a ChecklistItem),
    Term(&'a [Span]),
}

fn resolve<'a>(doc: &'a Document, path: &TreePath) -> Option<LeafRef<'a>> {
    let mut segs = path.0.iter();
    let PathSegment::Paragraph(i) = segs.next()? else {
        return None;
    };
    enum Cur<'a> {
        Para(&'a Paragraph),
        Check(&'a ChecklistItem),
        Term(&'a [Span]),
    }
    let mut cur = Cur::Para(doc.paragraphs.get(*i)?);
    for seg in segs {
        cur = match (cur, seg) {
            (Cur::Para(Paragraph::Quote { children }), PathSegment::QuoteChild(c)) => {
                Cur::Para(children.get(*c)?)
            }
            (
                Cur::Para(
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                ),
                PathSegment::ListEntry { entry, para },
            ) => Cur::Para(entries.get(*entry)?.get(*para)?),
            (Cur::Para(Paragraph::Checklist { items }), PathSegment::ChecklistItem(c)) => {
                Cur::Check(items.get(*c)?)
            }
            (Cur::Check(item), PathSegment::ChecklistItem(c)) => Cur::Check(item.children.get(*c)?),
            (
                Cur::Para(Paragraph::DefinitionList { items }),
                PathSegment::DefinitionTerm { item, term },
            ) => Cur::Term(items.get(*item)?.terms.get(*term)?),
            (
                Cur::Para(Paragraph::DefinitionList { items }),
                PathSegment::DefinitionPara { item, para },
            ) => Cur::Para(items.get(*item)?.definition.get(*para)?),
            // A term is a leaf: no segment may follow it.
            _ => return None,
        };
    }
    Some(match cur {
        Cur::Para(p) => LeafRef::Para(p),
        Cur::Check(c) => LeafRef::Check(c),
        Cur::Term(t) => LeafRef::Term(t),
    })
}

/// The inline spans of the leaf at `path`, or `None` for tables, horizontal rules
/// and invalid paths.
///
/// `None` is what marks a leaf *non-editable* across the engine (splits, merges and
/// inline edits all bail on it), so a rule must answer `None` rather than the empty
/// slice `tdoc`'s `Paragraph::content()` hands back for it — an empty slice would
/// read as an ordinary empty paragraph.
pub fn leaf_spans<'a>(doc: &'a Document, path: &TreePath) -> Option<&'a [Span]> {
    match resolve(doc, path)? {
        LeafRef::Para(Paragraph::Table { .. } | Paragraph::HorizontalRule) => None,
        LeafRef::Para(p) => Some(p.content()),
        LeafRef::Check(item) => Some(&item.content),
        LeafRef::Term(spans) => Some(spans),
    }
}

/// Mutable inline spans of the leaf at `path`, or `None` for tables, horizontal
/// rules and invalid paths.
pub fn leaf_spans_mut<'a>(doc: &'a mut Document, path: &TreePath) -> Option<&'a mut Vec<Span>> {
    let mut segs = path.0.iter();
    let PathSegment::Paragraph(i) = segs.next()? else {
        return None;
    };
    enum Cur<'a> {
        Para(&'a mut Paragraph),
        Check(&'a mut ChecklistItem),
        Term(&'a mut Vec<Span>),
    }
    let mut cur = Cur::Para(doc.paragraphs.get_mut(*i)?);
    for seg in segs {
        cur = match (cur, seg) {
            (Cur::Para(Paragraph::Quote { children }), PathSegment::QuoteChild(c)) => {
                Cur::Para(children.get_mut(*c)?)
            }
            (
                Cur::Para(
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                ),
                PathSegment::ListEntry { entry, para },
            ) => Cur::Para(entries.get_mut(*entry)?.get_mut(*para)?),
            (Cur::Para(Paragraph::Checklist { items }), PathSegment::ChecklistItem(c)) => {
                Cur::Check(items.get_mut(*c)?)
            }
            (Cur::Check(item), PathSegment::ChecklistItem(c)) => {
                Cur::Check(item.children.get_mut(*c)?)
            }
            (
                Cur::Para(Paragraph::DefinitionList { items }),
                PathSegment::DefinitionTerm { item, term },
            ) => Cur::Term(items.get_mut(*item)?.terms.get_mut(*term)?),
            (
                Cur::Para(Paragraph::DefinitionList { items }),
                PathSegment::DefinitionPara { item, para },
            ) => Cur::Para(items.get_mut(*item)?.definition.get_mut(*para)?),
            _ => return None,
        };
    }
    match cur {
        // Only leaf paragraph types own inline content; `content_mut` panics otherwise.
        Cur::Para(p) => match p {
            Paragraph::Text { content }
            | Paragraph::Header1 { content }
            | Paragraph::Header2 { content }
            | Paragraph::Header3 { content }
            | Paragraph::CodeBlock { content } => Some(content),
            _ => None,
        },
        Cur::Check(item) => Some(&mut item.content),
        Cur::Term(spans) => Some(spans),
    }
}

// ---- Navigation -------------------------------------------------------------------

pub fn first_leaf_path(doc: &Document) -> Option<TreePath> {
    leaf_paths(doc).into_iter().next()
}

pub fn last_leaf_path(doc: &Document) -> Option<TreePath> {
    leaf_paths(doc).into_iter().last()
}

pub fn next_leaf_path(doc: &Document, path: &TreePath) -> Option<TreePath> {
    let paths = leaf_paths(doc);
    let idx = paths.iter().position(|p| p == path)?;
    paths.into_iter().nth(idx + 1)
}

pub fn prev_leaf_path(doc: &Document, path: &TreePath) -> Option<TreePath> {
    let paths = leaf_paths(doc);
    let idx = paths.iter().position(|p| p == path)?;
    if idx == 0 {
        None
    } else {
        paths.into_iter().nth(idx - 1)
    }
}

/// Number of leaves in the document (the path-model analogue of block count).
pub fn leaf_count(doc: &Document) -> usize {
    enumerate_leaves(doc).len()
}

/// The leaf's inline content as flat runs (empty for tables / invalid paths).
pub fn leaf_inline(doc: &Document, path: &TreePath) -> Vec<InlineContent> {
    leaf_spans(doc, path)
        .map(spans_to_inline)
        .unwrap_or_default()
}

/// Replace the leaf's inline content (converting runs back to spans). Returns `false`
/// for tables / invalid paths (which own no editable spans), leaving the tree unchanged.
pub fn set_leaf_inline(doc: &mut Document, path: &TreePath, content: &[InlineContent]) -> bool {
    if let Some(spans) = leaf_spans_mut(doc, path) {
        *spans = inline_to_spans(content);
        true
    } else {
        false
    }
}

/// Build the presentation `BlockType` for a leaf (the transient descriptor the display
/// and menus consume). Resolves table rows when the leaf is a table.
pub fn leaf_block_type(doc: &Document, info: &LeafInfo) -> BlockType {
    // A rule outranks any list marker around it. A `ListItem` block carries its
    // content inline, and a rule has none, so reporting one as a list item would
    // render an empty bullet and drop the rule entirely; losing the bullet is the
    // lesser of the two.
    if info.kind != ParaKind::HorizontalRule
        && let Some(marker) = &info.marker
    {
        return BlockType::ListItem {
            ordered: marker.ordered,
            number: marker.ordinal,
            checkbox: marker.checkbox,
            depth: info.depth,
        };
    }
    match &info.kind {
        ParaKind::Paragraph => {
            if info.quote_depth > 0 {
                BlockType::BlockQuote
            } else {
                BlockType::Paragraph
            }
        }
        ParaKind::Heading(level) => BlockType::Heading { level: *level },
        ParaKind::CodeBlock => BlockType::CodeBlock { language: None },
        ParaKind::Table => BlockType::Table {
            rows: table_rows_at(doc, &info.path),
        },
        ParaKind::HorizontalRule => BlockType::HorizontalRule,
        ParaKind::DefinitionTerm => BlockType::DefinitionTerm {
            depth: info.definition_depth,
        },
    }
}

// ---- Pseudo-leaf / breadcrumb -----------------------------------------------------
//
// The "pseudo-leaf" (effective block type) is the block type the ESC+number menu acts
// on and the status bar shows as the rightmost crumb. A *paragraph-holding level* (a
// quote node, a list item's paragraph vec, or a checklist item) that holds a single
// text paragraph behaves like a leaf of the level's own type — so `Quote{[Text]}` is a
// "Quote" block, and any single-text list item is a "Bullet/Numbered List" block. This
// is per-item for lists, independent of how many items the list has.

/// One enclosing container level along a path, outermost-first.
struct Frame {
    /// The container-kind block type for this level (`BlockQuote` / `ListItem{..}` /
    /// `DefinitionTerm{..}`).
    block: BlockType,
    /// Number of paragraphs held by *this specific level's item* (a quote's children, a
    /// list entry's paragraphs; 1 for a checklist item). Drives the single-text collapse.
    para_count: usize,
    /// Whether a lone text child at this level collapses onto the container. A quote or
    /// list item is a single unit that a block-type change can act on wholesale, so those
    /// collapse; a definition *body* does not, because its container also holds terms —
    /// retyping a one-paragraph definition must not silently retype the whole list.
    collapsible: bool,
}

fn list_item_block(ordered: bool, checkbox: Option<bool>) -> BlockType {
    BlockType::ListItem {
        ordered,
        number: None,
        checkbox,
        depth: 0,
    }
}

/// The block type that labels a definition list as a *container* (breadcrumb, "select
/// parent" menu). Depth is a leaf property, so the label carries 0 — the same
/// simplification [`list_item_block`] makes for a list's ordinal.
fn definition_list_block() -> BlockType {
    BlockType::DefinitionTerm { depth: 0 }
}

/// Intrinsic block type of a leaf paragraph, ignoring any container it sits in.
fn para_intrinsic_block_type(doc: &Document, path: &TreePath, p: &Paragraph) -> BlockType {
    match p {
        Paragraph::Text { .. } => BlockType::Paragraph,
        Paragraph::Header1 { .. } => BlockType::Heading { level: 1 },
        Paragraph::Header2 { .. } => BlockType::Heading { level: 2 },
        Paragraph::Header3 { .. } => BlockType::Heading { level: 3 },
        Paragraph::CodeBlock { .. } => BlockType::CodeBlock { language: None },
        Paragraph::Table { .. } => BlockType::Table {
            rows: table_rows_at(doc, path),
        },
        Paragraph::HorizontalRule => BlockType::HorizontalRule,
        // Container nodes are never leaves; fall back to a plain paragraph.
        _ => BlockType::Paragraph,
    }
}

/// Walk `path`, collecting the enclosing container levels (outermost-first) plus the
/// leaf's intrinsic block type and whether the leaf is a plain text paragraph. Returns
/// `None` for an invalid path.
fn analyze_path(doc: &Document, path: &TreePath) -> Option<(Vec<Frame>, BlockType, bool)> {
    let mut segs = path.0.iter();
    let PathSegment::Paragraph(i) = segs.next()? else {
        return None;
    };
    enum Cur<'a> {
        Para(&'a Paragraph),
        Check(&'a ChecklistItem),
        Term,
    }
    let mut cur = Cur::Para(doc.paragraphs.get(*i)?);
    let mut frames: Vec<Frame> = Vec::new();
    // How many definition bodies deep we are, mirroring `LeafInfo::definition_depth`.
    let mut def_depth = 0usize;
    for seg in segs {
        cur = match (cur, seg) {
            (Cur::Para(Paragraph::Quote { children }), PathSegment::QuoteChild(c)) => {
                frames.push(Frame {
                    block: BlockType::BlockQuote,
                    para_count: children.len(),
                    collapsible: true,
                });
                Cur::Para(children.get(*c)?)
            }
            (
                Cur::Para(Paragraph::OrderedList { entries }),
                PathSegment::ListEntry { entry, para },
            ) => {
                let e = entries.get(*entry)?;
                frames.push(Frame {
                    block: list_item_block(true, None),
                    para_count: e.len(),
                    collapsible: true,
                });
                Cur::Para(e.get(*para)?)
            }
            (
                Cur::Para(Paragraph::UnorderedList { entries }),
                PathSegment::ListEntry { entry, para },
            ) => {
                let e = entries.get(*entry)?;
                frames.push(Frame {
                    block: list_item_block(false, None),
                    para_count: e.len(),
                    collapsible: true,
                });
                Cur::Para(e.get(*para)?)
            }
            (Cur::Para(Paragraph::Checklist { items }), PathSegment::ChecklistItem(c)) => {
                let it = items.get(*c)?;
                frames.push(Frame {
                    block: list_item_block(false, Some(it.checked)),
                    para_count: 1,
                    collapsible: true,
                });
                Cur::Check(it)
            }
            (Cur::Check(item), PathSegment::ChecklistItem(c)) => {
                let it = item.children.get(*c)?;
                frames.push(Frame {
                    block: list_item_block(false, Some(it.checked)),
                    para_count: 1,
                    collapsible: true,
                });
                Cur::Check(it)
            }
            // A term *is* the definition-list crumb, so it adds no frame of its own —
            // otherwise the breadcrumb would say "Definition List > Definition List".
            (
                Cur::Para(Paragraph::DefinitionList { items }),
                PathSegment::DefinitionTerm { item, term },
            ) => {
                items.get(*item)?.terms.get(*term)?;
                Cur::Term
            }
            (
                Cur::Para(Paragraph::DefinitionList { items }),
                PathSegment::DefinitionPara { item, para },
            ) => {
                let it = items.get(*item)?;
                frames.push(Frame {
                    block: BlockType::DefinitionTerm { depth: def_depth },
                    para_count: it.definition.len(),
                    collapsible: false,
                });
                def_depth += 1;
                Cur::Para(it.definition.get(*para)?)
            }
            _ => return None,
        };
    }
    let (leaf, leaf_is_text) = match cur {
        Cur::Para(p) => (
            para_intrinsic_block_type(doc, path, p),
            matches!(p, Paragraph::Text { .. }),
        ),
        // A checklist item's content is inline text; it always collapses, so this leaf
        // block type is only a placeholder that the collapse path never uses.
        Cur::Check(_) => (list_item_block(false, Some(false)), true),
        // A term carries inline text but is not a *paragraph*, so it never lets an
        // enclosing level collapse onto it.
        Cur::Term => (BlockType::DefinitionTerm { depth: def_depth }, false),
    };
    Some((frames, leaf, leaf_is_text))
}

/// Whether the cursor's innermost paragraph-holding level is a single-text collapse
/// (behaves like a leaf of the container's type).
fn is_collapse(frames: &[Frame], leaf_is_text: bool) -> bool {
    frames
        .last()
        .is_some_and(|f| f.collapsible && f.para_count == 1 && leaf_is_text)
}

/// The effective (pseudo-leaf) block type at `path`: the type the ESC+number menu will
/// change. A single-text container level collapses to its own kind; otherwise the leaf's
/// own type; a top-level leaf is unchanged.
pub fn effective_block_type(doc: &Document, path: &TreePath) -> BlockType {
    let Some((frames, leaf, leaf_is_text)) = analyze_path(doc, path) else {
        return BlockType::Paragraph;
    };
    if is_collapse(&frames, leaf_is_text) {
        frames.last().unwrap().block.clone()
    } else {
        leaf
    }
}

/// The ancestor container chain for the status-bar breadcrumb, outermost first, with the
/// pseudo-leaf last: a collapsed single-text level yields its own crumb only; a genuine
/// leaf yields the container crumbs followed by the leaf's own type.
pub fn block_breadcrumb(doc: &Document, path: &TreePath) -> Vec<BlockType> {
    let Some((frames, leaf, leaf_is_text)) = analyze_path(doc, path) else {
        return vec![BlockType::Paragraph];
    };
    if frames.is_empty() {
        return vec![leaf];
    }
    let collapse = is_collapse(&frames, leaf_is_text);
    let mut crumbs: Vec<BlockType> = frames.into_iter().map(|f| f.block).collect();
    if !collapse {
        crumbs.push(leaf);
    }
    crumbs
}

/// The container-kind block type of the node at `path` (which must point at a
/// quote/list/checklist node), for labelling the "select parent" menu. `None` if the node
/// is not a container.
pub fn container_block_at(doc: &Document, path: &TreePath) -> Option<BlockType> {
    match resolve(doc, path)? {
        LeafRef::Para(Paragraph::Quote { .. }) => Some(BlockType::BlockQuote),
        LeafRef::Para(Paragraph::OrderedList { .. }) => Some(list_item_block(true, None)),
        LeafRef::Para(Paragraph::UnorderedList { .. }) => Some(list_item_block(false, None)),
        LeafRef::Para(Paragraph::Checklist { .. }) => Some(list_item_block(false, Some(false))),
        LeafRef::Para(Paragraph::DefinitionList { .. }) => Some(definition_list_block()),
        _ => None,
    }
}

/// Whether the cursor's innermost level is a collapsed single-text container (used to
/// route pseudo-leaf block-type changes onto the container).
pub fn cursor_in_collapsed_container(doc: &Document, path: &TreePath) -> bool {
    let Some((frames, _, leaf_is_text)) = analyze_path(doc, path) else {
        return false;
    };
    !frames.is_empty() && is_collapse(&frames, leaf_is_text)
}

fn table_rows_at(doc: &Document, path: &TreePath) -> Vec<TableRow> {
    let Some(LeafRef::Para(Paragraph::Table { rows })) = resolve(doc, path) else {
        return Vec::new();
    };
    rows.iter()
        .map(|row| {
            TableRow::new(
                row.cells
                    .iter()
                    .map(|cell| TableCell::new(cell.is_header, spans_to_inline(&cell.content)))
                    .collect(),
            )
        })
        .collect()
}

// ---- Plain text & offsets ---------------------------------------------------------

fn span_plain_text(span: &Span, out: &mut String) {
    out.push_str(&span.text);
    for child in &span.children {
        span_plain_text(child, out);
    }
}

/// The flattened plain text of the leaf at `path` (empty for tables / invalid paths).
/// Byte offsets in a `DocumentPosition` index into this string.
pub fn leaf_plain_text(doc: &Document, path: &TreePath) -> String {
    let Some(spans) = leaf_spans(doc, path) else {
        return String::new();
    };
    let mut text = String::new();
    for span in spans {
        span_plain_text(span, &mut text);
    }
    text
}

/// Byte length of the leaf's flattened plain text.
pub fn leaf_text_len(doc: &Document, path: &TreePath) -> usize {
    leaf_plain_text(doc, path).len()
}

// ---- Position clamping & grapheme navigation --------------------------------------

/// Resolve a (possibly stale) path to a valid leaf path, snapping to the nearest
/// existing leaf in document order. Returns `None` only for an empty document.
fn nearest_leaf_path(doc: &Document, path: &TreePath) -> Option<TreePath> {
    let paths = leaf_paths(doc);
    if paths.iter().any(|p| p == path) {
        return Some(path.clone());
    }
    // Snap to the last leaf whose path is <= the target, else the first leaf.
    paths
        .iter()
        .rev()
        .find(|p| *p <= path)
        .or_else(|| paths.first())
        .cloned()
}

/// Clamp a position to a valid leaf and a grapheme boundary at or before its offset.
pub fn clamp_position(doc: &Document, pos: &DocumentPosition) -> DocumentPosition {
    let Some(path) = nearest_leaf_path(doc, &pos.path) else {
        return DocumentPosition::start();
    };
    let text = leaf_plain_text(doc, &path);
    let offset = grapheme_offset_at_or_before(&text, pos.offset);
    DocumentPosition::at(path, offset)
}

/// Clamp a position to a valid leaf and a grapheme boundary at or after its offset.
pub fn clamp_position_forward(doc: &Document, pos: &DocumentPosition) -> DocumentPosition {
    let Some(path) = nearest_leaf_path(doc, &pos.path) else {
        return DocumentPosition::start();
    };
    let text = leaf_plain_text(doc, &path);
    let offset = grapheme_offset_at_or_after(&text, pos.offset);
    DocumentPosition::at(path, offset)
}

/// Previous grapheme boundary within the same leaf (does not cross leaves).
pub fn previous_grapheme_position(doc: &Document, pos: &DocumentPosition) -> DocumentPosition {
    let Some(path) = nearest_leaf_path(doc, &pos.path) else {
        return DocumentPosition::start();
    };
    let text = leaf_plain_text(doc, &path);
    let offset = grapheme_offset_before(&text, pos.offset);
    DocumentPosition::at(path, offset)
}

/// Next grapheme boundary within the same leaf (does not cross leaves).
pub fn next_grapheme_position(doc: &Document, pos: &DocumentPosition) -> DocumentPosition {
    let Some(path) = nearest_leaf_path(doc, &pos.path) else {
        return DocumentPosition::start();
    };
    let text = leaf_plain_text(doc, &path);
    let offset = grapheme_offset_after(&text, pos.offset);
    DocumentPosition::at(path, offset)
}

fn grapheme_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries: Vec<usize> = text.grapheme_indices(true).map(|(idx, _)| idx).collect();
    if boundaries.is_empty() {
        boundaries.push(0);
        return boundaries;
    }
    if boundaries[0] != 0 {
        boundaries.insert(0, 0);
    }
    if *boundaries.last().unwrap() != text.len() {
        boundaries.push(text.len());
    }
    boundaries
}

fn grapheme_offset_at_or_before(text: &str, offset: usize) -> usize {
    let boundaries = grapheme_boundaries(text);
    let mut result = 0usize;
    let max_offset = offset.min(text.len());
    for boundary in boundaries {
        if boundary > max_offset {
            break;
        }
        result = boundary;
    }
    result
}

fn grapheme_offset_at_or_after(text: &str, offset: usize) -> usize {
    let boundaries = grapheme_boundaries(text);
    let max_offset = offset.min(text.len());
    for boundary in boundaries {
        if boundary >= max_offset {
            return boundary;
        }
    }
    text.len()
}

fn grapheme_offset_before(text: &str, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let boundaries = grapheme_boundaries(text);
    let mut previous = 0usize;
    let max_offset = offset.min(text.len());
    for boundary in boundaries {
        if boundary >= max_offset {
            if boundary == max_offset {
                return previous;
            }
            break;
        }
        previous = boundary;
    }
    previous
}

fn grapheme_offset_after(text: &str, offset: usize) -> usize {
    let boundaries = grapheme_boundaries(text);
    let max_offset = offset.min(text.len());
    for boundary in boundaries {
        if boundary > max_offset {
            return boundary;
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(md: &str) -> Document {
        tdoc::markdown::parse(&mut Cursor::new(md.as_bytes())).expect("parse")
    }

    // ----- Pseudo-leaf / breadcrumb -----

    #[test]
    fn effective_type_single_text_quote_collapses() {
        let doc = parse("> quoted");
        let path = TreePath::root(0).child(PathSegment::QuoteChild(0));
        assert_eq!(effective_block_type(&doc, &path), BlockType::BlockQuote);
        assert!(cursor_in_collapsed_container(&doc, &path));
    }

    #[test]
    fn effective_type_single_text_bullet_collapses_per_item() {
        // A single-text item in a multi-item list still behaves like a leaf.
        let doc = parse("- a\n- b\n- c");
        let path = TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 });
        assert!(matches!(
            effective_block_type(&doc, &path),
            BlockType::ListItem {
                ordered: false,
                checkbox: None,
                ..
            }
        ));
        assert!(cursor_in_collapsed_container(&doc, &path));
    }

    #[test]
    fn effective_type_multi_paragraph_quote_reports_leaf() {
        let mut doc = parse("x");
        doc.paragraphs = vec![Paragraph::new_quote().with_children(vec![
            Paragraph::Text {
                content: vec![Span::new_text("a")],
            },
            Paragraph::Header2 {
                content: vec![Span::new_text("b")],
            },
        ])];
        let path = TreePath::root(0).child(PathSegment::QuoteChild(1));
        assert_eq!(
            effective_block_type(&doc, &path),
            BlockType::Heading { level: 2 }
        );
        assert!(!cursor_in_collapsed_container(&doc, &path));
        assert_eq!(
            block_breadcrumb(&doc, &path),
            vec![BlockType::BlockQuote, BlockType::Heading { level: 2 }]
        );
    }

    #[test]
    fn breadcrumb_top_level_and_collapsed_quote() {
        let doc = parse("plain");
        assert_eq!(
            block_breadcrumb(&doc, &TreePath::root(0)),
            vec![BlockType::Paragraph]
        );
        let doc = parse("> quoted");
        let path = TreePath::root(0).child(PathSegment::QuoteChild(0));
        assert_eq!(block_breadcrumb(&doc, &path), vec![BlockType::BlockQuote]);
    }

    #[test]
    fn enumerates_top_level_paragraphs() {
        let doc = parse("First\n\nSecond\n\n# Heading");
        let leaves = enumerate_leaves(&doc);
        assert_eq!(leaves.len(), 3);
        assert_eq!(leaves[0].kind, ParaKind::Paragraph);
        assert_eq!(leaves[2].kind, ParaKind::Heading(1));
        assert!(leaves.iter().all(|l| l.marker.is_none() && l.depth == 0));
    }

    #[test]
    fn nested_unordered_list_depths_and_paths() {
        let doc = parse("- a\n    - b\n- c");
        let leaves = enumerate_leaves(&doc);
        let texts: Vec<_> = leaves
            .iter()
            .map(|l| (leaf_text(&doc, &l.path), l.depth, l.marker.is_some()))
            .collect();
        assert_eq!(
            texts,
            vec![
                ("a".to_string(), 0, true),
                ("b".to_string(), 1, true),
                ("c".to_string(), 0, true),
            ]
        );
    }

    #[test]
    fn ordered_list_ordinals_per_level() {
        let doc = parse("1. one\n2. two\n    1. nested-one\n    2. nested-two\n3. three");
        let leaves = enumerate_leaves(&doc);
        let ords: Vec<_> = leaves
            .iter()
            .map(|l| l.marker.as_ref().and_then(|m| m.ordinal))
            .collect();
        // Top level 1,2 then nested 1,2 then top 3.
        assert_eq!(ords, vec![Some(1), Some(2), Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn checklist_items_carry_checkbox() {
        let doc = parse("- [ ] todo\n- [x] done");
        let leaves = enumerate_leaves(&doc);
        let checks: Vec<_> = leaves
            .iter()
            .map(|l| l.marker.as_ref().and_then(|m| m.checkbox))
            .collect();
        assert_eq!(checks, vec![Some(false), Some(true)]);
    }

    #[test]
    fn quote_children_carry_quote_depth() {
        let doc = parse("> quoted line");
        let leaves = enumerate_leaves(&doc);
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].quote_depth, 1);
    }

    #[test]
    fn leaf_paths_are_in_sorted_order() {
        let doc = parse("- a\n    - b\n- c\n\nAfter");
        let paths = leaf_paths(&doc);
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(
            paths, sorted,
            "enumeration must already be in document order"
        );
    }

    #[test]
    fn navigation_round_trips() {
        let doc = parse("- a\n    - b\n- c");
        let paths = leaf_paths(&doc);
        assert_eq!(first_leaf_path(&doc).as_ref(), Some(&paths[0]));
        assert_eq!(last_leaf_path(&doc).as_ref(), Some(&paths[2]));
        assert_eq!(next_leaf_path(&doc, &paths[0]).as_ref(), Some(&paths[1]));
        assert_eq!(prev_leaf_path(&doc, &paths[1]).as_ref(), Some(&paths[0]));
        assert_eq!(prev_leaf_path(&doc, &paths[0]), None);
        assert_eq!(next_leaf_path(&doc, &paths[2]), None);
    }

    #[test]
    fn horizontal_rule_is_a_contentless_leaf() {
        let doc = parse("A\n\n---\n\nB");
        let leaves = enumerate_leaves(&doc);
        assert_eq!(
            leaves.iter().map(|l| l.kind.clone()).collect::<Vec<_>>(),
            vec![
                ParaKind::Paragraph,
                ParaKind::HorizontalRule,
                ParaKind::Paragraph
            ]
        );

        let rule = &leaves[1];
        // `None`, not `Some(&[])`: that is what marks the leaf non-editable.
        assert_eq!(leaf_spans(&doc, &rule.path), None);
        assert_eq!(leaf_text_len(&doc, &rule.path), 0);
        assert!(leaf_inline(&doc, &rule.path).is_empty());
        assert_eq!(leaf_block_type(&doc, rule), BlockType::HorizontalRule);
        assert_eq!(
            effective_block_type(&doc, &rule.path),
            BlockType::HorizontalRule
        );
        assert!(!cursor_in_collapsed_container(&doc, &rule.path));
    }

    #[test]
    fn quoted_rule_reports_itself_not_the_quote() {
        // A rule is never plain text, so a single-rule quote must not collapse to
        // a "Quote" pseudo-leaf the way a single-text one does.
        let mut doc = parse("x");
        doc.paragraphs =
            vec![Paragraph::new_quote().with_children(vec![Paragraph::new_horizontal_rule()])];
        let path = TreePath::root(0).child(PathSegment::QuoteChild(0));
        assert_eq!(effective_block_type(&doc, &path), BlockType::HorizontalRule);
        assert_eq!(
            block_breadcrumb(&doc, &path),
            vec![BlockType::BlockQuote, BlockType::HorizontalRule]
        );
    }

    #[test]
    fn rule_in_a_list_entry_outranks_the_bullet() {
        // A `ListItem` block carries its content inline and a rule has none, so
        // reporting the marker would render an empty bullet and drop the rule.
        let mut doc = parse("x");
        doc.paragraphs = vec![
            Paragraph::new_unordered_list()
                .with_entries(vec![vec![Paragraph::new_horizontal_rule()]]),
        ];
        let leaves = enumerate_leaves(&doc);
        assert_eq!(leaves.len(), 1);
        assert!(leaves[0].marker.is_some(), "the tree still says list item");
        assert_eq!(leaf_block_type(&doc, &leaves[0]), BlockType::HorizontalRule);
    }

    // ----- Definition lists -----

    /// Two items: `Coffee` with one definition, `Water` with two definition
    /// paragraphs (Markdown's repeated `:` form).
    fn definition_doc() -> Document {
        parse("Coffee\n: Black hot drink\n\nWater\n: The plain one\n: Also the wet one\n")
    }

    #[test]
    fn definition_list_enumerates_terms_then_definitions() {
        let doc = definition_doc();
        assert!(
            matches!(
                doc.paragraphs.as_slice(),
                [Paragraph::DefinitionList { .. }]
            ),
            "expected one definition list: {doc:?}"
        );
        let leaves = enumerate_leaves(&doc);
        let seen: Vec<_> = leaves
            .iter()
            .map(|l| (leaf_text(&doc, &l.path), l.kind.clone(), l.definition_depth))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("Coffee".into(), ParaKind::DefinitionTerm, 0),
                ("Black hot drink".into(), ParaKind::Paragraph, 1),
                ("Water".into(), ParaKind::DefinitionTerm, 0),
                ("The plain one".into(), ParaKind::Paragraph, 1),
                ("Also the wet one".into(), ParaKind::Paragraph, 1),
            ]
        );
        // A definition list is not a list: no markers, no list depth.
        assert!(
            leaves
                .iter()
                .all(|l| l.marker.is_none() && l.list_levels == 0)
        );
    }

    #[test]
    fn several_terms_share_one_definition() {
        // Markdown folds consecutive `<dt>` lines into a single term, so a
        // multi-term item comes from HTML (`<dt>Tea</dt><dt>Chai</dt><dd>…`).
        // Each term is its own leaf and they all precede the shared definition.
        let mut doc = parse("x");
        doc.paragraphs = vec![Paragraph::DefinitionList {
            items: vec![
                DefinitionItem::new()
                    .with_terms(vec![
                        vec![Span::new_text("Tea")],
                        vec![Span::new_text("Chai")],
                    ])
                    .with_definition(vec![Paragraph::Text {
                        content: vec![Span::new_text("One leaf, two names")],
                    }]),
            ],
        }];
        let leaves = enumerate_leaves(&doc);
        let seen: Vec<_> = leaves
            .iter()
            .map(|l| (leaf_text(&doc, &l.path), l.kind.clone()))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("Tea".into(), ParaKind::DefinitionTerm),
                ("Chai".into(), ParaKind::DefinitionTerm),
                ("One leaf, two names".into(), ParaKind::Paragraph),
            ]
        );
        let paths = leaf_paths(&doc);
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "terms must sort ahead of their definition");
    }

    #[test]
    fn definition_leaf_paths_are_in_document_order() {
        // Terms and definition paragraphs are siblings under one item but use
        // different path segments, so their ordering is the tie-break `order_key`
        // encodes rather than a plain index comparison.
        let doc = definition_doc();
        let paths = leaf_paths(&doc);
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(
            paths, sorted,
            "enumeration must already be in document order"
        );
    }

    #[test]
    fn definition_terms_are_editable_leaves() {
        let mut doc = definition_doc();
        let term = TreePath::root(0).child(PathSegment::DefinitionTerm { item: 0, term: 0 });
        // A term owns real spans, unlike a rule or a table.
        assert!(leaf_spans(&doc, &term).is_some());
        leaf_spans_mut(&mut doc, &term)
            .expect("term spans")
            .push(Span::new_text(" beans"));
        assert_eq!(leaf_text(&doc, &term), "Coffee beans");
    }

    #[test]
    fn definition_block_types_and_breadcrumb() {
        let doc = definition_doc();
        let leaves = enumerate_leaves(&doc);
        let term = &leaves[0];
        let body = &leaves[1];

        assert_eq!(
            leaf_block_type(&doc, term),
            BlockType::DefinitionTerm { depth: 0 }
        );
        assert_eq!(leaf_block_type(&doc, body), BlockType::Paragraph);

        // A term is the definition-list crumb itself, so it stands alone.
        assert_eq!(
            block_breadcrumb(&doc, &term.path),
            vec![BlockType::DefinitionTerm { depth: 0 }]
        );
        // A definition's paragraph reports itself *inside* the list — a lone-text
        // definition must not collapse onto the container the way a quote does,
        // or retyping one paragraph would retype the whole list.
        assert_eq!(
            block_breadcrumb(&doc, &body.path),
            vec![BlockType::DefinitionTerm { depth: 0 }, BlockType::Paragraph]
        );
        assert_eq!(effective_block_type(&doc, &body.path), BlockType::Paragraph);
        assert!(!cursor_in_collapsed_container(&doc, &body.path));

        assert_eq!(
            container_block_at(&doc, &TreePath::root(0)),
            Some(BlockType::DefinitionTerm { depth: 0 })
        );
    }

    #[test]
    fn definition_body_carries_its_own_depth() {
        // A definition list nested inside a definition indents one further step.
        let doc = parse("Outer\n: Inner\n  : Innermost\n");
        let leaves = enumerate_leaves(&doc);
        let depths: Vec<_> = leaves
            .iter()
            .map(|l| (leaf_text(&doc, &l.path), l.definition_depth))
            .collect();
        assert_eq!(
            depths,
            vec![
                ("Outer".to_string(), 0),
                ("Inner".to_string(), 1),
                ("Innermost".to_string(), 2),
            ],
            "a nested definition list indents past the one holding it"
        );
    }

    #[test]
    fn a_list_inside_a_definition_carries_both_depths() {
        // Tab pulls a following list into a definition, so its items must indent for the
        // definition they are in *and* carry their list marker level.
        let mut doc = parse("Coffee\n: drink\n");
        if let Some(Paragraph::DefinitionList { items }) = doc.paragraphs.get_mut(0) {
            items[0]
                .definition
                .push(Paragraph::new_unordered_list().with_entries(vec![vec![
                    Paragraph::new_text().with_content(vec![Span::new_text("beans")]),
                ]]));
        }
        let leaves = enumerate_leaves(&doc);
        let item = leaves
            .iter()
            .find(|l| leaf_text(&doc, &l.path) == "beans")
            .expect("the nested item");
        assert_eq!(item.definition_depth, 1, "indents for the definition");
        assert_eq!(item.list_levels, 1, "and still carries its bullet");
        assert!(item.marker.is_some());
    }

    #[test]
    fn leaf_spans_mut_edits_tree() {
        let mut doc = parse("hello");
        let path = TreePath::root(0);
        let spans = leaf_spans_mut(&mut doc, &path).expect("spans");
        spans.push(Span::new_text(" world"));
        assert_eq!(leaf_text(&doc, &path), "hello world");
    }

    fn leaf_text(doc: &Document, path: &TreePath) -> String {
        leaf_spans(doc, path)
            .map(|spans| spans.iter().map(span_text).collect::<String>())
            .unwrap_or_default()
    }

    fn span_text(span: &Span) -> String {
        let mut s = span.text.clone();
        for child in &span.children {
            s.push_str(&span_text(child));
        }
        s
    }
}
