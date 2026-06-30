# rutle

**R**ob's **U**niversal **T**ext **L**ayout **E**ngine — a rendering-agnostic
structured editor and layout core for [`tdoc::Document`].

`rutle` owns a `tdoc::Document` tree and provides everything needed to edit and
lay it out **without committing to any UI toolkit**. The same engine drives a
desktop GUI, a terminal UI, or an offscreen SVG renderer — a frontend only has
to supply text metrics and drawing primitives.

## The four layers

The engine is a stack of four layers. Each depends only on the ones below it,
and a frontend plugs in at the bottom.

To build a text editor using rutle, the following stack of four layers is
needed. The engine itself provides implementations for the editor and renderer
an application will need to host them and provide a fitting render context:

1. Host application – receives keystrokes, mouse events; runs paint loop; drives
   the editor.
2. `Editor` — the **editing engine**. Owns the authoritative
   `tdoc::Document` tree plus cursor/selection state and performs every mutation
   (typing, styling, structural edits, undo/redo). A host never touches the
   document directly; it calls into the `Editor`.
3. `Renderer` – reads the `Document`, measures and draws it in two phases:
   - **Layout:** turns the document plus font metrics into positioned lines,
     runs, and table grids.
   - **Paint:** walks the layout and emits drawing primitives. This phase
     owns the view state a host needs:
     viewport/scroll, cursor blink, link hover, search, and hit-testing.
4. `RenderContext` — the **backend** trait a frontend implements to supply
   real text metrics (`text_width`/`text_height`/…) and drawing
   (`draw_text`/`draw_rect_filled`/…).

## Usage

```toml
[dependencies]
rutle = "0.1"
tdoc = "0.11"   # the document model rutle reads and edits
```

rutle works on a [`tdoc::Document`](https://docs.rs/tdoc) and nothing else —
(de)serialization is `tdoc`'s job. Build a document however you like (e.g.
`tdoc::markdown::parse` or `tdoc::html::parse`, or construct one by hand) and
hand it to the editor, which owns it and applies every mutation:

```rust
use rutle::Editor;

let mut editor = Editor::default();
editor.set_document(document); // document: tdoc::Document
editor.toggle_heading().unwrap();

let edited: &tdoc::Document = editor.document();
```

To display a document, hand it to a `Renderer` and drive it against your own
`RenderContext`:

```rust
use rutle::Renderer;

// `MyBackend: RenderContext` supplies font metrics + drawing for your toolkit.
let mut renderer = Renderer::new(0, 0, 800, 600);
renderer.editor_mut().set_document(document); // document: tdoc::Document
renderer.draw(&mut backend);              // backend: &mut impl RenderContext
```

Implementing `RenderContext` means measuring and drawing text/rectangles/lines
for your platform. See [`tests/common/mod.rs`](tests/common/mod.rs) for a
complete SVG reference backend.

## Testing

```sh
cargo test     # behavior + insta SVG snapshot tests (proportional + monospace)
cargo bench    # layout/edit performance benchmarks
```

The snapshot tests render documents through an SVG `RenderContext` under two
metric regimes (kerned proportional and a synthetic monospace cell grid), so
they exercise the shared wrap/cursor/selection/table math the real backends
impose without depending on any specific toolkit.

## Provenance

`rutle` is the editing/layout core carved out of the
[Piki](https://github.com/roblillack/piki) editor so it can be shared across
multiple tools.

## License

MIT
