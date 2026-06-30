//! `rutle` — Rob's Universal Text Layout Engine: a rendering-agnostic structured
//! editor and layout core for [`tdoc::Document`].
//!
//! The engine is organized as four layers, each depending only on the ones
//! below it, so the same document logic drives any frontend:
//!
//! 1. [`Editor`] — the **editing engine**. Owns the authoritative
//!    [`tdoc::Document`] tree plus cursor/selection state and performs every
//!    mutation (typing, styling, structural edits, undo/redo). The source of
//!    truth; a host calls into it to change the document.
//! 2. **Layout** — the layout phase of [`Renderer`] (`layout_*`): turns the
//!    document plus font metrics into positioned lines, runs, and table grids.
//! 3. **Paint** — the rendering phase of [`Renderer`] (`draw_*`): walks the
//!    layout and emits drawing primitives. [`Renderer`] additionally owns the
//!    view state a host needs — viewport/scroll, cursor blink, link hover,
//!    search, and hit-testing.
//! 4. [`RenderContext`] — the **backend** trait a frontend implements to supply
//!    real text metrics (`text_width`/`text_height`/…) and drawing
//!    (`draw_text`/`draw_rect_filled`/…). The engine is agnostic to it: a GUI
//!    toolkit, a terminal cell grid, or the SVG backend used by the snapshot
//!    tests all plug in here.
//!
//! This crate is the shared editing/layout core carved out of the
//! [Piki](https://github.com/roblillack/piki) editor.

pub mod render_context;
pub mod richtext;
pub mod theme;

// Convenience re-exports for the most common entry points.
pub use render_context::{FontStyle, FontType, RenderContext};
pub use richtext::editor::{EditError, Editor, UndoKind};
pub use richtext::renderer::Renderer;
pub use richtext::structured_document::{
    Block, BlockType, InlineContent, Link, TableCell, TableRow, TextRun, TextStyle,
};
pub use richtext::tree_path::{DocumentPosition, PathSegment, TreePath};
pub use theme::{FontSettings, Theme};
