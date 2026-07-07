// Structural mutations of the `tdoc::Document` tree.
//
// `tree_walk` reads/navigates the tree; this module performs the surgery that editing
// requires: splitting a leaf on Enter, merging a leaf into its predecessor on Backspace,
// removing a node (pruning emptied containers), and toggling a checklist item. All take a
// leaf `TreePath` and operate relative to it, returning the resulting cursor location.

use tdoc::Document;
use tdoc::inline::Span;
use tdoc::paragraph::{ChecklistItem, Paragraph};

use super::inline_convert::inline_to_spans;
use super::structured_document::{Block, InlineContent};
use super::tree_path::{PathSegment, TreePath};
use super::tree_walk;

/// A mutable reference to a resolved tree node.
enum NodeMut<'a> {
    Para(&'a mut Paragraph),
    Check(&'a mut ChecklistItem),
}

/// Descend to the node at `path` (a `Paragraph` or a `ChecklistItem`).
fn node_at_mut<'a>(doc: &'a mut Document, path: &TreePath) -> Option<NodeMut<'a>> {
    let mut segs = path.0.iter();
    let PathSegment::Paragraph(i) = segs.next()? else {
        return None;
    };
    let mut cur = NodeMut::Para(doc.paragraphs.get_mut(*i)?);
    for seg in segs {
        cur = match (cur, seg) {
            (NodeMut::Para(Paragraph::Quote { children }), PathSegment::QuoteChild(c)) => {
                NodeMut::Para(children.get_mut(*c)?)
            }
            (
                NodeMut::Para(
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                ),
                PathSegment::ListEntry { entry, para },
            ) => NodeMut::Para(entries.get_mut(*entry)?.get_mut(*para)?),
            (NodeMut::Para(Paragraph::Checklist { items }), PathSegment::ChecklistItem(c)) => {
                NodeMut::Check(items.get_mut(*c)?)
            }
            (NodeMut::Check(item), PathSegment::ChecklistItem(c)) => {
                NodeMut::Check(item.children.get_mut(*c)?)
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// A shared reference to a resolved tree node (immutable counterpart of [`NodeMut`]).
enum NodeRef<'a> {
    Para(&'a Paragraph),
    Check(&'a ChecklistItem),
}

/// Descend to the node at `path` (a `Paragraph` or a `ChecklistItem`), read-only.
fn node_at<'a>(doc: &'a Document, path: &TreePath) -> Option<NodeRef<'a>> {
    let mut segs = path.0.iter();
    let PathSegment::Paragraph(i) = segs.next()? else {
        return None;
    };
    let mut cur = NodeRef::Para(doc.paragraphs.get(*i)?);
    for seg in segs {
        cur = match (cur, seg) {
            (NodeRef::Para(Paragraph::Quote { children }), PathSegment::QuoteChild(c)) => {
                NodeRef::Para(children.get(*c)?)
            }
            (
                NodeRef::Para(
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                ),
                PathSegment::ListEntry { entry, para },
            ) => NodeRef::Para(entries.get(*entry)?.get(*para)?),
            (NodeRef::Para(Paragraph::Checklist { items }), PathSegment::ChecklistItem(c)) => {
                NodeRef::Check(items.get(*c)?)
            }
            (NodeRef::Check(item), PathSegment::ChecklistItem(c)) => {
                NodeRef::Check(item.children.get(*c)?)
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn parent_path(path: &TreePath) -> TreePath {
    TreePath(path.0[..path.0.len().saturating_sub(1)].to_vec())
}

/// A new paragraph carrying `runs`, used as the trailing half of a split leaf. A split that
/// carries text into the new paragraph keeps the original block type (splitting a heading
/// mid-text yields two headings). An *empty* continuation — Enter pressed at the end of the
/// leaf — instead starts a plain paragraph, so pressing Enter after a heading drops you into
/// normal body text. Non-leaf paragraphs (lists, quotes, tables) have no inline content and
/// also fall back to plain text.
fn same_kind_paragraph(like: &Paragraph, runs: &[InlineContent]) -> Paragraph {
    let spans = inline_to_spans(runs);
    if runs.iter().all(|c| c.text_len() == 0) {
        return Paragraph::new_text().with_content(spans);
    }
    match like {
        Paragraph::Header1 { .. } => Paragraph::new_header1().with_content(spans),
        Paragraph::Header2 { .. } => Paragraph::new_header2().with_content(spans),
        Paragraph::Header3 { .. } => Paragraph::new_header3().with_content(spans),
        Paragraph::CodeBlock { .. } => Paragraph::new_code_block().with_content(spans),
        _ => Paragraph::new_text().with_content(spans),
    }
}

/// Split `content` at flattened byte `offset`, returning `(left, right)`.
fn split_runs(
    content: &[InlineContent],
    offset: usize,
) -> (Vec<InlineContent>, Vec<InlineContent>) {
    let mut block = Block::paragraph();
    block.content = content.to_vec();
    let right = block.split_content_at(offset);
    (block.content, right)
}

/// Replace the leaf paragraph at `path` with `make(spans)`, preserving its inline spans
/// and its position in the tree (in-place variant change, e.g. Text → Header). Returns
/// `false` for checklist items / invalid paths.
pub fn replace_leaf_variant(
    doc: &mut Document,
    path: &TreePath,
    make: impl FnOnce(Vec<Span>) -> Paragraph,
) -> bool {
    let spans = match node_at_mut(doc, path) {
        Some(NodeMut::Para(p)) => p.content().to_vec(),
        _ => return false,
    };
    match node_at_mut(doc, path) {
        Some(NodeMut::Para(p)) => {
            *p = make(spans);
            true
        }
        _ => false,
    }
}

/// Toggle the checked state of the checklist item at `path`. Returns the new state, or
/// `None` if the path is not a checklist item.
pub fn toggle_checkmark(doc: &mut Document, path: &TreePath) -> Option<bool> {
    match node_at_mut(doc, path)? {
        NodeMut::Check(item) => {
            item.checked = !item.checked;
            Some(item.checked)
        }
        NodeMut::Para(_) => None,
    }
}

/// Split the leaf at `path` at byte `offset` into a sibling that follows it (a new
/// paragraph, list entry, quote child, or checklist item depending on context). Returns
/// the path of the new (right-hand) leaf, or `None` for tables / invalid paths.
pub fn split_leaf(doc: &mut Document, path: &TreePath, offset: usize) -> Option<TreePath> {
    // Only editable leaves (not tables) can be split.
    let runs = tree_walk::leaf_spans(doc, path).map(|_| tree_walk::leaf_inline(doc, path))?;
    let (left, right) = split_runs(&runs, offset);
    tree_walk::set_leaf_inline(doc, path, &left);

    let last = path.0.last()?.clone();
    let pp = parent_path(path);
    match last {
        PathSegment::Paragraph(i) => {
            // Both halves keep the original block type — splitting a heading (at any
            // offset, including the start) yields two headings, never a demoted paragraph.
            let new = same_kind_paragraph(doc.paragraphs.get(i)?, &right);
            doc.paragraphs.insert(i + 1, new);
            Some(TreePath::root(i + 1))
        }
        PathSegment::QuoteChild(c) => {
            if let NodeMut::Para(Paragraph::Quote { children }) = node_at_mut(doc, &pp)? {
                let new = same_kind_paragraph(children.get(c)?, &right);
                children.insert(c + 1, new);
                Some(pp.child(PathSegment::QuoteChild(c + 1)))
            } else {
                None
            }
        }
        PathSegment::ListEntry { entry, para } => {
            if let NodeMut::Para(
                Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
            ) = node_at_mut(doc, &pp)?
            {
                // The new entry takes the right half plus any continuation paragraphs of
                // the original entry; its first paragraph keeps the split leaf's kind.
                let continuation = if para < entries.get(entry)?.len() {
                    entries[entry].split_off(para + 1)
                } else {
                    Vec::new()
                };
                let new_para = same_kind_paragraph(entries.get(entry)?.get(para)?, &right);
                let mut new_entry = vec![new_para];
                new_entry.extend(continuation);
                entries.insert(entry + 1, new_entry);
                Some(pp.child(PathSegment::ListEntry {
                    entry: entry + 1,
                    para: 0,
                }))
            } else {
                None
            }
        }
        PathSegment::ChecklistItem(c) => {
            let new_item = ChecklistItem::new(false).with_content(inline_to_spans(&right));
            match node_at_mut(doc, &pp)? {
                NodeMut::Para(Paragraph::Checklist { items }) => {
                    items.insert(c + 1, new_item);
                }
                NodeMut::Check(item) => {
                    item.children.insert(c + 1, new_item);
                }
                _ => return None,
            }
            Some(pp.child(PathSegment::ChecklistItem(c + 1)))
        }
    }
}

/// Like [`split_leaf`], but inside a list item the right half becomes a *continuation
/// paragraph* within the same entry (a new plain paragraph after the current one) rather
/// than starting a new list item. Elsewhere — top level, quotes, checklists — it behaves
/// exactly like `split_leaf` (a quote already splits into a sibling paragraph).
pub fn split_leaf_continuation(
    doc: &mut Document,
    path: &TreePath,
    offset: usize,
) -> Option<TreePath> {
    let PathSegment::ListEntry { entry, para } = path.0.last()?.clone() else {
        return split_leaf(doc, path, offset);
    };
    // Reject tables / invalid leaves, then split the inline runs.
    tree_walk::leaf_spans(doc, path)?;
    let runs = tree_walk::leaf_inline(doc, path);
    let (left, right) = split_runs(&runs, offset);
    tree_walk::set_leaf_inline(doc, path, &left);

    let pp = parent_path(path);
    if let NodeMut::Para(
        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
    ) = node_at_mut(doc, &pp)?
    {
        let e = entries.get_mut(entry)?;
        e.insert(
            para + 1,
            Paragraph::new_text().with_content(inline_to_spans(&right)),
        );
        Some(pp.child(PathSegment::ListEntry {
            entry,
            para: para + 1,
        }))
    } else {
        None
    }
}

/// Split the list entry containing `path` at that paragraph boundary: the paragraph at
/// `path` — which must be a *continuation* paragraph, not the entry's first — together with
/// any paragraphs after it move into a new entry inserted immediately after the current one.
/// So an empty trailing paragraph becomes a fresh (empty) list item rather than dissolving
/// the whole item. Returns the new entry's first-paragraph path, or `None` if `path` is the
/// entry's leading paragraph (`para == 0`) or is not a list entry at all.
pub fn split_list_entry(doc: &mut Document, path: &TreePath) -> Option<TreePath> {
    let PathSegment::ListEntry { entry, para } = path.0.last()?.clone() else {
        return None;
    };
    if para == 0 {
        return None;
    }
    let pp = parent_path(path);
    if let NodeMut::Para(
        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
    ) = node_at_mut(doc, &pp)?
    {
        let e = entries.get_mut(entry)?;
        if para >= e.len() {
            return None;
        }
        let new_entry = e.split_off(para);
        entries.insert(entry + 1, new_entry);
        Some(pp.child(PathSegment::ListEntry {
            entry: entry + 1,
            para: 0,
        }))
    } else {
        None
    }
}

/// Merge the leaf at `path` into the previous leaf in document order (appending its text).
/// Returns the resulting cursor position (the join point), or `None` when there is no
/// previous leaf or either leaf is a table.
pub fn merge_with_previous(doc: &mut Document, path: &TreePath) -> Option<(TreePath, usize)> {
    let prev = tree_walk::prev_leaf_path(doc, path)?;
    // Both leaves must be editable (tables have no text to merge).
    tree_walk::leaf_spans(doc, &prev)?;
    tree_walk::leaf_spans(doc, path)?;

    let prev_len = tree_walk::leaf_text_len(doc, &prev);
    let cur_runs = tree_walk::leaf_inline(doc, path);

    let mut block = Block::paragraph();
    block.content = tree_walk::leaf_inline(doc, &prev);
    block.content.extend(cur_runs);
    block.normalize_content();
    tree_walk::set_leaf_inline(doc, &prev, &block.content);

    remove_node_at(doc, path);
    Some((prev, prev_len))
}

/// Remove the node at `path` from its parent container, pruning containers that become
/// empty as a result (e.g. removing a list's last entry removes the list).
pub fn remove_node_at(doc: &mut Document, path: &TreePath) {
    let Some(last) = path.0.last().cloned() else {
        return;
    };
    let pp = parent_path(path);
    match last {
        PathSegment::Paragraph(i) => {
            if i < doc.paragraphs.len() {
                doc.paragraphs.remove(i);
            }
        }
        PathSegment::QuoteChild(c) => {
            let mut empty = false;
            if let Some(NodeMut::Para(Paragraph::Quote { children })) = node_at_mut(doc, &pp) {
                if c < children.len() {
                    children.remove(c);
                }
                empty = children.is_empty();
            }
            if empty {
                remove_node_at(doc, &pp);
            }
        }
        PathSegment::ListEntry { entry, para } => {
            let mut empty = false;
            if let Some(NodeMut::Para(
                Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
            )) = node_at_mut(doc, &pp)
            {
                if entry < entries.len() {
                    if para < entries[entry].len() {
                        entries[entry].remove(para);
                    }
                    if entries[entry].is_empty() {
                        entries.remove(entry);
                    }
                }
                empty = entries.is_empty();
            }
            if empty {
                remove_node_at(doc, &pp);
            }
        }
        PathSegment::ChecklistItem(c) => {
            let mut empty = false;
            match node_at_mut(doc, &pp) {
                Some(NodeMut::Para(Paragraph::Checklist { items })) => {
                    if c < items.len() {
                        items.remove(c);
                    }
                    empty = items.is_empty();
                }
                Some(NodeMut::Check(item)) if c < item.children.len() => {
                    item.children.remove(c);
                }
                _ => {}
            }
            if empty {
                remove_node_at(doc, &pp);
            }
        }
    }
}

fn list_ordered(p: &Paragraph) -> Option<bool> {
    match p {
        Paragraph::OrderedList { .. } => Some(true),
        Paragraph::UnorderedList { .. } => Some(false),
        _ => None,
    }
}

fn new_list(ordered: bool, entries: Vec<Vec<Paragraph>>) -> Paragraph {
    if ordered {
        Paragraph::new_ordered_list().with_entries(entries)
    } else {
        Paragraph::new_unordered_list().with_entries(entries)
    }
}

/// Append list `children` as a nested same-kind sublist of `entry`. If `entry` already ends
/// with a sublist of the matching kind, the children are merged into it instead.
fn append_children(entry: &mut Vec<Paragraph>, ordered: bool, children: Vec<Vec<Paragraph>>) {
    if let Some(last) = entry.last_mut()
        && list_ordered(last) == Some(ordered)
        && let Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } = last
    {
        entries.extend(children);
        return;
    }
    entry.push(new_list(ordered, children));
}

/// The kind of an editable list container.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListKind {
    Ordered,
    Unordered,
    Checklist,
}

impl ListKind {
    pub fn from_flags(ordered: bool, checklist: bool) -> ListKind {
        if checklist {
            ListKind::Checklist
        } else if ordered {
            ListKind::Ordered
        } else {
            ListKind::Unordered
        }
    }
}

/// Immutably descend to the `Paragraph` at `path` (through quotes and lists). Returns
/// `None` if the path leaves the paragraph tree (e.g. into a checklist item).
fn para_at<'a>(doc: &'a Document, path: &TreePath) -> Option<&'a Paragraph> {
    let mut segs = path.0.iter();
    let PathSegment::Paragraph(i) = segs.next()? else {
        return None;
    };
    let mut cur = doc.paragraphs.get(*i)?;
    for seg in segs {
        cur = match (cur, seg) {
            (Paragraph::Quote { children }, PathSegment::QuoteChild(c)) => children.get(*c)?,
            (
                Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                PathSegment::ListEntry { entry, para },
            ) => entries.get(*entry)?.get(*para)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// The kind of the list/checklist that *directly* contains the leaf at `path`, or `None`
/// if the leaf is not a list/checklist item.
pub fn containing_list_kind(doc: &Document, path: &TreePath) -> Option<ListKind> {
    match path.0.last()? {
        // A checklist item nests via `ChecklistItem.children`, so its container is always a
        // checklist regardless of whether the parent node is the checklist or another item.
        PathSegment::ChecklistItem(_) => Some(ListKind::Checklist),
        PathSegment::ListEntry { .. } => match para_at(doc, &parent_path(path))? {
            Paragraph::OrderedList { .. } => Some(ListKind::Ordered),
            Paragraph::UnorderedList { .. } => Some(ListKind::Unordered),
            _ => None,
        },
        _ => None,
    }
}

/// The list kind of a top-level paragraph *node* (ordered / unordered / checklist), or `None`
/// for any other paragraph. Unlike [`containing_list_kind`], this classifies the node itself
/// rather than a leaf's container.
pub fn list_node_kind(p: &Paragraph) -> Option<ListKind> {
    match p {
        Paragraph::OrderedList { .. } => Some(ListKind::Ordered),
        Paragraph::UnorderedList { .. } => Some(ListKind::Unordered),
        Paragraph::Checklist { .. } => Some(ListKind::Checklist),
        _ => None,
    }
}

/// Fold a run of top-level paragraphs into a single list/checklist node of `target` kind.
/// Existing list/checklist nodes contribute their items (remapped to the target kind); every
/// other paragraph becomes one item, flattened to plain text (dropping heading/code styling) —
/// the same flattening a single-paragraph list toggle performs.
pub fn paragraphs_into_list(paragraphs: Vec<Paragraph>, target: ListKind) -> Paragraph {
    match target {
        ListKind::Checklist => {
            let mut items: Vec<ChecklistItem> = Vec::new();
            for p in paragraphs {
                match p {
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
                        items.extend(entries.into_iter().map(entry_to_checklist_item));
                    }
                    Paragraph::Checklist { items: its } => items.extend(its),
                    other => {
                        items.push(ChecklistItem::new(false).with_content(other.content().to_vec()))
                    }
                }
            }
            Paragraph::new_checklist().with_checklist_items(items)
        }
        ListKind::Ordered | ListKind::Unordered => {
            let ordered = target == ListKind::Ordered;
            let mut entries: Vec<Vec<Paragraph>> = Vec::new();
            for p in paragraphs {
                match p {
                    Paragraph::OrderedList { entries: es }
                    | Paragraph::UnorderedList { entries: es } => entries.extend(es),
                    Paragraph::Checklist { items } => entries.extend(
                        items
                            .into_iter()
                            .map(|it| checklist_item_to_entry(it, ordered)),
                    ),
                    other => entries.push(vec![
                        Paragraph::new_text().with_content(other.content().to_vec()),
                    ]),
                }
            }
            new_list(ordered, entries)
        }
    }
}

/// Expand a run of top-level paragraphs, replacing every list/checklist node with its items as
/// plain paragraphs (an entry's first paragraph loses its bullet; continuation paragraphs and
/// nested sublists are lifted out alongside it). Non-list paragraphs pass through unchanged.
/// The inverse of [`paragraphs_into_list`]; mirrors how a single list is unwrapped.
pub fn lists_into_paragraphs(paragraphs: Vec<Paragraph>) -> Vec<Paragraph> {
    let mut out: Vec<Paragraph> = Vec::new();
    for p in paragraphs {
        match p {
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
                for entry in entries {
                    let mut paras = entry.into_iter();
                    if let Some(first) = paras.next() {
                        out.push(Paragraph::new_text().with_content(first.content().to_vec()));
                    }
                    out.extend(paras);
                }
            }
            Paragraph::Checklist { items } => {
                for item in items {
                    out.push(Paragraph::new_text().with_content(item.content));
                    if !item.children.is_empty() {
                        out.push(Paragraph::new_checklist().with_checklist_items(item.children));
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Convert one ordered/unordered list entry into a checklist item: the entry's first
/// paragraph supplies the item text, and any continuation paragraphs / nested sublists
/// become nested checklist children.
fn entry_to_checklist_item(entry: Vec<Paragraph>) -> ChecklistItem {
    let mut paras = entry.into_iter();
    let content = paras
        .next()
        .map(|p| p.content().to_vec())
        .unwrap_or_default();
    let mut item = ChecklistItem::new(false).with_content(content);
    for p in paras {
        match p {
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
                item.children
                    .extend(entries.into_iter().map(entry_to_checklist_item));
            }
            Paragraph::Checklist { items } => item.children.extend(items),
            other => item
                .children
                .push(ChecklistItem::new(false).with_content(other.content().to_vec())),
        }
    }
    item
}

/// Convert a checklist item into an ordered/unordered list entry: the item text becomes
/// the entry's paragraph and any nested children become a nested list of the same kind.
fn checklist_item_to_entry(item: ChecklistItem, ordered: bool) -> Vec<Paragraph> {
    let mut entry = vec![Paragraph::new_text().with_content(item.content)];
    if !item.children.is_empty() {
        let sub = item
            .children
            .into_iter()
            .map(|c| checklist_item_to_entry(c, ordered))
            .collect();
        entry.push(new_list(ordered, sub));
    }
    entry
}

/// Change the kind of the list that *directly* contains the leaf at `path` to `target`,
/// preserving the surrounding nesting. Ordered↔unordered is an in-place variant swap;
/// to/from a checklist re-shapes the entries. Returns the leaf's new path, or `None` if
/// the leaf is not a list item (or the conversion has no representation — e.g. a checklist
/// nested inside another checklist item cannot become an ordered list).
pub fn change_list_kind(doc: &mut Document, path: &TreePath, target: ListKind) -> Option<TreePath> {
    let last = path.0.last()?.clone();
    let list_path = parent_path(path);
    match last {
        PathSegment::ListEntry { entry, .. } => match target {
            ListKind::Ordered | ListKind::Unordered => {
                let want_ordered = target == ListKind::Ordered;
                let entries = take_list_entries(doc, &list_path)?;
                set_node(doc, &list_path, new_list(want_ordered, entries))?;
                Some(path.clone())
            }
            ListKind::Checklist => {
                let entries = take_list_entries(doc, &list_path)?;
                let items = entries.into_iter().map(entry_to_checklist_item).collect();
                set_node(
                    doc,
                    &list_path,
                    Paragraph::new_checklist().with_checklist_items(items),
                )?;
                Some(list_path.child(PathSegment::ChecklistItem(entry)))
            }
        },
        PathSegment::ChecklistItem(c) => match target {
            ListKind::Checklist => Some(path.clone()),
            ListKind::Ordered | ListKind::Unordered => {
                let want_ordered = target == ListKind::Ordered;
                // Only a checklist held by a `Paragraph` node (top-level or nested as a
                // sublist) can become an ordered/unordered list; one nested inside another
                // checklist item lives in a `Vec<ChecklistItem>` with no list node to swap.
                let items = match node_at_mut(doc, &list_path)? {
                    NodeMut::Para(Paragraph::Checklist { items }) => std::mem::take(items),
                    _ => return None,
                };
                let entries = items
                    .into_iter()
                    .map(|it| checklist_item_to_entry(it, want_ordered))
                    .collect();
                set_node(doc, &list_path, new_list(want_ordered, entries))?;
                Some(list_path.child(PathSegment::ListEntry { entry: c, para: 0 }))
            }
        },
        _ => None,
    }
}

/// Carve items `[start, end]` out of the list/checklist at `list_path` into a sub-list of
/// kind `target`, splitting the original list into up to three siblings in its container
/// (before / converted / after); the unselected halves keep the original kind. The item
/// representation is remapped as needed (entries ↔ checklist items). Preserves leaf order and
/// count, so a caller can restore cursor/selection by flat leaf index. Returns the path of
/// the first converted item, or `None` if `list_path` is not a list `Paragraph` node, the
/// range is degenerate or covers the whole list, or `target` already matches the list's kind.
pub fn convert_list_item_range(
    doc: &mut Document,
    list_path: &TreePath,
    start: usize,
    end: usize,
    target: ListKind,
) -> Option<TreePath> {
    let current = match node_at_mut(doc, list_path)? {
        NodeMut::Para(p) => list_like_kind(p)?,
        _ => return None,
    };
    if current == target {
        return None;
    }
    let len = match node_at_mut(doc, list_path)? {
        NodeMut::Para(
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
        ) => entries.len(),
        NodeMut::Para(Paragraph::Checklist { items }) => items.len(),
        _ => return None,
    };
    // Only a *partial* range splits the list; a whole-list selection is a plain conversion,
    // left to the caller's normal path.
    if start > end || end >= len || (start == 0 && end + 1 == len) {
        return None;
    }

    let mut replacement: Vec<Paragraph> = Vec::new();
    match current {
        ListKind::Ordered | ListKind::Unordered => {
            let src_ordered = current == ListKind::Ordered;
            let mut all = match node_at_mut(doc, list_path)? {
                NodeMut::Para(
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                ) => std::mem::take(entries),
                _ => return None,
            };
            let after = all.split_off(end + 1);
            let middle = all.split_off(start);
            let before = all;
            if !before.is_empty() {
                replacement.push(new_list(src_ordered, before));
            }
            replacement.push(match target {
                ListKind::Ordered => new_list(true, middle),
                ListKind::Unordered => new_list(false, middle),
                ListKind::Checklist => Paragraph::new_checklist().with_checklist_items(
                    middle.into_iter().map(entry_to_checklist_item).collect(),
                ),
            });
            if !after.is_empty() {
                replacement.push(new_list(src_ordered, after));
            }
        }
        ListKind::Checklist => {
            let mut all = match node_at_mut(doc, list_path)? {
                NodeMut::Para(Paragraph::Checklist { items }) => std::mem::take(items),
                _ => return None,
            };
            let after = all.split_off(end + 1);
            let middle = all.split_off(start);
            let before = all;
            if !before.is_empty() {
                replacement.push(Paragraph::new_checklist().with_checklist_items(before));
            }
            // current is a checklist and differs from target, so target is ordered/unordered.
            let want_ordered = target == ListKind::Ordered;
            replacement.push(new_list(
                want_ordered,
                middle
                    .into_iter()
                    .map(|it| checklist_item_to_entry(it, want_ordered))
                    .collect(),
            ));
            if !after.is_empty() {
                replacement.push(Paragraph::new_checklist().with_checklist_items(after));
            }
        }
    }

    let base = container_splice(doc, list_path, replacement)?;
    let middle_idx = base + usize::from(start > 0);
    let first_item = match target {
        ListKind::Checklist => PathSegment::ChecklistItem(0),
        _ => PathSegment::ListEntry { entry: 0, para: 0 },
    };
    Some(container_child_path(list_path, middle_idx).child(first_item))
}

/// Take (leaving empty) the entries of the ordered/unordered list at `path`.
fn take_list_entries(doc: &mut Document, path: &TreePath) -> Option<Vec<Vec<Paragraph>>> {
    match node_at_mut(doc, path)? {
        NodeMut::Para(
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
        ) => Some(std::mem::take(entries)),
        _ => None,
    }
}

/// Replace the whole `Paragraph` node at `path` with `replacement`.
fn set_node(doc: &mut Document, path: &TreePath, replacement: Paragraph) -> Option<()> {
    match node_at_mut(doc, path)? {
        NodeMut::Para(p) => {
            *p = replacement;
            Some(())
        }
        _ => None,
    }
}

/// The path of the `idx`-th sibling in the container that holds the node at `node_path`.
fn container_child_path(node_path: &TreePath, idx: usize) -> TreePath {
    match node_path.0.last() {
        Some(PathSegment::QuoteChild(_)) => {
            parent_path(node_path).child(PathSegment::QuoteChild(idx))
        }
        Some(PathSegment::ListEntry { entry, .. }) => {
            parent_path(node_path).child(PathSegment::ListEntry {
                entry: *entry,
                para: idx,
            })
        }
        _ => TreePath::root(idx),
    }
}

/// Replace the single node at `node_path` with `replacement` in its container: top-level
/// paragraphs, a quote's children, or a list entry's paragraph vec (so a container nested
/// inside a list item can be spliced too). Returns the base index of the replacement.
fn container_splice(
    doc: &mut Document,
    node_path: &TreePath,
    replacement: Vec<Paragraph>,
) -> Option<usize> {
    let last = node_path.0.last()?.clone();
    match last {
        PathSegment::Paragraph(i) => {
            if i >= doc.paragraphs.len() {
                return None;
            }
            doc.paragraphs.splice(i..=i, replacement);
            Some(i)
        }
        PathSegment::QuoteChild(c) => match node_at_mut(doc, &parent_path(node_path))? {
            NodeMut::Para(Paragraph::Quote { children }) => {
                if c >= children.len() {
                    return None;
                }
                children.splice(c..=c, replacement);
                Some(c)
            }
            _ => None,
        },
        PathSegment::ListEntry { entry, para } => {
            match node_at_mut(doc, &parent_path(node_path))? {
                NodeMut::Para(
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                ) => {
                    let e = entries.get_mut(entry)?;
                    if para >= e.len() {
                        return None;
                    }
                    e.splice(para..=para, replacement);
                    Some(para)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Move the block addressed by `path` one step toward its previous (`up`) or next sibling
/// within its immediate parent container, carrying any nested descendants. Used by Alt-Up/Down
/// to resort blocks at whatever level the cursor sits: top-level paragraphs, a quote's
/// children, checklist items, or list items. For a list the whole entry moves (all its
/// paragraphs and sublists) regardless of which paragraph `path` addresses, so the cursor's
/// `para` is preserved. Returns the moved block's new path, or `None` at the container's
/// first/last boundary or for an invalid path.
pub fn move_sibling(doc: &mut Document, path: &TreePath, up: bool) -> Option<TreePath> {
    let last = path.0.last()?.clone();
    let parent = parent_path(path);
    match last {
        PathSegment::Paragraph(i) => {
            let target = sibling_target(i, doc.paragraphs.len(), up)?;
            doc.paragraphs.swap(i, target);
            Some(TreePath::root(target))
        }
        PathSegment::QuoteChild(c) => {
            let NodeMut::Para(Paragraph::Quote { children }) = node_at_mut(doc, &parent)? else {
                return None;
            };
            let target = sibling_target(c, children.len(), up)?;
            children.swap(c, target);
            Some(parent.child(PathSegment::QuoteChild(target)))
        }
        PathSegment::ListEntry { entry, para } => {
            let NodeMut::Para(
                Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
            ) = node_at_mut(doc, &parent)?
            else {
                return None;
            };
            let target = sibling_target(entry, entries.len(), up)?;
            entries.swap(entry, target);
            Some(parent.child(PathSegment::ListEntry {
                entry: target,
                para,
            }))
        }
        PathSegment::ChecklistItem(c) => {
            let items = match node_at_mut(doc, &parent)? {
                NodeMut::Para(Paragraph::Checklist { items }) => items,
                NodeMut::Check(item) => &mut item.children,
                _ => return None,
            };
            let target = sibling_target(c, items.len(), up)?;
            items.swap(c, target);
            Some(parent.child(PathSegment::ChecklistItem(target)))
        }
    }
}

/// The index of the sibling one step up (`up`) or down from `idx` in a container of `len`
/// items, or `None` at the boundary (the first item moving up, the last moving down) or when
/// `idx` is out of range.
fn sibling_target(idx: usize, len: usize, up: bool) -> Option<usize> {
    if idx >= len {
        return None;
    }
    if up {
        idx.checked_sub(1)
    } else {
        (idx + 1 < len).then_some(idx + 1)
    }
}

// ---- Cross-boundary block move (Alt-Up/Down) --------------------------------------

/// Move the block at `path` one step up/down in reading order, crossing container
/// boundaries. Within a list this reorders siblings ([`move_sibling`]); at a list's edge the
/// item *leaves* the list carried as a same-kind single-item list (preserving its
/// checkbox/bullet) and, in the same step, moves past the block beyond the list — so the
/// item visibly advances rather than splitting off a same-position adjacent list. It then
/// keeps hopping past neighboring blocks and merges into the next same-kind list it reaches.
/// A plain text paragraph that meets a list/quote is drawn into it, and a quote child at the
/// quote's edge is lifted out. Sublists nested inside a list item keep the old edge-is-no-op
/// behavior (Shift-Tab remains the way out of those). Returns the moved block's new leaf
/// path, or `None` when it cannot move — a top-level block already at the document's edge, a
/// nested sublist item at its edge, or an invalid path.
pub fn move_block(doc: &mut Document, path: &TreePath, up: bool) -> Option<TreePath> {
    match path.0.last()? {
        PathSegment::ListEntry { .. } | PathSegment::ChecklistItem(_) => {
            move_list_item(doc, path, up)
        }
        PathSegment::Paragraph(_) | PathSegment::QuoteChild(_) => move_plain_block(doc, path, up),
    }
}

/// Number of entries/items directly held by the list/checklist node at `list_path`.
fn list_child_count(doc: &Document, list_path: &TreePath) -> Option<usize> {
    Some(match node_at(doc, list_path)? {
        NodeRef::Para(
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
        ) => entries.len(),
        NodeRef::Para(Paragraph::Checklist { items }) => items.len(),
        NodeRef::Check(item) => item.children.len(),
        _ => return None,
    })
}

/// Number of children of a container paragraph (quote children / list entries / checklist
/// items); `0` for a non-container.
fn container_child_count(p: &Paragraph) -> usize {
    match p {
        Paragraph::Quote { children } => children.len(),
        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => entries.len(),
        Paragraph::Checklist { items } => items.len(),
        _ => 0,
    }
}

/// The path segment that selects child `idx` of container paragraph `p`, or `None` for a
/// non-container. List/checklist children carry `para = 0`; callers restore `para` if needed.
fn child_segment_for(p: &Paragraph, idx: usize) -> Option<PathSegment> {
    match p {
        Paragraph::Quote { .. } => Some(PathSegment::QuoteChild(idx)),
        Paragraph::OrderedList { .. } | Paragraph::UnorderedList { .. } => {
            Some(PathSegment::ListEntry {
                entry: idx,
                para: 0,
            })
        }
        Paragraph::Checklist { .. } => Some(PathSegment::ChecklistItem(idx)),
        _ => None,
    }
}

/// Move a list/checklist item ([`PathSegment::ListEntry`]/[`ChecklistItem`] leaf) one step.
/// Interior items reorder within the list; an item at the list's edge that lives in a
/// top-level or quote-child list crosses out of it.
fn move_list_item(doc: &mut Document, path: &TreePath, up: bool) -> Option<TreePath> {
    let list_path = parent_path(path);
    let len = list_child_count(doc, &list_path)?;
    let idx = match path.0.last()? {
        PathSegment::ListEntry { entry, .. } => *entry,
        PathSegment::ChecklistItem(c) => *c,
        _ => return None,
    };
    let at_boundary = if up { idx == 0 } else { idx + 1 >= len };
    if !at_boundary {
        // Interior: reorder among siblings, carrying the whole entry subtree.
        return move_sibling(doc, path, up);
    }
    // At the list's edge. Only a list sitting directly in a `Vec<Paragraph>` (the document
    // top level or a quote) can be crossed out of; a sublist nested in a list item stays put.
    let (slice, li) = sibling_slice(doc, &list_path)?;
    if len == 1 {
        // The item *is* the whole list: move that single-item list as a block.
        return move_lone_list(doc, path, &list_path, up);
    }
    // One of several items leaving its list. It must *move past* the block beyond its list
    // (or out of an enclosing quote) in a single step — detaching it into a same-position
    // adjacent list would only renumber without visibly moving anything. If there is nothing
    // beyond the list in that direction, the item is already at the document's edge.
    let has_neighbor = if up { li > 0 } else { li + 1 < slice.len() };
    let in_quote = matches!(list_path.0.last(), Some(PathSegment::QuoteChild(_)));
    if !has_neighbor && !in_quote {
        return None;
    }
    let lone_leaf = detach_item_to_lone_list(doc, path, up)?;
    let lone_list = parent_path(&lone_leaf);
    move_lone_list(doc, &lone_leaf, &lone_list, up)
}

/// Split the boundary item at `path` out of its (multi-item) list into a fresh same-kind
/// single-item list, inserted immediately before (`up`) or after the list in the list's
/// container. The item keeps its kind, checkbox, continuation paragraphs and sublists. This
/// is only the first half of a boundary move — the caller then hops the detached list past
/// the neighbor block so the item actually advances (see [`move_list_item`]).
fn detach_item_to_lone_list(doc: &mut Document, path: &TreePath, up: bool) -> Option<TreePath> {
    let list_path = parent_path(path);
    let last = path.0.last()?.clone();
    let (lone, tail) = match last {
        PathSegment::ListEntry { entry, para } => {
            let ordered = match node_at_mut(doc, &list_path)? {
                NodeMut::Para(p) => list_ordered(p)?,
                _ => return None,
            };
            let removed = match node_at_mut(doc, &list_path)? {
                NodeMut::Para(
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                ) => {
                    if entry >= entries.len() {
                        return None;
                    }
                    entries.remove(entry)
                }
                _ => return None,
            };
            (
                new_list(ordered, vec![removed]),
                PathSegment::ListEntry { entry: 0, para },
            )
        }
        PathSegment::ChecklistItem(c) => {
            let removed = match node_at_mut(doc, &list_path)? {
                NodeMut::Para(Paragraph::Checklist { items }) => {
                    if c >= items.len() {
                        return None;
                    }
                    items.remove(c)
                }
                _ => return None,
            };
            (
                Paragraph::new_checklist().with_checklist_items(vec![removed]),
                PathSegment::ChecklistItem(0),
            )
        }
        _ => return None,
    };
    let (vec, li) = sibling_vec_mut(doc, &list_path)?;
    let insert_at = if up { li } else { li + 1 };
    vec.insert(insert_at, lone);
    Some(container_child_path(&list_path, insert_at).child(tail))
}

/// Move a single-item list (the item *is* the list) as one block within its container: it
/// leaves a quote at the quote's edge, merges into an adjacent same-kind list, or otherwise
/// swaps past its neighbor. The cursor follows the item into its new home.
fn move_lone_list(
    doc: &mut Document,
    path: &TreePath,
    list_path: &TreePath,
    up: bool,
) -> Option<TreePath> {
    // The item's own leaf segment (its `para` for list entries) rides along with it.
    let (is_checklist, para) = match path.0.last()? {
        PathSegment::ListEntry { para, .. } => (false, *para),
        PathSegment::ChecklistItem(_) => (true, 0),
        _ => return None,
    };
    let child_seg = |idx: usize| {
        if is_checklist {
            PathSegment::ChecklistItem(idx)
        } else {
            PathSegment::ListEntry { entry: idx, para }
        }
    };

    let (slice, li) = sibling_slice(doc, list_path)?;
    let len = slice.len();
    let at_boundary = if up { li == 0 } else { li + 1 >= len };
    if at_boundary {
        // At the container's edge: leave an enclosing quote, else the document edge stops us.
        return match list_path.0.last()? {
            PathSegment::QuoteChild(c) => {
                let lifted = exit_quote_to_container(doc, &parent_path(list_path), *c)?;
                Some(lifted.child(child_seg(0)))
            }
            _ => None,
        };
    }

    let j = if up { li - 1 } else { li + 1 };
    let my_kind = list_like_kind(&slice[li]);
    // A same-kind list right next to us: merge straight in.
    if my_kind.is_some() && list_like_kind(&slice[j]) == my_kind {
        return merge_lone_into_list_at(doc, list_path, li, j, up, para);
    }
    // A plain (non-container) block with a same-kind list just beyond it: cross the block and
    // merge into that list in one step, so we never leave two adjacent same-kind lists behind
    // (which for ordered lists would show a stray restart at `1.`).
    let beyond = if up { j.checked_sub(1) } else { Some(j + 1) };
    if my_kind.is_some()
        && !is_container_para(&slice[j])
        && let Some(k) = beyond
        && slice.get(k).and_then(list_like_kind) == my_kind
    {
        return merge_lone_into_list_at(doc, list_path, li, k, up, para);
    }
    // Otherwise hop the whole single-item list past its neighbor.
    let (vec, _) = sibling_vec_mut(doc, list_path)?;
    vec.swap(li, j);
    Some(container_child_path(list_path, j).child(child_seg(0)))
}

/// Merge the single-item list at index `li` into the same-kind list at index `k` in the same
/// container, then drop the now-empty lone list. The item joins `k`'s end when moving up (so it
/// lands just past `li`) or `k`'s start when moving down; `k` may be the immediate neighbor or
/// one block beyond a plain leaf we're crossing. Returns the merged item's new leaf path.
fn merge_lone_into_list_at(
    doc: &mut Document,
    list_path: &TreePath,
    li: usize,
    k: usize,
    up: bool,
    para: usize,
) -> Option<TreePath> {
    let (vec, _) = sibling_vec_mut(doc, list_path)?;
    let is_checklist = matches!(vec[li], Paragraph::Checklist { .. });
    // Take the lone list's sole entry/item and splice it into the target list: at its end when
    // moving up (so the item lands just past `li`), its start when moving down.
    let child_idx = if is_checklist {
        let item = match &mut vec[li] {
            Paragraph::Checklist { items } => items.pop()?,
            _ => return None,
        };
        match &mut vec[k] {
            Paragraph::Checklist { items } => {
                if up {
                    items.push(item);
                    items.len() - 1
                } else {
                    items.insert(0, item);
                    0
                }
            }
            _ => return None,
        }
    } else {
        let entry = match &mut vec[li] {
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
                entries.pop()?
            }
            _ => return None,
        };
        match &mut vec[k] {
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
                if up {
                    entries.push(entry);
                    entries.len() - 1
                } else {
                    entries.insert(0, entry);
                    0
                }
            }
            _ => return None,
        }
    };
    vec.remove(li);
    // Removing the lone list at `li` shifts the target down by one if it sat after `li`.
    let target = if li < k { k - 1 } else { k };
    let tail = if is_checklist {
        PathSegment::ChecklistItem(child_idx)
    } else {
        PathSegment::ListEntry {
            entry: child_idx,
            para,
        }
    };
    Some(container_child_path(list_path, target).child(tail))
}

/// Move a plain leaf paragraph (a top-level or quote-child Text/heading/code/table) one step:
/// a text paragraph is drawn into an adjacent list/quote/checklist; at a quote's edge it is
/// lifted out; otherwise it hops past its neighbor. Top-level blocks can't leave the document.
fn move_plain_block(doc: &mut Document, path: &TreePath, up: bool) -> Option<TreePath> {
    let (slice, i) = sibling_slice(doc, path)?;
    let len = slice.len();
    let at_boundary = if up { i == 0 } else { i + 1 >= len };
    if at_boundary {
        return match path.0.last()? {
            PathSegment::QuoteChild(c) => exit_quote_to_container(doc, &parent_path(path), *c),
            _ => None,
        };
    }
    let j = if up { i - 1 } else { i + 1 };
    let me_is_text = matches!(
        node_at(doc, path),
        Some(NodeRef::Para(Paragraph::Text { .. }))
    );
    if me_is_text && is_container_para(&slice[j]) {
        return collapse_into_neighbor(doc, path, up);
    }
    let (vec, _) = sibling_vec_mut(doc, path)?;
    vec.swap(i, j);
    Some(container_child_path(path, j))
}

/// Draw the text paragraph at `path` into its adjacent container neighbor — at the neighbor's
/// start when moving down, its end when moving up — as a new list entry / checklist item /
/// quote child. Returns the path of the drawn-in leaf.
fn collapse_into_neighbor(doc: &mut Document, path: &TreePath, up: bool) -> Option<TreePath> {
    let (slice, i) = sibling_slice(doc, path)?;
    let j = if up { i - 1 } else { i + 1 };
    let at_start = !up;
    let landed = if at_start {
        0
    } else {
        container_child_count(&slice[j])
    };
    let (vec, i) = sibling_vec_mut(doc, path)?;
    let me = vec.remove(i);
    // After removing the paragraph at `i`, a following neighbor shifts down by one.
    let nj = if i < j { j - 1 } else { j };
    add_paragraphs_to_container(&mut vec[nj], vec![me], at_start);
    let seg = child_segment_for(&vec[nj], landed)?;
    Some(container_child_path(path, nj).child(seg))
}

// ---- Multi-block (selection) move -------------------------------------------------

/// Number of direct children of the container at `container_path` (the document top level for
/// an empty path, else a quote/list/checklist node), or `None` for a non-container.
pub fn container_child_count_at(doc: &Document, container_path: &TreePath) -> Option<usize> {
    if container_path.is_empty() {
        return Some(doc.paragraphs.len());
    }
    Some(match node_at(doc, container_path)? {
        NodeRef::Para(
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
        ) => entries.len(),
        NodeRef::Para(Paragraph::Checklist { items }) => items.len(),
        NodeRef::Para(Paragraph::Quote { children }) => children.len(),
        NodeRef::Check(item) => item.children.len(),
        _ => return None,
    })
}

/// Shift the single child just outside the `[first, last]` run to the run's far side, moving
/// the run one step within `vec`: the child before the run moves to just after it (`up`), or
/// the child after the run moves to just before it. No-op (false) at the vec's edge.
fn rotate_run<T>(vec: &mut Vec<T>, first: usize, last: usize, up: bool) -> bool {
    if up {
        if first == 0 || last >= vec.len() {
            return false;
        }
        let x = vec.remove(first - 1);
        vec.insert(last, x);
    } else {
        if last + 1 >= vec.len() {
            return false;
        }
        let x = vec.remove(last + 1);
        vec.insert(first, x);
    }
    true
}

/// Reorder the run of children `[first, last]` of the container at `container_path` one step
/// up/down among its siblings (see [`rotate_run`]). Treats each child as an opaque block.
pub fn rotate_children(
    doc: &mut Document,
    container_path: &TreePath,
    first: usize,
    last: usize,
    up: bool,
) -> bool {
    if container_path.is_empty() {
        return rotate_run(&mut doc.paragraphs, first, last, up);
    }
    match node_at_mut(doc, container_path) {
        Some(NodeMut::Para(
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
        )) => rotate_run(entries, first, last, up),
        Some(NodeMut::Para(Paragraph::Checklist { items })) => rotate_run(items, first, last, up),
        Some(NodeMut::Para(Paragraph::Quote { children })) => rotate_run(children, first, last, up),
        Some(NodeMut::Check(item)) => rotate_run(&mut item.children, first, last, up),
        _ => false,
    }
}

/// Move the run of children `[first, last]` of the container at `container_path` one step in
/// reading order, carrying them together. Within the container this reorders siblings; at the
/// container's edge a list/checklist run leaves the list (as a same-kind list that hops the
/// neighbour or merges into the next same-kind list) and a quote run is lifted out. Returns the
/// run's new `(container, first, last)` location, or `None` when it cannot move (a top-level run
/// at the document edge, a nested sublist run at its edge, or an invalid range).
pub fn move_block_range(
    doc: &mut Document,
    container_path: &TreePath,
    first: usize,
    last: usize,
    up: bool,
) -> Option<(TreePath, usize, usize)> {
    let count = last.checked_sub(first)? + 1;
    let cc = container_child_count_at(doc, container_path)?;
    if last >= cc {
        return None;
    }
    let has_neighbor = if up { first > 0 } else { last + 1 < cc };
    if has_neighbor {
        if !rotate_children(doc, container_path, first, last, up) {
            return None;
        }
        return Some(if up {
            (container_path.clone(), first - 1, last - 1)
        } else {
            (container_path.clone(), first + 1, last + 1)
        });
    }
    // Boundary: the run must leave its container.
    if container_path.is_empty() {
        return None; // top-level run already at the document's edge
    }
    // Only a list/checklist/quote sitting directly in a `Vec<Paragraph>` can be crossed out of;
    // a container nested inside a list/checklist item keeps the run where it is.
    sibling_slice(doc, container_path)?;
    match node_at(doc, container_path)? {
        NodeRef::Para(
            Paragraph::OrderedList { .. }
            | Paragraph::UnorderedList { .. }
            | Paragraph::Checklist { .. },
        ) => cross_out_list_run(doc, container_path, first, last, count, up),
        NodeRef::Para(Paragraph::Quote { .. }) => {
            cross_out_quote_run(doc, container_path, first, last, count, up)
        }
        _ => None,
    }
}

/// Prepend `src`'s entries/items to the start of the same-kind list `dst` (mirror of
/// [`append_list_items`], which appends).
fn prepend_list_items(dst: &mut Paragraph, src: Paragraph) {
    match dst {
        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
            if let Paragraph::OrderedList { entries: s } | Paragraph::UnorderedList { entries: s } =
                src
            {
                entries.splice(0..0, s);
            }
        }
        Paragraph::Checklist { items } => {
            if let Paragraph::Checklist { items: s } = src {
                items.splice(0..0, s);
            }
        }
        _ => {}
    }
}

/// Carry the list/checklist run `[first, last]` out of the list at `list_path` as one group: it
/// leaves as a same-kind list that then hops past the block beyond the list, or merges (all its
/// items) into the next same-kind list, or is lifted out of an enclosing quote. Returns the
/// group's new `(container, first, last)`.
fn cross_out_list_run(
    doc: &mut Document,
    list_path: &TreePath,
    first: usize,
    last: usize,
    count: usize,
    up: bool,
) -> Option<(TreePath, usize, usize)> {
    // Guard: the group can only advance if there is a block beyond the list (or the list sits in
    // a quote to leave); otherwise it is already at the document edge — splitting it off would
    // just leave a same-position adjacent list.
    let (slice0, li0) = sibling_slice(doc, list_path)?;
    let has_nb = if up { li0 > 0 } else { li0 + 1 < slice0.len() };
    let in_quote = matches!(list_path.0.last(), Some(PathSegment::QuoteChild(_)));
    if !has_nb && !in_quote {
        return None;
    }
    let ordered = match node_at(doc, list_path)? {
        NodeRef::Para(Paragraph::OrderedList { .. }) => Some(true),
        NodeRef::Para(Paragraph::UnorderedList { .. }) => Some(false),
        NodeRef::Para(Paragraph::Checklist { .. }) => None,
        _ => return None,
    };
    // Extract the run into a fresh same-kind list `g`.
    let g = match node_at_mut(doc, list_path)? {
        NodeMut::Para(
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
        ) => {
            if last >= entries.len() {
                return None;
            }
            let run: Vec<Vec<Paragraph>> = entries.splice(first..=last, []).collect();
            new_list(ordered?, run)
        }
        NodeMut::Para(Paragraph::Checklist { items }) => {
            if last >= items.len() {
                return None;
            }
            let run: Vec<ChecklistItem> = items.splice(first..=last, []).collect();
            Paragraph::new_checklist().with_checklist_items(run)
        }
        _ => return None,
    };
    let list_empty =
        matches!(node_at(doc, list_path), Some(NodeRef::Para(p)) if container_child_count(p) == 0);
    let (vec, li) = sibling_vec_mut(doc, list_path)?;
    // Place `g` beside the (possibly now-empty) source list.
    let gi = if up {
        vec.insert(li, g);
        if list_empty {
            vec.remove(li + 1); // drop the emptied source list, now just after `g`
        }
        li
    } else if list_empty {
        vec[li] = g; // replace the emptied source list in place
        li
    } else {
        vec.insert(li + 1, g);
        li + 1
    };

    // Move `g` one step within the container: lift out of a quote at the edge, merge into a
    // same-kind list, or hop past a plain neighbour.
    let len = vec.len();
    let at_edge = if up { gi == 0 } else { gi + 1 >= len };
    if at_edge {
        // Guard guaranteed a quote here (a plain edge would have returned early).
        let quote_path = parent_path(list_path);
        let lifted = exit_quote_to_container(doc, &quote_path, gi)?;
        return Some((lifted, 0, count - 1));
    }
    let j = if up { gi - 1 } else { gi + 1 };
    let my_kind = list_like_kind(&vec[gi]);
    if my_kind.is_some() && list_like_kind(&vec[j]) == my_kind {
        return merge_group_into(doc, list_path, gi, j, count, up);
    }
    let beyond = if up { j.checked_sub(1) } else { Some(j + 1) };
    if my_kind.is_some()
        && !is_container_para(&vec[j])
        && let Some(k) = beyond
        && vec.get(k).and_then(list_like_kind) == my_kind
    {
        return merge_group_into(doc, list_path, gi, k, count, up);
    }
    vec.swap(gi, j);
    // `g` now sits at `j`; the group is its own items `[0, count-1]`.
    Some((container_child_path(list_path, j), 0, count - 1))
}

/// Merge the whole group list at `gi` into the same-kind list at `k` (both children of the
/// container holding `list_path`): appended at `k`'s end when moving up, prepended at its start
/// when moving down. Returns the merged run's new `(container, first, last)`.
fn merge_group_into(
    doc: &mut Document,
    list_path: &TreePath,
    gi: usize,
    k: usize,
    count: usize,
    up: bool,
) -> Option<(TreePath, usize, usize)> {
    let (vec, _) = sibling_vec_mut(doc, list_path)?;
    let base = if up {
        container_child_count(&vec[k])
    } else {
        0
    };
    let g = std::mem::replace(&mut vec[gi], Paragraph::new_text());
    if up {
        append_list_items(&mut vec[k], g);
    } else {
        prepend_list_items(&mut vec[k], g);
    }
    vec.remove(gi);
    let target = if gi < k { k - 1 } else { k };
    Some((
        container_child_path(list_path, target),
        base,
        base + count - 1,
    ))
}

/// Lift the quote-child run `[first, last]` out of the quote at `quote_path` into the quote's
/// container (each child becomes a plain paragraph), splitting the quote around it. Returns the
/// lifted run's new `(container, first, last)`.
fn cross_out_quote_run(
    doc: &mut Document,
    quote_path: &TreePath,
    first: usize,
    last: usize,
    count: usize,
    up: bool,
) -> Option<(TreePath, usize, usize)> {
    let _ = up; // a quote run lifts out the same way in either direction
    let children = match node_at_mut(doc, quote_path)? {
        NodeMut::Para(Paragraph::Quote { children }) => {
            if last >= children.len() {
                return None;
            }
            std::mem::take(children)
        }
        _ => return None,
    };
    let mut before = children;
    let after = before.split_off(last + 1);
    let run = before.split_off(first); // `before` keeps [0, first), `run` is [first, last]

    let mut replacement: Vec<Paragraph> = Vec::new();
    if !before.is_empty() {
        replacement.push(Paragraph::new_quote().with_children(before));
    }
    let run_start = replacement.len();
    replacement.extend(run);
    if !after.is_empty() {
        replacement.push(Paragraph::new_quote().with_children(after));
    }
    let base = container_splice(doc, quote_path, replacement)?;
    let container = parent_path(quote_path);
    Some((container, base + run_start, base + run_start + count - 1))
}

// ---- Container conversion / dissolve / merge --------------------------------------

/// The four convertible container kinds (a superset of [`ListKind`] that adds quotes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContainerKind {
    Quote,
    Ordered,
    Unordered,
    Checklist,
}

fn container_kind_of(p: &Paragraph) -> Option<ContainerKind> {
    match p {
        Paragraph::Quote { .. } => Some(ContainerKind::Quote),
        Paragraph::OrderedList { .. } => Some(ContainerKind::Ordered),
        Paragraph::UnorderedList { .. } => Some(ContainerKind::Unordered),
        Paragraph::Checklist { .. } => Some(ContainerKind::Checklist),
        _ => None,
    }
}

/// Convert the container node at `container_path` to `target` in place, remapping its
/// children (quote children ↔ one list entry / checklist item each; list/checklist ↔ quote
/// children). Preserves leaf order and count, so a caller can restore the cursor by flat
/// leaf index. Returns `None` for a non-container node or a same-kind no-op. Works at any
/// nesting depth (uses `node_at_mut`/`set_node`, which descend everywhere).
pub fn convert_container(
    doc: &mut Document,
    container_path: &TreePath,
    target: ContainerKind,
) -> Option<()> {
    let want_ordered = matches!(target, ContainerKind::Ordered);
    // Normalize the container's contents into a list of items (each a paragraph vec).
    let items: Vec<Vec<Paragraph>> = match node_at_mut(doc, container_path)? {
        NodeMut::Para(node) => {
            if container_kind_of(node) == Some(target) {
                return None; // already the target kind
            }
            match node {
                Paragraph::Quote { children } => std::mem::take(children)
                    .into_iter()
                    .map(|c| vec![c])
                    .collect(),
                Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
                    std::mem::take(entries)
                }
                Paragraph::Checklist { items } => std::mem::take(items)
                    .into_iter()
                    .map(|it| checklist_item_to_entry(it, want_ordered))
                    .collect(),
                _ => return None,
            }
        }
        _ => return None,
    };
    let new_node = match target {
        ContainerKind::Quote => {
            Paragraph::new_quote().with_children(items.into_iter().flatten().collect())
        }
        ContainerKind::Ordered => new_list(true, items),
        ContainerKind::Unordered => new_list(false, items),
        ContainerKind::Checklist => Paragraph::new_checklist()
            .with_checklist_items(items.into_iter().map(entry_to_checklist_item).collect()),
    };
    set_node(doc, container_path, new_node)?;
    Some(())
}

/// Dissolve the container node at `container_path`, lifting its children up one level into
/// its own parent (document top level or an enclosing quote — the containers
/// `container_splice` understands). Quote children lift verbatim; list entries lift as
/// paragraphs (first para as plain text, continuation paras and sublists alongside);
/// checklist items lift as text + a nested checklist for their children. Returns the base
/// index of the lifted run, or `None` for a non-container node or an unsupported parent
/// (a list entry / checklist item). Preserves leaf order and count.
pub fn dissolve_container(doc: &mut Document, container_path: &TreePath) -> Option<usize> {
    let paras: Vec<Paragraph> = match node_at_mut(doc, container_path)? {
        NodeMut::Para(Paragraph::Quote { children }) => std::mem::take(children),
        NodeMut::Para(
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
        ) => {
            let mut out = Vec::new();
            for entry in std::mem::take(entries) {
                let mut paras = entry.into_iter();
                if let Some(first) = paras.next() {
                    out.push(Paragraph::new_text().with_content(first.content().to_vec()));
                }
                out.extend(paras);
            }
            out
        }
        NodeMut::Para(Paragraph::Checklist { items }) => {
            let mut out = Vec::new();
            for item in std::mem::take(items) {
                out.push(Paragraph::new_text().with_content(item.content));
                if !item.children.is_empty() {
                    out.push(Paragraph::new_checklist().with_checklist_items(item.children));
                }
            }
            out
        }
        _ => return None,
    };
    container_splice(doc, container_path, paras)
}

/// Lift quote child `c` out of the quote at `quote_path`, splitting the quote into
/// before/moved/after (the moved child keeps its type). Mirrors `exit_list_to_container`.
fn exit_quote_to_container(
    doc: &mut Document,
    quote_path: &TreePath,
    c: usize,
) -> Option<TreePath> {
    let children = match node_at_mut(doc, quote_path)? {
        NodeMut::Para(Paragraph::Quote { children }) => {
            if c >= children.len() {
                return None;
            }
            std::mem::take(children)
        }
        _ => return None,
    };
    let mut before = children;
    let after = before.split_off(c + 1);
    let moved = before.pop()?; // the child at index c, preserved verbatim

    let mut replacement: Vec<Paragraph> = Vec::new();
    if !before.is_empty() {
        replacement.push(Paragraph::new_quote().with_children(before));
    }
    let moved_start = replacement.len();
    replacement.push(moved);
    if !after.is_empty() {
        replacement.push(Paragraph::new_quote().with_children(after));
    }

    let base = container_splice(doc, quote_path, replacement)?;
    Some(container_child_path(quote_path, base + moved_start))
}

/// Lift entry `entry` of the list that is child `c` of a quote (`list_path` points at the
/// list) out of that quote while keeping it a list item: the quote is split around the list,
/// the moved entry becomes a single-entry list of the same kind placed between the halves in
/// the quote's container, and any entries before/after it — plus the quote's other children —
/// stay in quote halves. The inverse of nesting a list item into a preceding quote with Tab
/// (mirrors `exit_quote_to_container`, but the lifted thing stays a list). Returns the moved
/// item's new path.
fn exit_quote_list_item(
    doc: &mut Document,
    list_path: &TreePath,
    entry: usize,
    para: usize,
) -> Option<TreePath> {
    let PathSegment::QuoteChild(c) = list_path.0.last()?.clone() else {
        return None;
    };
    let ordered = match node_at_mut(doc, list_path)? {
        NodeMut::Para(p) => list_ordered(p)?,
        _ => return None,
    };
    let quote_path = parent_path(list_path);
    let children = match node_at_mut(doc, &quote_path)? {
        NodeMut::Para(Paragraph::Quote { children }) => {
            if c >= children.len() {
                return None;
            }
            std::mem::take(children)
        }
        _ => return None,
    };
    // Split the quote's children around the list, and the list's entries around `entry`.
    let mut before_children = children;
    let after_children = before_children.split_off(c + 1);
    let list_para = before_children.pop()?; // the list itself
    let entries = match list_para {
        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => entries,
        _ => return None,
    };
    if entry >= entries.len() {
        return None;
    }
    let mut before_entries = entries;
    let after_entries = before_entries.split_off(entry + 1);
    let moved = before_entries.pop()?; // the entry's paragraphs

    // The quote half kept before the lifted item: its earlier children, then any earlier
    // entries as a list. The half after: any later entries as a list, then its later children.
    let mut before_half = before_children;
    if !before_entries.is_empty() {
        before_half.push(new_list(ordered, before_entries));
    }
    let mut after_half: Vec<Paragraph> = Vec::new();
    if !after_entries.is_empty() {
        after_half.push(new_list(ordered, after_entries));
    }
    after_half.extend(after_children);

    let mut replacement: Vec<Paragraph> = Vec::new();
    if !before_half.is_empty() {
        replacement.push(Paragraph::new_quote().with_children(before_half));
    }
    let moved_start = replacement.len();
    replacement.push(new_list(ordered, vec![moved]));
    if !after_half.is_empty() {
        replacement.push(Paragraph::new_quote().with_children(after_half));
    }

    let base = container_splice(doc, &quote_path, replacement)?;
    Some(
        container_child_path(&quote_path, base + moved_start)
            .child(PathSegment::ListEntry { entry: 0, para }),
    )
}

/// Add `paras` to `container` as new items — each paragraph becoming its own list entry /
/// checklist item, or a quote child — at the start (`at_start`) or the end. Used to nest a
/// selection of top-level paragraphs into an adjacent list/quote/checklist.
pub fn add_paragraphs_to_container(
    container: &mut Paragraph,
    paras: Vec<Paragraph>,
    at_start: bool,
) {
    match container {
        Paragraph::Quote { children } => {
            if at_start {
                children.splice(0..0, paras);
            } else {
                children.extend(paras);
            }
        }
        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
            let new_entries = paras.into_iter().map(|p| vec![p]);
            if at_start {
                entries.splice(0..0, new_entries);
            } else {
                entries.extend(new_entries);
            }
        }
        Paragraph::Checklist { items } => {
            let new_items = paras
                .into_iter()
                .map(|p| ChecklistItem::new(false).with_content(p.content().to_vec()));
            if at_start {
                items.splice(0..0, new_items);
            } else {
                items.extend(new_items);
            }
        }
        _ => {}
    }
}

/// Whether `p` is a container paragraph (quote, list, or checklist) that paragraphs can be
/// nested into.
fn is_container_para(p: &Paragraph) -> bool {
    container_kind_of(p).is_some()
}

/// Whether the sibling paragraphs `[s, e]` — in the container that holds `first_child_path`,
/// i.e. the document top level or a single quote's children — have a sibling container
/// immediately before them (an append target) or immediately after (a prepend target).
pub fn has_adjacent_container(
    doc: &Document,
    first_child_path: &TreePath,
    s: usize,
    e: usize,
) -> bool {
    let Some((vec, _)) = sibling_slice(doc, first_child_path) else {
        return false;
    };
    if e >= vec.len() {
        return false;
    }
    (s > 0 && vec.get(s - 1).is_some_and(is_container_para))
        || (e + 1 < vec.len() && vec.get(e + 1).is_some_and(is_container_para))
}

/// Move the sibling paragraphs `[s, e]` (in the container that holds `first_child_path`) into
/// an adjacent sibling container: appended to a container immediately before them, or, failing
/// that, prepended to one immediately after. Each paragraph becomes its own list/checklist
/// item or quote child. Preceding takes priority. Returns whether it nested. Works at the
/// document top level or within a quote — the inverse of lifting a child out with `[`.
pub fn nest_paragraphs_into_adjacent(
    doc: &mut Document,
    first_child_path: &TreePath,
    s: usize,
    e: usize,
) -> bool {
    let Some((vec, _)) = sibling_vec_mut(doc, first_child_path) else {
        return false;
    };
    if e >= vec.len() {
        return false;
    }
    let preceding = s > 0 && vec.get(s - 1).is_some_and(is_container_para);
    let following = e + 1 < vec.len() && vec.get(e + 1).is_some_and(is_container_para);
    if !preceding && !following {
        return false;
    }
    let drained: Vec<Paragraph> = vec.drain(s..=e).collect();
    if preceding {
        add_paragraphs_to_container(&mut vec[s - 1], drained, false);
    } else {
        // After draining `s..=e`, the following container now sits at index `s`.
        add_paragraphs_to_container(&mut vec[s], drained, true);
    }
    true
}

/// The list kind of a paragraph node, or `None` if it is not a list/checklist.
fn list_like_kind(p: &Paragraph) -> Option<ListKind> {
    match p {
        Paragraph::OrderedList { .. } => Some(ListKind::Ordered),
        Paragraph::UnorderedList { .. } => Some(ListKind::Unordered),
        Paragraph::Checklist { .. } => Some(ListKind::Checklist),
        _ => None,
    }
}

/// Append `src`'s entries/items into the same-kind list `dst`.
fn append_list_items(dst: &mut Paragraph, src: Paragraph) {
    match dst {
        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
            if let Paragraph::OrderedList { entries: s } | Paragraph::UnorderedList { entries: s } =
                src
            {
                entries.extend(s);
            }
        }
        Paragraph::Checklist { items } => {
            if let Paragraph::Checklist { items: s } = src {
                items.extend(s);
            }
        }
        _ => {}
    }
}

/// The `Vec<Paragraph>` (document top level or a quote's children) holding the node at
/// `child_path`, plus that node's index within it.
fn sibling_vec_mut<'a>(
    doc: &'a mut Document,
    child_path: &TreePath,
) -> Option<(&'a mut Vec<Paragraph>, usize)> {
    match child_path.0.last()? {
        PathSegment::Paragraph(i) => Some((&mut doc.paragraphs, *i)),
        PathSegment::QuoteChild(c) => {
            let qp = parent_path(child_path);
            match node_at_mut(doc, &qp)? {
                NodeMut::Para(Paragraph::Quote { children }) => Some((children, *c)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Immutable counterpart of [`sibling_vec_mut`]: the slice of siblings (document top level or
/// a quote's children) holding the node at `child_path`, plus that node's index within it.
fn sibling_slice<'a>(doc: &'a Document, child_path: &TreePath) -> Option<(&'a [Paragraph], usize)> {
    match child_path.0.last()? {
        PathSegment::Paragraph(i) => Some((&doc.paragraphs, *i)),
        PathSegment::QuoteChild(c) => {
            let qp = parent_path(child_path);
            match node_at(doc, &qp)? {
                NodeRef::Para(Paragraph::Quote { children }) => Some((children, *c)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn with_last_index(path: &TreePath, idx: usize) -> TreePath {
    let mut segs = path.0.clone();
    if let Some(last) = segs.last_mut() {
        *last = match &*last {
            PathSegment::Paragraph(_) => PathSegment::Paragraph(idx),
            PathSegment::QuoteChild(_) => PathSegment::QuoteChild(idx),
            other => other.clone(),
        };
    }
    TreePath(segs)
}

/// Merge the list at `list_path` (a top-level or quote-child list) with immediately
/// adjacent siblings of the same kind, concatenating their entries/items into one list.
/// Returns the merged list's new path. Preserves leaf order and count.
pub fn merge_adjacent_lists(doc: &mut Document, list_path: &TreePath) -> TreePath {
    let Some((vec, idx)) = sibling_vec_mut(doc, list_path) else {
        return list_path.clone();
    };
    let Some(kind) = vec.get(idx).and_then(list_like_kind) else {
        return list_path.clone();
    };
    let mut cur = idx;
    // Merge following same-kind siblings into the current list.
    while cur + 1 < vec.len() && list_like_kind(&vec[cur + 1]) == Some(kind) {
        let next = vec.remove(cur + 1);
        append_list_items(&mut vec[cur], next);
    }
    // Merge the current list into any preceding same-kind sibling.
    while cur > 0 && list_like_kind(&vec[cur - 1]) == Some(kind) {
        let moved = vec.remove(cur);
        append_list_items(&mut vec[cur - 1], moved);
        cur -= 1;
    }
    with_last_index(list_path, cur)
}

/// Indent the list/checklist item at `path` beneath its previous sibling (nesting it in a
/// same-kind sublist). Returns the item's new path, or `None` for the first item / a
/// non-list-item leaf.
/// If the item at `path` is the first item of a top-level list/checklist preceded by another
/// ordered/unordered list, move it into that preceding list — nesting under the list's last
/// item (into a trailing sublist when present), exactly like a normal indent. This lets a
/// bullet/number/checkbox that *starts* a list following another list be indented straight
/// into it. A following ordered/unordered list adopts the preceding list's kind; a following
/// checklist keeps its checkboxes, nesting as a checklist sublist.
fn merge_first_item_into_preceding_list(doc: &mut Document, path: &TreePath) -> Option<TreePath> {
    let i = match path.0.as_slice() {
        [
            PathSegment::Paragraph(i),
            PathSegment::ListEntry { entry: 0, para: 0 },
        ] => *i,
        [PathSegment::Paragraph(i), PathSegment::ChecklistItem(0)] => *i,
        _ => return None,
    };
    if i == 0
        || !matches!(
            doc.paragraphs.get(i - 1),
            Some(Paragraph::OrderedList { .. } | Paragraph::UnorderedList { .. })
        )
    {
        return None;
    }
    let prev = i - 1;

    // A following checklist: nest its first item under the preceding list's last entry as a
    // checklist sublist (preserving the checkboxes), reusing a trailing checklist if present.
    if matches!(doc.paragraphs.get(i), Some(Paragraph::Checklist { .. })) {
        let moved = match doc.paragraphs.get_mut(i)? {
            Paragraph::Checklist { items } => {
                if items.is_empty() {
                    return None;
                }
                items.remove(0)
            }
            _ => return None,
        };
        if matches!(doc.paragraphs.get(i), Some(Paragraph::Checklist { items }) if items.is_empty())
        {
            doc.paragraphs.remove(i);
        }
        let last_entry = match doc.paragraphs.get_mut(prev)? {
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
                entries.len().checked_sub(1)?
            }
            _ => return None,
        };
        let (sub_para, item_idx) = match doc.paragraphs.get_mut(prev)? {
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
                let entry = entries.get_mut(last_entry)?;
                if matches!(entry.last(), Some(Paragraph::Checklist { .. })) {
                    let pi = entry.len() - 1;
                    match entry.get_mut(pi)? {
                        Paragraph::Checklist { items } => {
                            items.push(moved);
                            (pi, items.len() - 1)
                        }
                        _ => return None,
                    }
                } else {
                    entry.push(Paragraph::new_checklist().with_checklist_items(vec![moved]));
                    (entry.len() - 1, 0)
                }
            }
            _ => return None,
        };
        return Some(
            TreePath::root(prev)
                .child(PathSegment::ListEntry {
                    entry: last_entry,
                    para: sub_para,
                })
                .child(PathSegment::ChecklistItem(item_idx)),
        );
    }

    // A following ordered/unordered list: detach the first entry, pruning its list if it
    // becomes empty.
    let moved = match doc.paragraphs.get_mut(i)? {
        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
            if entries.is_empty() {
                return None;
            }
            entries.remove(0)
        }
        _ => return None,
    };
    let emptied = matches!(
        doc.paragraphs.get(i),
        Some(Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries })
            if entries.is_empty()
    );
    if emptied {
        doc.paragraphs.remove(i);
    }
    // Append to the preceding list (still at i - 1), then indent so it nests under that
    // list's last item exactly like a normal indent (joining a trailing sublist if any).
    let appended = match doc.paragraphs.get_mut(prev)? {
        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
            entries.push(moved);
            entries.len() - 1
        }
        _ => return None,
    };
    let appended_path = TreePath::root(prev).child(PathSegment::ListEntry {
        entry: appended,
        para: 0,
    });
    indent_list_item(doc, &appended_path).or(Some(appended_path))
}

/// Move the first entry of the ordered/unordered list containing `path` into the list's
/// immediately preceding sibling when that sibling is a quote (at the document top level or
/// within a quote) — nesting the item *into* the quote while keeping it a list item, i.e. as
/// an entry of a list child of the quote (joining a trailing list there if present, otherwise
/// a new list of the same kind). The item stays a bullet/number, now inside the quote. The
/// outer list is pruned when it empties. Lists preceding are handled by
/// [`merge_first_item_into_preceding_list`] instead (a checklist cannot hold a list). Returns
/// the moved item's new path, or `None` if `path` is not the first entry of such a list.
fn nest_first_item_into_preceding_quote(doc: &mut Document, path: &TreePath) -> Option<TreePath> {
    // Only the first entry of an ordered/unordered list qualifies (cursor anywhere in it).
    if !matches!(path.0.last()?, PathSegment::ListEntry { entry: 0, .. }) {
        return None;
    }
    let list_path = parent_path(path);
    let (vec, idx) = sibling_slice(doc, &list_path)?;
    if idx == 0 || container_kind_of(vec.get(idx - 1)?)? != ContainerKind::Quote {
        return None;
    }
    // Detach the first entry, remembering the list's kind so the item stays the same kind.
    let ordered = match node_at_mut(doc, &list_path)? {
        NodeMut::Para(p) => list_ordered(p)?,
        _ => return None,
    };
    let moved = match node_at_mut(doc, &list_path)? {
        NodeMut::Para(
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
        ) => {
            if entries.is_empty() {
                return None;
            }
            entries.remove(0)
        }
        _ => return None,
    };
    let emptied = matches!(
        node_at_mut(doc, &list_path),
        Some(NodeMut::Para(
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
        )) if entries.is_empty()
    );
    // Add the entry to the quote as a list child — reusing a trailing list there, else a new
    // list of the same kind — so the item remains a list item, now inside the quote.
    let (cvec, cidx) = sibling_vec_mut(doc, &list_path)?;
    let prev = cidx - 1;
    let (qchild, entry_idx) = match &mut cvec[prev] {
        Paragraph::Quote { children } => {
            if matches!(children.last(), Some(p) if list_ordered(p).is_some()) {
                let qi = children.len() - 1;
                match &mut children[qi] {
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries } => {
                        entries.push(moved);
                        (qi, entries.len() - 1)
                    }
                    _ => return None,
                }
            } else {
                children.push(new_list(ordered, vec![moved]));
                (children.len() - 1, 0)
            }
        }
        _ => return None,
    };
    if emptied {
        cvec.remove(cidx); // `prev < cidx`, so the quote's index is unaffected
    }
    Some(
        with_last_index(&list_path, prev)
            .child(PathSegment::QuoteChild(qchild))
            .child(PathSegment::ListEntry {
                entry: entry_idx,
                para: 0,
            }),
    )
}

/// Indent the list/checklist item at `path`, or — for the first item of a top-level list
/// that follows another list — merge it into that preceding list, or — for the first item of
/// a list that follows a quote — nest it into that quote (keeping it a list item).
pub fn indent_list_item_or_merge(doc: &mut Document, path: &TreePath) -> Option<TreePath> {
    indent_list_item(doc, path)
        .or_else(|| merge_first_item_into_preceding_list(doc, path))
        .or_else(|| nest_first_item_into_preceding_quote(doc, path))
}

pub fn indent_list_item(doc: &mut Document, path: &TreePath) -> Option<TreePath> {
    let last = path.0.last()?.clone();
    let pp = parent_path(path);
    match last {
        PathSegment::ListEntry { entry, para } => {
            if entry == 0 {
                return None;
            }
            let ordered = match node_at_mut(doc, &pp)? {
                NodeMut::Para(p) => list_ordered(p)?,
                _ => return None,
            };
            let moved = match node_at_mut(doc, &pp)? {
                NodeMut::Para(
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                ) => {
                    if entry >= entries.len() {
                        return None;
                    }
                    entries.remove(entry)
                }
                _ => return None,
            };
            let prev = entry - 1;
            let (sub_para, sub_entry) = match node_at_mut(doc, &pp)? {
                NodeMut::Para(
                    Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                ) => {
                    let prev_entry = entries.get_mut(prev)?;
                    // Merge into whatever ordered/unordered list already ends the previous
                    // item, regardless of its kind — a bullet indented under an item that
                    // ends in a numbered sublist joins that numbered sublist, and vice
                    // versa — rather than starting a second sublist beside it.
                    let reuse = matches!(prev_entry.last(), Some(p) if list_ordered(p).is_some());
                    if reuse {
                        let pi = prev_entry.len() - 1;
                        match prev_entry.get_mut(pi)? {
                            Paragraph::OrderedList { entries: se }
                            | Paragraph::UnorderedList { entries: se } => {
                                se.push(moved);
                                (pi, se.len() - 1)
                            }
                            _ => return None,
                        }
                    } else {
                        prev_entry.push(new_list(ordered, vec![moved]));
                        (prev_entry.len() - 1, 0)
                    }
                }
                _ => return None,
            };
            Some(
                pp.child(PathSegment::ListEntry {
                    entry: prev,
                    para: sub_para,
                })
                .child(PathSegment::ListEntry {
                    entry: sub_entry,
                    para,
                }),
            )
        }
        PathSegment::ChecklistItem(c) => {
            if c == 0 {
                return None;
            }
            let moved = match node_at_mut(doc, &pp)? {
                NodeMut::Para(Paragraph::Checklist { items }) => {
                    if c >= items.len() {
                        return None;
                    }
                    items.remove(c)
                }
                NodeMut::Check(item) => {
                    if c >= item.children.len() {
                        return None;
                    }
                    item.children.remove(c)
                }
                _ => return None,
            };
            let child_idx = match node_at_mut(doc, &pp)? {
                NodeMut::Para(Paragraph::Checklist { items }) => {
                    let prev = items.get_mut(c - 1)?;
                    prev.children.push(moved);
                    prev.children.len() - 1
                }
                NodeMut::Check(item) => {
                    let prev = item.children.get_mut(c - 1)?;
                    prev.children.push(moved);
                    prev.children.len() - 1
                }
                _ => return None,
            };
            Some(
                pp.child(PathSegment::ChecklistItem(c - 1))
                    .child(PathSegment::ChecklistItem(child_idx)),
            )
        }
        _ => None,
    }
}

/// Outdent the list/checklist item at `path` one level: into the parent list/checklist if
/// nested, out of an enclosing quote as a list item if the list sits directly in a quote,
/// otherwise out of the list into its container (as a paragraph). Returns the new path, or
/// `None` for a non-list-item leaf. This is the Shift-Tab / `[` behavior, which reduces
/// nesting while preserving list-ness.
pub fn outdent_list_item(doc: &mut Document, path: &TreePath) -> Option<TreePath> {
    outdent_list_item_inner(doc, path, true)
}

/// Like [`outdent_list_item`], but a list item nested directly in a quote is *delisted* into
/// the quote as a plain paragraph (losing its bullet) rather than lifted out of the quote as a
/// list. Used where outdenting means "stop being a list item" — Enter on an empty item, and
/// toggling a list off — instead of "reduce nesting."
pub fn outdent_list_item_delisting(doc: &mut Document, path: &TreePath) -> Option<TreePath> {
    outdent_list_item_inner(doc, path, false)
}

fn outdent_list_item_inner(
    doc: &mut Document,
    path: &TreePath,
    keep_list_in_quote: bool,
) -> Option<TreePath> {
    let last = path.0.last()?.clone();
    let pp = parent_path(path);
    match last {
        PathSegment::ListEntry { entry, para } => match pp.0.last() {
            Some(PathSegment::ListEntry {
                entry: outer_e,
                para: outer_para,
            }) => {
                let (outer_e, outer_para) = (*outer_e, *outer_para);
                let ppp = parent_path(&pp);
                let ordered = match node_at_mut(doc, &pp)? {
                    NodeMut::Para(p) => list_ordered(p)?,
                    _ => return None,
                };
                // Detach the outdented entry together with the siblings that follow it in
                // the inner list; those followers become the outdented item's children
                // (they were visually nested under it and stay grouped with it).
                let (mut moved, following) = match node_at_mut(doc, &pp)? {
                    NodeMut::Para(
                        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                    ) => {
                        if entry >= entries.len() {
                            return None;
                        }
                        let following = entries.split_off(entry + 1);
                        (entries.remove(entry), following)
                    }
                    _ => return None,
                };
                let inner_empty = match node_at_mut(doc, &pp)? {
                    NodeMut::Para(
                        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                    ) => entries.is_empty(),
                    _ => false,
                };
                if !following.is_empty() {
                    append_children(&mut moved, ordered, following);
                }
                match node_at_mut(doc, &ppp)? {
                    NodeMut::Para(
                        Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
                    ) => {
                        if inner_empty
                            && let Some(outer_entry) = entries.get_mut(outer_e)
                            && outer_para < outer_entry.len()
                        {
                            outer_entry.remove(outer_para);
                        }
                        if outer_e < entries.len() {
                            entries.insert(outer_e + 1, moved);
                        }
                    }
                    _ => return None,
                }
                Some(ppp.child(PathSegment::ListEntry {
                    entry: outer_e + 1,
                    para,
                }))
            }
            // A list nested directly in a quote: lift the item out of the quote, keeping it a
            // list item (splitting the quote around it) — the inverse of Tab nesting a list
            // item into a preceding quote — unless we are delisting, in which case fall through
            // to drop it into the quote as a plain text child.
            Some(PathSegment::QuoteChild(_)) if keep_list_in_quote => {
                exit_quote_list_item(doc, &pp, entry, para)
            }
            _ => exit_list_to_container(doc, &pp, entry, para),
        },
        PathSegment::ChecklistItem(c) => match pp.0.last() {
            Some(PathSegment::ChecklistItem(parent_c)) => {
                let parent_c = *parent_c;
                let ppp = parent_path(&pp);
                // Detach the outdented item with its following siblings; the followers
                // become its own children (appended after any children it already has).
                let (mut moved, following) = match node_at_mut(doc, &pp)? {
                    NodeMut::Check(item) => {
                        if c >= item.children.len() {
                            return None;
                        }
                        let following = item.children.split_off(c + 1);
                        (item.children.remove(c), following)
                    }
                    _ => return None,
                };
                moved.children.extend(following);
                match node_at_mut(doc, &ppp)? {
                    NodeMut::Para(Paragraph::Checklist { items }) => {
                        items.insert(parent_c + 1, moved);
                    }
                    NodeMut::Check(item) => {
                        item.children.insert(parent_c + 1, moved);
                    }
                    _ => return None,
                }
                Some(ppp.child(PathSegment::ChecklistItem(parent_c + 1)))
            }
            // A checklist nested inside an ordered/unordered list entry: lift the item back
            // out to the outer list's level, keeping it a checklist (the inverse of nesting a
            // checklist under a bullet item), rather than delisting it into a text paragraph.
            Some(PathSegment::ListEntry { entry, para }) => {
                let (oe, op) = (*entry, *para);
                exit_nested_checklist_item(doc, &pp, oe, op, c)
            }
            _ => exit_checklist_to_container(doc, &pp, c),
        },
        // A quote child lifts out one level, splitting the quote around it.
        PathSegment::QuoteChild(c) => exit_quote_to_container(doc, &pp, c),
        _ => None,
    }
}

/// Split a top-level (or quote-child) list at `list_path`, lifting entry `entry` out as
/// plain paragraph(s) in the list's container. Returns the path of the lifted leaf.
fn exit_list_to_container(
    doc: &mut Document,
    list_path: &TreePath,
    entry: usize,
    para: usize,
) -> Option<TreePath> {
    let ordered = match node_at_mut(doc, list_path)? {
        NodeMut::Para(p) => list_ordered(p)?,
        _ => return None,
    };
    let entries = match node_at_mut(doc, list_path)? {
        NodeMut::Para(
            Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
        ) => {
            if entry >= entries.len() {
                return None;
            }
            std::mem::take(entries)
        }
        _ => return None,
    };
    let mut before = entries;
    let after = before.split_off(entry + 1);
    let moved = before.pop()?; // the entry's paragraphs

    let mut replacement: Vec<Paragraph> = Vec::new();
    if !before.is_empty() {
        replacement.push(new_list(ordered, before));
    }
    let moved_start = replacement.len();
    replacement.extend(moved);
    if !after.is_empty() {
        replacement.push(new_list(ordered, after));
    }

    let base = container_splice(doc, list_path, replacement)?;
    Some(container_child_path(list_path, base + moved_start + para))
}

/// Lift a top-level checklist item out of the checklist as a plain paragraph.
fn exit_checklist_to_container(
    doc: &mut Document,
    list_path: &TreePath,
    c: usize,
) -> Option<TreePath> {
    let items = match node_at_mut(doc, list_path)? {
        NodeMut::Para(Paragraph::Checklist { items }) => {
            if c >= items.len() {
                return None;
            }
            std::mem::take(items)
        }
        _ => return None,
    };
    let mut before = items;
    let after = before.split_off(c + 1);
    let moved = before.pop()?;

    let mut replacement: Vec<Paragraph> = Vec::new();
    if !before.is_empty() {
        replacement.push(Paragraph::new_checklist().with_checklist_items(before));
    }
    let moved_start = replacement.len();
    replacement.push(Paragraph::new_text().with_content(moved.content));
    if !moved.children.is_empty() {
        replacement.push(Paragraph::new_checklist().with_checklist_items(moved.children));
    }
    if !after.is_empty() {
        replacement.push(Paragraph::new_checklist().with_checklist_items(after));
    }

    let base = container_splice(doc, list_path, replacement)?;
    Some(container_child_path(list_path, base + moved_start))
}

/// Lift a checklist item out of a checklist that is nested inside an ordered/unordered list
/// entry, up to the outer list's container — as a checklist positioned right after the outer
/// list (joining an adjacent checklist there), keeping its checkbox. Following siblings in the
/// sub-checklist become the lifted item's children. The inverse of nesting a checklist under a
/// bullet item. `oe`/`op` locate the checklist within the outer list entry. Returns the lifted
/// item's new path, or `None` if the outer list is itself nested in a list entry (unsupported).
fn exit_nested_checklist_item(
    doc: &mut Document,
    checklist_path: &TreePath,
    oe: usize,
    op: usize,
    c: usize,
) -> Option<TreePath> {
    // Detach item `c` with its following siblings, which become its children.
    let moved = match node_at_mut(doc, checklist_path)? {
        NodeMut::Para(Paragraph::Checklist { items }) => {
            if c >= items.len() {
                return None;
            }
            let following = items.split_off(c + 1);
            let mut moved = items.remove(c);
            moved.children.extend(following);
            moved
        }
        _ => return None,
    };
    // If the sub-checklist is now empty, remove it from the outer list entry.
    let emptied = matches!(
        node_at_mut(doc, checklist_path),
        Some(NodeMut::Para(Paragraph::Checklist { items })) if items.is_empty()
    );
    let outer_list_path = parent_path(checklist_path);
    if emptied {
        match node_at_mut(doc, &outer_list_path)? {
            NodeMut::Para(
                Paragraph::OrderedList { entries } | Paragraph::UnorderedList { entries },
            ) => {
                if let Some(entry) = entries.get_mut(oe)
                    && op < entry.len()
                {
                    entry.remove(op);
                }
            }
            _ => return None,
        }
    }
    // Place the lifted item as a checklist just after the outer list, joining an adjacent
    // checklist there if present (so a whole selected run collects into one checklist as the
    // items are lifted bottom-up).
    let (vec, idx) = sibling_vec_mut(doc, &outer_list_path)?;
    let after = idx + 1;
    if let Some(Paragraph::Checklist { items }) = vec.get_mut(after) {
        items.insert(0, moved);
    } else {
        vec.insert(
            after,
            Paragraph::new_checklist().with_checklist_items(vec![moved]),
        );
    }
    Some(with_last_index(&outer_list_path, after).child(PathSegment::ChecklistItem(0)))
}

/// Remove the list/checklist item at `item_path` from its *immediate* list, placing its
/// paragraph(s) into that list's enclosing container — the document, a quote, or (for a
/// nested item) the parent list item's paragraph vec — at the list's position, splitting
/// the list around it. Unlike `outdent_list_item`, a nested item becomes a plain paragraph
/// *inside its parent list item* rather than an item of the outer list. Returns the lifted
/// paragraph's new path.
pub fn delist_item(doc: &mut Document, item_path: &TreePath) -> Option<TreePath> {
    let last = item_path.0.last()?.clone();
    let list_path = parent_path(item_path);
    match last {
        PathSegment::ListEntry { entry, para } => {
            exit_list_to_container(doc, &list_path, entry, para)
        }
        PathSegment::ChecklistItem(c) => exit_checklist_to_container(doc, &list_path, c),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(md: &str) -> Document {
        tdoc::markdown::parse(&mut Cursor::new(md.as_bytes())).expect("parse")
    }

    fn md(doc: &Document) -> String {
        let mut buf = Vec::new();
        tdoc::markdown::write(&mut buf, doc).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn text(s: &str) -> Paragraph {
        Paragraph::Text {
            content: vec![Span::new_text(s)],
        }
    }

    // ----- Container conversion / dissolve / merge -----

    #[test]
    fn convert_bullet_list_to_quote_flattens_entries() {
        let mut doc = parse("- a\n- b");
        assert!(convert_container(&mut doc, &TreePath::root(0), ContainerKind::Quote).is_some());
        if let Paragraph::Quote { children } = &doc.paragraphs[0] {
            assert_eq!(children.len(), 2);
        } else {
            panic!("expected a quote");
        }
    }

    #[test]
    fn convert_quote_to_bullet_makes_one_item_per_child() {
        let mut doc = parse("x");
        doc.paragraphs = vec![Paragraph::new_quote().with_children(vec![text("a"), text("b")])];
        assert!(
            convert_container(&mut doc, &TreePath::root(0), ContainerKind::Unordered).is_some()
        );
        if let Paragraph::UnorderedList { entries } = &doc.paragraphs[0] {
            assert_eq!(entries.len(), 2);
        } else {
            panic!("expected a list");
        }
    }

    #[test]
    fn convert_container_same_kind_is_noop() {
        let mut doc = parse("- a\n- b");
        assert!(
            convert_container(&mut doc, &TreePath::root(0), ContainerKind::Unordered).is_none()
        );
    }

    #[test]
    fn dissolve_quote_lifts_children_to_top_level() {
        let mut doc = parse("> a");
        assert_eq!(dissolve_container(&mut doc, &TreePath::root(0)), Some(0));
        assert_eq!(doc.paragraphs.len(), 1);
        assert!(matches!(doc.paragraphs[0], Paragraph::Text { .. }));
    }

    #[test]
    fn dissolve_bullet_list_lifts_entries_as_paragraphs() {
        let mut doc = parse("- a\n- b");
        assert_eq!(dissolve_container(&mut doc, &TreePath::root(0)), Some(0));
        assert_eq!(doc.paragraphs.len(), 2);
        assert!(matches!(doc.paragraphs[0], Paragraph::Text { .. }));
        assert!(matches!(doc.paragraphs[1], Paragraph::Text { .. }));
    }

    #[test]
    fn dissolve_inside_list_entry_lifts_into_entry() {
        // A quote nested inside a list entry dissolves into that entry's paragraphs.
        let mut doc = parse("x");
        doc.paragraphs = vec![Paragraph::new_unordered_list().with_entries(vec![vec![
            text("lead"),
            Paragraph::new_quote().with_children(vec![text("a")]),
        ]])];
        let quote_path = TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 1 });
        assert_eq!(dissolve_container(&mut doc, &quote_path), Some(1));
        if let Paragraph::UnorderedList { entries } = &doc.paragraphs[0] {
            assert_eq!(entries[0].len(), 2, "quote's child lifted into the entry");
            assert!(matches!(entries[0][1], Paragraph::Text { .. }));
        } else {
            panic!("expected the list to remain");
        }
    }

    #[test]
    fn outdent_middle_quote_child_splits_quote() {
        let mut doc = parse("x");
        doc.paragraphs =
            vec![Paragraph::new_quote().with_children(vec![text("a"), text("b"), text("c")])];
        let path = TreePath::root(0).child(PathSegment::QuoteChild(1));
        assert!(outdent_list_item(&mut doc, &path).is_some());
        assert_eq!(doc.paragraphs.len(), 3);
        assert!(matches!(doc.paragraphs[0], Paragraph::Quote { .. }));
        assert!(matches!(doc.paragraphs[1], Paragraph::Text { .. }));
        assert!(matches!(doc.paragraphs[2], Paragraph::Quote { .. }));
    }

    #[test]
    fn outdent_only_quote_child_dissolves_quote() {
        let mut doc = parse("> only");
        let path = TreePath::root(0).child(PathSegment::QuoteChild(0));
        assert!(outdent_list_item(&mut doc, &path).is_some());
        assert_eq!(doc.paragraphs.len(), 1);
        assert!(matches!(doc.paragraphs[0], Paragraph::Text { .. }));
    }

    #[test]
    fn split_list_entry_peels_off_continuation_paragraph() {
        // An item with a lead paragraph plus a trailing empty paragraph: splitting at the
        // trailing paragraph moves it into a new entry while the lead stays put.
        let mut doc = parse("x");
        doc.paragraphs = vec![
            Paragraph::new_unordered_list()
                .with_entries(vec![vec![text("lead"), Paragraph::new_text()]]),
        ];
        let path = TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 1 });
        let new = split_list_entry(&mut doc, &path).expect("split off the continuation");
        assert_eq!(
            new,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 })
        );
        let Paragraph::UnorderedList { entries } = &doc.paragraphs[0] else {
            panic!("expected the list to remain");
        };
        assert_eq!(entries.len(), 2, "a new entry was created");
        assert_eq!(entries[0].len(), 1, "lead stays in the first entry");
        assert_eq!(
            entries[1].len(),
            1,
            "the continuation moved to the new entry"
        );
    }

    #[test]
    fn split_list_entry_rejects_leading_paragraph() {
        // The entry's first paragraph is not a continuation, so there is nothing to peel off.
        let mut doc = parse("- only");
        let path = TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 0 });
        assert!(split_list_entry(&mut doc, &path).is_none());
    }

    #[test]
    fn merge_adjacent_bullet_lists_into_one() {
        let mut doc = parse("x");
        doc.paragraphs = vec![
            Paragraph::new_unordered_list().with_entries(vec![vec![text("a")]]),
            Paragraph::new_unordered_list().with_entries(vec![vec![text("b")]]),
        ];
        assert_eq!(
            merge_adjacent_lists(&mut doc, &TreePath::root(0)),
            TreePath::root(0)
        );
        assert_eq!(doc.paragraphs.len(), 1);
        if let Paragraph::UnorderedList { entries } = &doc.paragraphs[0] {
            assert_eq!(entries.len(), 2);
        } else {
            panic!("expected one merged list");
        }
    }

    #[test]
    fn indent_merges_into_existing_sublist_of_other_kind() {
        // Outer bullet list; item "a" already ends in a nested *numbered* sublist.
        let mut doc = parse("x");
        doc.paragraphs = vec![Paragraph::new_unordered_list().with_entries(vec![
            vec![
                text("a"),
                Paragraph::new_ordered_list().with_entries(vec![vec![text("x")]]),
            ],
            vec![text("b")],
        ])];
        // Indent "b" (entry 1) under "a".
        let path = TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 });
        assert!(indent_list_item(&mut doc, &path).is_some());
        let Paragraph::UnorderedList { entries } = &doc.paragraphs[0] else {
            panic!("expected the outer list");
        };
        assert_eq!(entries.len(), 1, "b left the outer list");
        assert_eq!(entries[0].len(), 2, "a still has its text + one sublist");
        let Paragraph::OrderedList { entries: sub } = &entries[0][1] else {
            panic!("b should join a's existing numbered sublist, not start a new one");
        };
        assert_eq!(sub.len(), 2, "b joined the numbered sublist");
    }

    #[test]
    fn merge_does_not_join_different_kinds() {
        let mut doc = parse("x");
        doc.paragraphs = vec![
            Paragraph::new_unordered_list().with_entries(vec![vec![text("a")]]),
            Paragraph::new_ordered_list().with_entries(vec![vec![text("b")]]),
        ];
        merge_adjacent_lists(&mut doc, &TreePath::root(0));
        assert_eq!(doc.paragraphs.len(), 2);
    }

    #[test]
    fn split_top_level_paragraph() {
        let mut doc = parse("HelloWorld");
        let new = split_leaf(&mut doc, &TreePath::root(0), 5).unwrap();
        assert_eq!(new, TreePath::root(1));
        assert_eq!(md(&doc).trim(), "Hello\n\nWorld");
    }

    #[test]
    fn split_list_item_creates_new_entry() {
        let mut doc = parse("- onetwo");
        let path = TreePath::root(0).child(PathSegment::ListEntry { entry: 0, para: 0 });
        let new = split_leaf(&mut doc, &path, 3).unwrap();
        assert_eq!(
            new,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 })
        );
        assert_eq!(md(&doc).trim(), "- one\n- two");
    }

    #[test]
    fn split_heading_keeps_block_type_on_both_halves() {
        // Splitting in the middle of a heading yields two headings.
        let mut doc = parse("# HeaderText");
        let new = split_leaf(&mut doc, &TreePath::root(0), 6).unwrap();
        assert_eq!(new, TreePath::root(1));
        assert!(matches!(doc.paragraphs[0], Paragraph::Header1 { .. }));
        assert!(matches!(doc.paragraphs[1], Paragraph::Header1 { .. }));
        assert_eq!(md(&doc).trim(), "# Header\n\n# Text");
    }

    #[test]
    fn split_heading_at_end_starts_plain_paragraph() {
        // Enter at the end of a heading drops into a plain body paragraph.
        let mut doc = parse("# Title");
        let new = split_leaf(&mut doc, &TreePath::root(0), 5).unwrap();
        assert_eq!(new, TreePath::root(1));
        assert!(matches!(doc.paragraphs[0], Paragraph::Header1 { .. }));
        assert!(matches!(doc.paragraphs[1], Paragraph::Text { .. }));
    }

    #[test]
    fn split_heading_at_start_keeps_both_headings() {
        // At the start, the (empty) leaf above and the content below are both headings —
        // the moved content is never demoted to a plain paragraph.
        let mut doc = parse("# Title");
        let new = split_leaf(&mut doc, &TreePath::root(0), 0).unwrap();
        assert_eq!(new, TreePath::root(1));
        assert!(matches!(doc.paragraphs[0], Paragraph::Header1 { .. }));
        assert!(doc.paragraphs[0].content().is_empty());
        assert!(matches!(doc.paragraphs[1], Paragraph::Header1 { .. }));
        assert_eq!(doc.paragraphs[1].content()[0].text, "Title");
    }

    #[test]
    fn change_nested_list_kind_preserves_outer_level() {
        // Outer ordered list with a nested ordered list under its first entry.
        let mut doc = parse("1. one\n   1. two");
        let nested = TreePath::root(0)
            .child(PathSegment::ListEntry { entry: 0, para: 1 })
            .child(PathSegment::ListEntry { entry: 0, para: 0 });
        let new = change_list_kind(&mut doc, &nested, ListKind::Unordered).unwrap();
        assert_eq!(new, nested);
        // Outer list stays ordered; only the nested list became unordered.
        let Paragraph::OrderedList { entries } = &doc.paragraphs[0] else {
            panic!("outer list should stay ordered");
        };
        assert!(matches!(entries[0][1], Paragraph::UnorderedList { .. }));
    }

    #[test]
    fn outdent_adopts_following_siblings_as_children() {
        // a > {x, y, z}. Outdenting x lifts it beside a and makes y, z its children.
        let mut doc = parse("- a\n  - x\n  - y\n  - z");
        let x = TreePath::root(0)
            .child(PathSegment::ListEntry { entry: 0, para: 1 })
            .child(PathSegment::ListEntry { entry: 0, para: 0 });
        let new = outdent_list_item(&mut doc, &x).unwrap();
        assert_eq!(
            new,
            TreePath::root(0).child(PathSegment::ListEntry { entry: 1, para: 0 })
        );
        let Paragraph::UnorderedList { entries } = &doc.paragraphs[0] else {
            panic!("expected an unordered list");
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].len(), 1); // "a" no longer has a sublist
        let Paragraph::UnorderedList { entries: adopted } = &entries[1][1] else {
            panic!("x should have adopted a sublist of its followers");
        };
        assert_eq!(adopted.len(), 2); // y and z
    }

    #[test]
    fn outdent_middle_item_keeps_preceding_siblings() {
        // a > {x, y, z}. Outdenting y leaves x under a, and z becomes y's child.
        let mut doc = parse("- a\n  - x\n  - y\n  - z");
        let y = TreePath::root(0)
            .child(PathSegment::ListEntry { entry: 0, para: 1 })
            .child(PathSegment::ListEntry { entry: 1, para: 0 });
        outdent_list_item(&mut doc, &y).unwrap();
        let Paragraph::UnorderedList { entries } = &doc.paragraphs[0] else {
            panic!("expected an unordered list");
        };
        assert_eq!(entries.len(), 2);
        // "a" keeps a sublist containing just x.
        let Paragraph::UnorderedList {
            entries: a_children,
        } = &entries[0][1]
        else {
            panic!("a should still have a sublist with x");
        };
        assert_eq!(a_children.len(), 1);
        // y adopted z.
        let Paragraph::UnorderedList {
            entries: y_children,
        } = &entries[1][1]
        else {
            panic!("y should have adopted z");
        };
        assert_eq!(y_children.len(), 1);
    }

    #[test]
    fn merge_paragraph_into_previous() {
        let mut doc = parse("Hello\n\nWorld");
        let (pos, off) = merge_with_previous(&mut doc, &TreePath::root(1)).unwrap();
        assert_eq!(pos, TreePath::root(0));
        assert_eq!(off, 5);
        assert_eq!(md(&doc).trim(), "HelloWorld");
    }

    #[test]
    fn removing_only_list_item_prunes_list() {
        let mut doc = parse("Before\n\n- only\n\nAfter");
        // The list is paragraphs[1]; its single item is entry 0, para 0.
        let item = TreePath::root(1).child(PathSegment::ListEntry { entry: 0, para: 0 });
        remove_node_at(&mut doc, &item);
        assert_eq!(md(&doc).trim(), "Before\n\nAfter");
    }

    #[test]
    fn toggle_checkmark_flips() {
        let mut doc = parse("- [ ] todo");
        let item = TreePath::root(0).child(PathSegment::ChecklistItem(0));
        assert_eq!(toggle_checkmark(&mut doc, &item), Some(true));
        assert!(md(&doc).contains("[x]"));
    }
}
