# rutle

**R**ob's **U**niversal **T**ext **L**ayout **E**ngine — a rendering-agnostic
structured editor and layout core for [`tdoc::Document`].

`rutle` owns a `tdoc::Document` tree and provides everything needed to edit and
lay it out without committing to any particular UI toolkit:

- **`StructuredEditor`** — the authoritative document model. It owns the tree and
  performs every mutation (typing, styling, structural edits, undo/redo), so
  frontends never mutate the document directly.
- **`StructuredRichDisplay`** — lays a document out against a backend-agnostic
  **`DrawContext`** (wrapping, cursor/caret positioning, selection geometry,
  tables). It never draws pixels itself; it asks the `DrawContext` to measure
  and emit primitives.
- **`DrawContext`** — the trait a frontend implements to plug in real font
  metrics and drawing. The same engine drives any backend: a GUI toolkit
  (e.g. FLTK), a terminal cell grid, or the SVG renderer used by the snapshot
  tests in [`tests/`](tests/).

This crate was extracted from the [Piki](https://github.com/roblillack/piki)
editor so the editing/layout core can be shared across multiple tools.

## Usage

```toml
[dependencies]
rutle = "0.1"
```

```rust
use rutle::StructuredEditor;

// The editor owns the document; load it from Markdown and mutate via its API.
let mut editor = StructuredEditor::default();
editor.load_markdown("# Hello\n\nSome **bold** text.");

// To render, drive a `StructuredRichDisplay` against your own `DrawContext`
// implementation (see `tests/common/mod.rs` for an SVG reference backend).
```

## Testing

```sh
cargo test     # behavior + insta SVG snapshot tests
cargo bench     # layout/edit performance benchmarks
```

## License

MIT
