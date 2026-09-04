# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While pre-1.0, the minor version is bumped for breaking changes.

<!-- next-header -->

## [Unreleased] - ReleaseDate

### Added

- Support for horizontal rules (thematic breaks), matching `tdoc`'s
  `Paragraph::HorizontalRule` (tdoc#49). A rule is a **non-editable leaf** like a
  table: the caret can rest on it, but there is nothing in it to type into.
  - `BlockType::HorizontalRule` and `tree_walk::ParaKind::HorizontalRule`.
    `tree_walk::leaf_spans` answers `None` for a rule — the marker the whole
    engine uses for "not editable" — so splits, merges and inline edits skip it.
  - `Editor::insert_horizontal_rule()` inserts a rule as a top-level block: a
    top-level paragraph is split around it when the caret sits inside its text,
    at a block edge the rule slots in above or below, and from inside a
    list/quote it follows the whole top-level block. The caret lands in the
    block after the rule (an empty paragraph is appended if there is none).
  - Backspace and Delete remove a rule — on the rule itself, and from the edge
    of the block just below or above it. Rules inside a selection are removed
    with it, and copying across one keeps it in the clipboard document.
  - `set_block_type(BlockType::HorizontalRule)` is a deliberate no-op: a rule has
    no inline form, so converting a paragraph into one would drop its text.
  - Layout gives the rule one contentless row — so it is a hit-testable caret
    stop — and paints a line across the block's content column, or the centered
    `───── • ─────` ornament `tdoc`'s terminal formatter uses when
    `Theme::horizontal_rule_as_text` is on.
  - New theme knobs: `horizontal_rule_color`, `horizontal_rule_thickness`,
    `horizontal_rule_spacing` and `horizontal_rule_as_text`. Under
    `classic_block_spacing`, a rule carries (2, 2) line margins, matching tdoc.
- Support for definition lists, matching `tdoc`'s `Paragraph::DefinitionList`.
  Unlike a table or a rule, a definition list is **fully editable**: both halves
  of an item — its terms (`<dt>`) and the paragraphs of its definition (`<dd>`)
  — are ordinary leaves that the caret, typing, styling and selection reach the
  same way they reach any other text.
  - New path segments `PathSegment::DefinitionTerm { item, term }` and
    `PathSegment::DefinitionPara { item, para }`. They are siblings under one
    item but distinct variants, so `TreePath` ordering gained a tie-break that
    keeps an item's terms ahead of its definition.
  - `BlockType::DefinitionTerm { depth }` and
    `tree_walk::ParaKind::DefinitionTerm` for terms. A definition's *body* has no
    block type of its own: its paragraphs keep their intrinsic type (paragraph,
    heading, nested list, …) and indent from the new
    `tree_walk::LeafInfo::definition_depth`, exactly as continuation paragraphs
    inside a list item stay `Paragraph` and indent from their list depth.
  - Enter walks the list one half at a time, so a glossary is written straight
    through: in a term it splits into two terms of the same item, but at the
    *end* of a term whose item has no definition yet it opens that definition
    and moves in; in a definition it starts the **next item**, taking the right
    half as that item's term and the paragraphs below it as its definition —
    the same shape as Enter in a list item starting the next item. Enter on an
    empty term or definition *leaves* the list, splitting it around the new
    paragraph, exactly as Enter on an empty list item leaves a list. So
    term → Enter → definition → Enter → term → … → Enter twice ends the list.
  - **Tab pulls a list under a definition into it.** A list typed below a
    definition list is a list of its own; Tab on its first item moves that item
    into the definition that ends the list above, where it stays the kind of item
    it is — bullet, numbered or checkbox, with its checked state — joining a list
    of that kind already ending that definition, so a whole list can be pulled in
    an item at a time. Shift-Tab is the inverse: the item leaves the definition
    list as a list of its own again (splitting the list when items follow, as any
    definition paragraph leaving does) and rejoins the list it came from rather
    than leaving a seam. This mirrors Tab nesting a list item into a preceding
    quote. `toggle_list` / `toggle_checklist` on such an item still means "no
    longer a list" and delists it where it stands, into a paragraph of the
    definition.
  - `insert_continuation` (Ctrl+P in Pure) keeps its meaning inside a
    definition: another paragraph of the *same* definition. With Enter now
    moving on to the next term, this and Tab are the ways to grow one.
  - **Tab / Shift-Tab switch a line between the two halves.** `Editor::outdent`
    — a new counterpart to `Editor::indent` — turns a definition paragraph into
    the next item's term, taking the paragraphs that followed it in the old
    definition along as its own definition. Tab on a term does the inverse: the
    term becomes the last paragraph of the definition above it, and if it was
    its item's only term that item's definition follows it there and the empty
    item is dropped. The two are exact inverses, so a line can be moved between
    the halves and back. A term of a multi-term item moves alone, leaving the
    others in place, and the list's very first term has nothing above it to join.
    `cursor_can_indent` / `cursor_can_unnest` report both, so a frontend's
    indent/outdent affordances light up without extra work.
  - Backspace merges across the term/definition boundary and prunes an item left
    with nothing, then the list left with no items.
  - `set_block_type(BlockType::DefinitionTerm { .. })` toggles: outside a
    definition list it wraps the selected top-level paragraphs into one, a term
    per paragraph; inside one it dissolves the list back into plain paragraphs.
    Both directions preserve leaf order and count.
  - `set_block_type` with any *other* target lifts the leaf out of the list first
    and applies the type to what that leaves behind — a term and a definition are
    the two halves of an item, not blocks that can become a heading where they
    stand. Everything around the converted leaf keeps its place:
    - A **term** (`tree_edit::lift_definition_term`) leaves on its own. Terms
      before it stay in a list above; terms after it stay in a list below, keeping
      the definition they head. Only when no term is left to head the definition —
      the converted term was the item's last — does the definition come out with
      it, so a one-term item yields the term and its definition as two plain
      paragraphs and nothing is ever orphaned above a term that no longer exists.
    - A **definition** (`tree_edit::lift_definition_para`) takes only its own
      content out, to just below the list, keeping the term a term with an empty
      definition to type into. The paragraphs that followed it inside the
      definition come along keeping their own types.

    Because the lift feeds the ordinary block-type path, container targets (quote,
    list, checklist) work through it unchanged, and the caret stays in the same
    text.
  - **Definition lists split and rejoin automatically.** A leaf leaves its list by
    splitting it, so the halves are put back together whenever the paragraph
    between them stops separating them:
    `tree_edit::merge_adjacent_definition_lists` runs when a paragraph is turned
    into a definition list (matching how the list toggles already merge with an
    adjacent same-kind list), and `remove_node_at` rejoins the lists a deleted
    paragraph leaves touching. Two adjacent lists render exactly like one but
    serialize with a separator between them, so without this a term that left the
    list and came back would leave a permanent seam in the saved file.
  - A lone-text definition deliberately does **not** collapse onto its container
    the way a single-text quote or list item does, so retyping a one-paragraph
    definition affects that paragraph alone rather than the whole list.
  - Layout draws terms in the new `Theme::definition_term` font (bold by
    default — a term has no marker, so weight is what sets it apart) and indents
    each definition by `Theme::definition_indent`, including across paragraph
    breaks and nested definition lists. `Theme::definition_term_spacing` keeps a
    term tight against the definition it heads.

### Changed

- **Breaking:** `BlockType` gained `HorizontalRule` and `DefinitionTerm`
  variants, and `PathSegment` gained `DefinitionTerm`/`DefinitionPara`, so
  exhaustive matches over either in frontends need new arms.
- **Breaking:** `Theme` gained `definition_indent`, `definition_term_spacing` and
  `definition_term`; `tree_walk::LeafInfo` gained `definition_depth`. Frontends
  that construct either struct literally (rather than via `..Default::default()`)
  need the new fields.
- `Editor::outdent` is the new entry point for Shift-Tab, replacing direct calls
  to `outdent_list_item`. It routes a definition paragraph to the definition-list
  move and everything else to `outdent_list_item` unchanged, mirroring how
  `indent` already routed list items and adjacent-container nesting.
- The `tdoc` dependency is now `0.12`, which carries both horizontal rules and
  definition lists. This replaces the temporary git pin.
- Reveal-codes tags are now *drawn* as WordPerfect-style code boxes — an
  outlined, filled box whose pointed end faces the text the style applies to
  (right where it opens, left where it closes) — instead of being simulated with
  the bracket text `[Bold>` / `<Bold]`. The shape comes from the new
  `RenderContext::draw_reveal_tag`, which has a default implementation built
  from the existing fill/line primitives, so a pixel backend gets the boxes
  without doing anything; a backend with a polygon primitive can override it for
  an antialiased shape. Tag runs are laid out wider than their label to make
  room for the box's padding and point, and the caret steps over that full
  width. A tag rests on its line's text baseline — the *block's* font size, not
  the tag's — so tags in a heading sit on the words they mark instead of
  floating at the top of the taller line. (#11)
- New theme fields: `reveal_tag_border` (the box outline) and `reveal_tag_text`.
  **A character-cell backend must set `reveal_tag_text = true`**, which keeps
  the old bracketed-text tags — a box can't be drawn in a character grid.
  `reveal_tag_bg` also lightened to `0xDDDDD5FF` to suit a filled, outlined box.
  (#11)
- `tree_edit::paragraphs_into_list` is now `tree_edit::paragraphs_into_lists` and
  returns `Vec<Paragraph>`: a run can convert into more than one node when it
  holds a block that cannot become an item (a table). (#13)

### Fixed

- A list/checklist item whose content starts with a hard break (or is otherwise
  empty on its first visual line) now lays out all of its remaining lines. The
  marker-merge path bailed out to a marker-only line whenever the item's first
  visual line had no runs, dropping every following line from the layout — so a
  pasted text paragraph carrying hard breaks, converted to a list, rendered as a
  single empty bullet even though the document was intact. (#13)

- Converting a range of paragraphs into a list (`toggle_list` /
  `toggle_ordered_list` / `toggle_checklist`) or into a quote (`toggle_quote`) no
  longer drops the content of the container blocks it covers. Every non-list
  paragraph was flattened through `Paragraph::content()`, which is empty for a
  quote or a table, so those blocks turned into empty items and everything inside
  them was lost — selecting a document whose text sat in a trailing quote and
  hitting "List Item" left the first paragraph plus a single empty bullet. Quotes
  now contribute their paragraphs (recursively) as items of their own; a table,
  which has no item representation, stays where it is and splits the list instead
  of collapsing; and a quote/table/list becomes a quote child verbatim when
  quoting a range. The reverse direction (delisting, `dissolve_container`) keeps a
  container that is a list entry's body intact as well. (#13)

- Converting a range of paragraphs into a list or a quote no longer drops a
  definition list or a horizontal rule — the two block kinds #13 could not know
  about. A definition list owns no inline content, so, like a quote, it now
  contributes its terms and definition paragraphs as items of their own
  (recursively, so a quote inside a definition comes along) instead of collapsing
  into one empty item; for a checklist, whose items are spans only,
  `tree_edit::paragraph_as_spans` joins their text rather than coming back empty.
  A horizontal rule is a *leaf* with no inline content, which `content()`
  flattening turned into an empty paragraph — it now behaves like a table,
  staying where it is and splitting the list around it, and it survives being
  quoted or lifted out of a list entry intact.

- Wrapping a multi-paragraph selection in a single checklist item
  (`Editor::wrap_selection`) no longer runs the blocks' text together:
  `head`/`middle`/`tail` became `headmiddletail`. The separating space lived
  inside `paragraph_as_spans`, which the caller invoked once per paragraph, so it
  never applied between them; the new `tree_edit::paragraphs_as_spans` walks the
  whole run into one sink instead.

- Quoting or wrapping a selection that *ends* inside a container is no longer
  silently ignored. `toggle_quote`, `Editor::wrap_selection` and the definition-list
  toggle each required both ends of the selection to address a bare top-level
  paragraph, so a Select All on a document ending in a list, quote or definition
  list did nothing at all. They now take the whole top-level blocks the selection
  covers — the range the list toggles have used since #13 — and track the caret by
  its leaf ordinal within that range, so it stays on the line it was on instead of
  jumping to the first one. `set_block_type(BlockType::DefinitionTerm { .. })` with
  a multi-block selection goes to the same toggle rather than the cursor-only
  collapsed-container path, which would have lifted just one item out of its list.

- `toggle_quote` now toggles over a *range*, like the list toggles: a selection
  spanning several top-level blocks that are all quotes unquotes every one of
  them, and a partly-quoted range becomes a single quote (the quotes already
  there go in as children verbatim). The cursor's own block used to decide for
  the whole range, so a selection ending inside a quote unquoted that one and
  left the rest untouched. The result is left selected, so a second press undoes
  it. Toggling off also works from a caret *below* the quote child — inside a
  list or definition within the quote — where "Quote" previously wrapped the
  whole quote in a second one.
- A list toggle no longer does nothing when the caret sits below the top level.
  `toggle_list` / `toggle_ordered_list` / `toggle_checklist` needed a top-level
  paragraph to convert, so a caret in a quote's paragraph or a definition list
  was ignored:
  - In a quote, the list is now built *inside* that quote, around the children
    the selection covers (`tree_edit::children_into_lists`), and merges with an
    adjacent same-kind list there — so a quote's paragraphs can be bulleted one
    at a time into a single list. Toggling off delists back to quote paragraphs
    as before.
  - On a definition list's term or definition, the toggle routes to the same lift
    `set_block_type(BlockType::ListItem { .. })` uses, so the two agree: a term
    takes its whole item out of the list, a definition takes its own content out
    below it, and the new bullet is what that leaves behind.

- Lifting a list item out of a list that sits inside a definition no longer
  dropped it. `tree_edit::container_splice` — the splice every "leaf leaves its
  container" move ends in — knew the document top level, a quote's children and a
  list entry's paragraphs, but not a definition's, so it bailed out *after* the
  entries had been taken and the item was gone. It now understands a definition's
  paragraph vec (and `para_at`, the read-side walk, descends into one too, so the
  list toggles see a list nested in a definition at all).

- Tab / Shift-Tab over a selection that covers an item *and* its subitems no
  longer flattens the nesting. Every item in the selection was shifted on its
  own, so a subitem moved one level out at the same time as its parent — and the
  two steps are not the same step: the parent left its container while the
  subitem only lost a level, landing the two side by side. An item inside another
  item the selection covers now rides along with it, so the whole selected
  subtree moves one level and keeps the shape the author built. Shift-Tab on a
  selected checklist inside a definition is where this showed: the subitems came
  out level with their parents.

- Merging one list item into another no longer loses what hung below it, and
  **a checklist item's subitems are no longer deleted with it**. A checklist item
  holds its subitems inside itself, so `tree_edit::remove_node_at` took them
  along when the item went — Delete on an empty item, which merges the next item
  into it, dropped that item's whole subtree. Removing an item now leaves its
  subitems in its place, one level shallower, and a merge moves the merged item's
  body onto the item that absorbed its text (`move_item_body`): subitems become
  the absorbing item's, and a list entry's continuation paragraphs and sublists
  are inserted just after the paragraph they merged into, instead of standing
  under an item with none of their text left.

- Enter inside a list item now keeps the item's body with its text. A checklist
  item's subitems live inside the item, so they stayed with the half that kept
  the item struct: pressing Enter at the start of an item — where all of the text
  moves to the new item below — left the subitems hanging under the empty item
  above, sandwiched between an empty line and their own text. They now follow the
  text, as a list entry's continuation paragraphs and sublists already did.
  The other end of that rule is new for both kinds: when the new item gets *none*
  of the text (Enter at the very end of a line) it is a fresh sibling rather than
  the item's continuation, so it no longer takes the body along — a sublist stays
  under the line it belongs to instead of moving to the empty item below it.

## [0.5.0] - 2026-07-08

### Changed

- The default theme's `paragraph_spacing` is now `12` (was `5`). At the default
  17px line height the old value left a paragraph break barely distinguishable
  from a hard line break; `12` opens a clear gap between paragraphs. (#8)
- A heading's bottom margin is now `heading_bottom_margin` alone (default raised
  `10` → `15`) instead of `paragraph_spacing + heading_bottom_margin`.
  `layout_inline_block` no longer folds `paragraph_spacing` into its result — each
  block adds its own trailing gap at the call site — so raising the paragraph gap
  no longer inflates the space below headings. The net gap below a heading is
  unchanged from before the `paragraph_spacing` bump (was `5 + 10`, now `15`).
  Paragraph and block-quote trailing spacing are unchanged. (#8)

### Fixed

- Toggling a list kind (`toggle_list` / `toggle_ordered_list` / `toggle_checklist`)
  over a selection that spans several top-level paragraphs now produces the same
  result regardless of where the cursor sits in the selection or how the block
  kinds are mixed. Previously the outcome depended on the selection's direction:
  with the cursor in an existing list the whole thing was toggled *off*, and with
  the cursor in a plain paragraph the command silently did nothing when the other
  end of the selection reached into a list. The range now folds into a single list
  of the requested kind (plain paragraphs become items, other-kind lists are
  remapped, an adjacent same-kind list is absorbed), or, when the whole range is
  already that kind, delists back to plain paragraphs. (#9)
- A leaf block-type change (`set_block_type` to Paragraph / Heading / Code) over a
  selection spanning more than one block now converts *every* selected block, not
  just the one the cursor sits in. Previously, when the cursor was inside a
  collapsed single-line container (a list item, checklist item, or one-line quote)
  the change applied only to that block and ignored the rest of the selection —
  so, e.g., selecting three bullet items and pressing Heading 1 changed only one.
  Each selected block is now lifted out of its list/quote container and converted
  exactly as a single-block change would be, and a partial selection of a list's
  items splits the list around the converted ones. (#9)
- A list now leaves proper trailing space before the following block instead of
  the tight inter-item `list_item_spacing`. Previously a list hugged the next
  paragraph even though it sat well clear of the preceding one; where a list
  ends and non-list content resumes, the list is now separated by
  `paragraph_spacing`, matching the gap that precedes it. Spacing *between* list
  items is unchanged. Only affects the additive spacing model (the GUI); the
  classic cell-backend model, which zeroes these fields, is untouched. (#8)
- A continuation paragraph inside a list item now renders with correct vertical
  spacing. The break *between* an item's paragraphs now uses the full
  `paragraph_spacing`, so a multi-paragraph item reads as paragraphs rather than
  hugging like a wrapped line; and the gap *after* the item's last continuation
  paragraph, before the next item, is now the tight `list_item_spacing` instead
  of an extra paragraph gap. Previously the two gaps were swapped — the paragraph
  break was cramped while a stray paragraph-sized gap opened before the next
  item. Both the item's marker line and its continuation paragraphs now derive
  their trailing gap from the same list-aware rule. (#10)

## [0.4.1] - 2026-07-07

### Fixed

- Pasting a multi-paragraph fragment into a list item or quote child (any
  non-top-level cursor) via `insert_document` now preserves inline styling.
  Previously only a single-paragraph fragment was spliced run-by-run; a
  multi-paragraph fragment fell back to inserting the document's raw Markdown as
  literal text, so links and emphasis were lost (e.g. `[label](url)` showed up
  verbatim). Each fragment paragraph is now inserted run-by-run and separated by
  a structural break, so pasting N copied list items yields N styled sibling
  items rather than one block of escaped Markdown. (#7)

## [0.4.0] - 2026-07-07

### Changed

- Alt-Up/Down block moves (`move_blocks_up` / `move_blocks_down`) now cross
  container boundaries instead of stopping at a list's edge. Reordering *within*
  a list is unchanged, but a list/checklist item at the list's edge now leaves
  the list — carried as a same-kind single-item list that keeps its
  bullet/number/checkbox — and in one step moves past the block beyond the list
  (a heading, paragraph, …), then keeps hopping and merges into the next
  same-kind list it reaches — merging directly in when it lands beside one, so
  two adjacent same-kind lists never coexist (ordered lists stay continuously
  numbered) and each press advances the item by one visual line. A plain text
  paragraph that meets a list/quote is drawn into it, and a quote child at the
  quote's edge is lifted out. So an item can now be reordered across a whole note
  (e.g. moved from one checklist to another past an intervening heading) rather
  than only within its own list.
  Moves are a no-op only at the document's edge; sublist items nested inside a
  list item keep reordering within their sublist (Shift-Tab is still the way out
  of a nested sublist). `tree_edit::move_sibling` is superseded for this path by
  the new `tree_edit::move_block`. (#5)
- Alt-Up/Down now also moves a **multi-block selection** as a group: when the
  selection spans several sibling blocks (top-level paragraphs, list items,
  checklist items, or quote children) they reorder together and, at their
  container's edge, cross out of it together the same way a single block does —
  a run of list items leaves the list as a same-kind list and merges into the
  next one it reaches (checkboxes/numbering preserved), and a run of quote
  children is lifted out. The moved run stays selected. New `tree_edit`
  entry points: `move_block_range`, `rotate_children`, `container_child_count_at`.
  (#6)

### Fixed

- Converting a plain paragraph into a list item now merges it into an adjacent
  same-kind list instead of leaving a second, separate list beside it. Turning a
  fresh paragraph above (or below) an existing checklist into a checklist item —
  a common way to prepend a new entry — previously produced two adjacent
  checklists rather than one. The merge-with-neighbour step lived only in
  `set_block_type`, so the direct `toggle_list` / `toggle_ordered_list` /
  `toggle_checklist` entry points (used by menus and shortcuts) skipped it; it now
  lives in `toggle_list_kind` itself, so every caller folds into the neighbouring
  list, ordered lists renumber correctly, and only a list of the *same* kind is
  ever merged in. (#4)

## [0.3.2] - 2026-07-07

### Fixed

- Rebuilding a tdoc span tree from the flat inline model no longer fragments an
  outer style that fully contains a shorter differently-styled span (e.g.
  `**bold and <u>nderlined</u>**`, or `**~~struck~~ and bold**`). `inline_to_spans`
  previously nested styles in a *fixed* layer order, so an outer bold split into
  two sibling spans around an inner underline/strike (`**bold and <u>nderlined</u>**`
  → `**bold and **<u>**nderlined**</u>`), which tdoc serialized to a delimiter run
  that no longer parsed as emphasis — editing and saving such text corrupted it. It
  now factors out the style that spans the longest run first (ties broken by the
  canonical layer order), so the outer style wraps the inner one regardless of their
  source order and the round-trip stays stable. (#3)

## [0.3.1] - 2026-07-06

### Fixed

- Rebuilding a tdoc span tree from the flat inline model no longer emits two
  adjacent same-style spans when consecutive runs share an outer style (e.g.
  `~~**durch**gestrichen~~`, where only the first run is also bold). Previously
  each run was nested independently, producing sibling `Strike{…}` spans that
  tdoc serialized to a colliding delimiter run (`~~…~~~~…~~`, `****`) which no
  longer parsed as emphasis — so editing and saving such text corrupted it.
  `inline_to_spans` now factors shared styles (outermost first) into a single
  wrapping span, so the round-trip stays stable. As a side effect, a run that
  combines `code` with another style now nests correctly instead of dropping the
  other style. (#2)

## [0.3.0] - 2026-07-05

### Added

- **Caret affinity at inline-style boundaries.** At a style boundary (e.g. the
  seam between plain `Hello ` and bold `World!`) a single byte offset now denotes
  two navigable caret positions, distinguished by an `Affinity` (`Left`/`Right`):
  Left/Right arrow keys pause for the extra stop, `Editor::insert_text` inserts
  into the run on the affinity side (so typing there either joins the style or
  stays outside it), and the drawn caret leans toward that side (how the lean is
  drawn is up to the backend — see `RenderContext::draw_caret`). Active whenever
  reveal codes is *off* (reveal codes keeps its existing tag-by-tag stepping); the
  default `Left` affinity preserves the previous left-biased behavior. Toggleable via
  `Editor::set_style_boundary_stops` (on by default); when off, Left/Right step a
  plain grapheme, insertion is left-biased, and the caret is a plain bar. New
  public API: `Affinity`, `Editor::cursor_affinity`,
  `Editor::cursor_at_style_boundary`, `Editor::style_boundary_stops`,
  `Editor::set_style_boundary_stops`. (#1)
- **`RenderContext::draw_caret`** — backends now render the caret themselves, so
  the *design* of the affinity lean is backend-specific. The default is a plain
  bar plus short horizontal head and foot ticks (filled rects) pointing toward the
  lean; a backend can override it to draw something richer. Accompanied by the
  `CaretLean` enum (`None`/`Left`/`Right`). (#1)
- **`RenderContext::supports_caret_affinity`** (default `true`) — the capability
  gate for the affinity feature. A character-cell backend can't render a sub-cell
  lean (and usually drives the terminal's own caret), so it overrides this to
  `false`; the renderer syncs it onto the editor each layout pass and the two
  affinity stops collapse into the classic single caret — no extra navigation
  stop, no lean, left-biased insertion — regardless of `style_boundary_stops`.
  `Editor::set_affinity_supported` is the underlying knob (hosts don't normally
  call it directly). The monospace layout-snapshot suite now stands in for a cell
  backend and asserts affinity stays inert there. (#1)

## [0.2.1] - 2026-07-02

### Changed

- `Editor::move_blocks_up` / `move_blocks_down` now reorder the block at the
  cursor's *current* nesting level rather than only top-level paragraphs: list
  items (the whole entry, carrying its continuation paragraphs and sublists),
  checklist items, and quote children are all resorted among their siblings, and
  a nested sub-item stays within its sublist. No-op at a container's first/last
  boundary. Signatures are unchanged. Backed by a new `tree_edit::move_sibling`.

## [0.2.0] - 2026-07-02

A general "block model" for containers (quotes and lists), plus list-rendering
fixes. Additive to the public API.

### Added

- Pseudo-leaf / breadcrumb queries in `tree_walk`: `effective_block_type` (the
  effective block type at a path — a container holding a single text paragraph
  collapses to a leaf of the container's kind), `block_breadcrumb` (the
  outermost-to-innermost block-type chain), `container_block_at`, and
  `cursor_in_collapsed_container`. `Editor` exposes `cursor_effective_block_type`
  and `cursor_block_breadcrumb`.
- Container operations in `tree_edit`: `ContainerKind`, `convert_container`
  (quote ↔ list ↔ checklist, in place, at any depth), `dissolve_container` (lift
  a container's children up one level), `merge_adjacent_lists`, `delist_item`
  (lift a list item out into its enclosing container), `convert_list_item_range`
  (carve a contiguous run of items out of a list into a new list of another kind,
  splitting the original into up to three siblings), `split_leaf_continuation`
  (split a list item into a continuation paragraph within the same item), and
  `split_list_entry` (peel a list item's continuation paragraph off into a new
  item of the same list).
  `convert_container` / `dissolve_container` / `delist_item` also handle containers
  nested inside a list item, not just at the top level or in a quote.
- `Editor` block-model methods: `wrap_selection(ContainerKind)` (wrap the
  selection in a new container, preserving inner types), `indent` /
  `insert_continuation`, the "select parent" helpers `cursor_depth`,
  `container_block_at_depth`, `convert_container_at_depth`,
  `dissolve_container_at_depth`, `container_dissolvable_at_depth`, and the menu
  gating helpers `cursor_can_unnest`, `cursor_can_indent`,
  `cursor_can_nest_into_preceding`.
- `Theme::list_indent`: minimum horizontal indent per list nesting level, so a
  cell backend (whose fonts report `font_size == 0`) can still indent nested
  list items. Defaults to `0`, preserving the GUI's one-em-per-level metric.

### Changed

- `set_block_type` / `toggle_quote` now operate on the **pseudo-leaf**: converting
  to a quote (or list) flattens the block instead of nesting it (a heading becomes
  a plain quote), and a single-text container behaves like a leaf so block-type
  changes round-trip. Converting a list *item* to a quote/leaf affects only that
  item (splitting the list); converting between list kinds applies to the whole
  list for a plain cursor, but a selection spanning **several items of one list**
  carves just those items out into a new list of the target kind (splitting the
  original around them). Converting to a list merges with adjacent same-kind lists.
- `outdent_list_item` (`[` / Shift-Tab) also lifts a quote child out of its quote,
  and lifts an item of a checklist nested inside an ordered/unordered list entry
  back out to the outer list's level as a checklist (keeping its checkbox) — the
  inverse of nesting a checklist under a bullet item, instead of delisting it into
  a plain text paragraph. It further lifts a list item whose list sits **directly
  in a quote** out of that quote *keeping its bullet* (splitting the quote around
  it, via the new `exit_quote_list_item`) — the inverse of Tab nesting a list item
  into a preceding quote. Contexts that mean "stop being a list item" (Enter on an
  empty item, toggling a list off) instead use the new
  `outdent_list_item_delisting`, which drops such an item into the quote as a plain
  paragraph. And `indent` nests the selected paragraph(s) into an
  adjacent container — appended to a container immediately before them, or prepended
  to one immediately after — each paragraph becoming a new list item / checklist item
  / quote child. This works both at the document top level and among a quote's
  children (a plain paragraph inside a quote nests into a list that is also inside
  that quote), not just at the top level. (`nest_into_preceding_container` →
  `nest_selection_into_adjacent`; `cursor_can_nest_into_preceding` →
  `can_nest_selection_into_adjacent`; new `tree_edit::add_paragraphs_to_container`,
  `has_adjacent_container`, and `nest_paragraphs_into_adjacent`.)
- `indent_list_item` merges the indented item into whatever ordered/unordered
  sublist already ends the previous item, regardless of kind (a bullet indented
  under an item ending in a numbered sublist joins that numbered sublist, and vice
  versa), instead of starting a second sublist beside it. New
  `indent_list_item_or_merge` additionally lets the *first* item of a top-level
  list that follows another list indent straight into that preceding list (merging
  under its last item), which the editor's `indent` now uses. This also covers a
  *checklist* following an ordered/unordered list: its first item nests under the
  preceding list's last item as a checklist sublist (checkboxes preserved),
  reusing a trailing checklist so a whole selected run collects into one sublist
  rather than staircasing. And when a list's first item follows a **quote** instead
  of another list, `indent_list_item_or_merge` now nests that item *into* the quote
  while keeping it a list item — as an entry of a list child of the quote (joining a
  trailing list there, else a new list of the same kind) — pruning the outer list if
  it empties, so a bullet item directly below a quote can be pulled into the quote
  with Tab and stay a bullet.

### Fixed

- `insert_newline` (Enter) in an empty paragraph now promotes it one structural
  level per press instead of dissolving its enclosing list item. An empty
  *continuation* paragraph in a multi-paragraph list item splits off into a new
  item (via the new `split_list_entry`) rather than lifting the item's other
  paragraphs out with it; a genuinely empty item still exits its list; and an
  empty quote child now exits the quote as well (previously Enter there created
  another empty quote child). So repeated Enter walks an empty leaf out one
  container at a time.
- Nested list items now indent per level in a cell backend (the renderer used the
  font `font_size` as the per-level step, which cell backends force to `0`, so
  nesting collapsed flat). See `Theme::list_indent`.
- Content inside a list (continuation paragraphs, code blocks) aligns with the
  item's text rather than a fixed bullet width, and ordered-list number padding is
  computed across the whole list — fixing misaligned continuations and
  inconsistent padding in numbered lists with two-digit numbers.

## [0.1.0] - 2026-07-01

Initial release — the rendering-agnostic editing and layout core carved out of
the [Piki](https://github.com/roblillack/piki) editor so it can be shared
across multiple tools.

### Added

- `Editor`: the editing engine. Owns the authoritative `tdoc::Document` tree
  plus cursor/selection state and performs every mutation — typing, styling,
  structural edits, and undo/redo.
- `Renderer`: a two-phase layout/paint engine. Layout turns the document plus
  font metrics into positioned lines, runs, and table grids; paint walks the
  layout and emits drawing primitives. Owns the view state a host needs —
  viewport/scroll, cursor blink, link hover, search, and hit-testing.
- `RenderContext`: the backend trait a frontend implements to supply real text
  metrics and drawing primitives, keeping the engine independent of any UI
  toolkit.
- insta-based SVG snapshot tests run under both proportional and monospace
  metric regimes, plus layout/edit performance benchmarks.

<!-- next-url -->
[Unreleased]: https://github.com/roblillack/rutle/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/roblillack/rutle/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/roblillack/rutle/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/roblillack/rutle/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/roblillack/rutle/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/roblillack/rutle/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/roblillack/rutle/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/roblillack/rutle/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/roblillack/rutle/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/roblillack/rutle/releases/tag/v0.1.0
