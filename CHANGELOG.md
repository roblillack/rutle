# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While pre-1.0, the minor version is bumped for breaking changes.

<!-- next-header -->

## [Unreleased] - ReleaseDate

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
  other style.

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
[Unreleased]: https://github.com/roblillack/rutle/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/roblillack/rutle/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/roblillack/rutle/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/roblillack/rutle/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/roblillack/rutle/releases/tag/v0.1.0
