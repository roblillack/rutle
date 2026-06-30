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

pub mod editor;
pub mod inline_convert;
pub mod render_context;
pub mod renderer;
pub mod reveal;
pub mod structured_document;
pub mod theme;
pub mod tree_edit;
pub mod tree_path;
pub mod tree_walk;

// Markdown/HTML (de)serialization lives in `tdoc`; rutle works on `tdoc::Document`
// directly. These thin `tdoc` wrappers exist only to support the test suite, so
// the module is compiled for tests only and is not part of the public API.
#[cfg(test)]
mod markdown_converter;

// Convenience re-exports for the most common entry points.
pub use editor::{EditError, Editor, UndoKind};
pub use render_context::{FontStyle, FontType, RenderContext};
pub use renderer::Renderer;
pub use structured_document::{
    Block, BlockType, InlineContent, Link, TableCell, TableRow, TextRun, TextStyle,
};
pub use theme::{FontSettings, Theme};
pub use tree_path::{DocumentPosition, PathSegment, TreePath};
