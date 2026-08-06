#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontType {
    Heading,
    Content,
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontStyle {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

/// Which end of a styled span a reveal-codes tag marks. The pointed end of the
/// drawn tag faces the text the style applies to: right for `Open`, left for
/// `Close`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealTagKind {
    /// A style starts here (`[Bold>`).
    Open,
    /// A style ends here (`<Bold]`).
    Close,
}

/// Geometry and colors of one reveal-codes tag, handed to
/// [`RenderContext::draw_reveal_tag`]. The label is drawn by the engine on top
/// of the shape, inset by `point` on the pointed side.
#[derive(Debug, Clone, Copy)]
pub struct RevealTag {
    /// Top-left of the tag's bounding box.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Width of the pointed end (right for `Open`, left for `Close`), included
    /// in `width`.
    pub point: i32,
    pub kind: RevealTagKind,
    /// Interior fill (`Theme::reveal_tag_bg`).
    pub fill: u32,
    /// Outline (`Theme::reveal_tag_border`).
    pub border: u32,
}

/// Which way a caret leans to signal inline-style affinity at a boundary. The
/// *rendering* of the lean is up to the backend (see [`RenderContext::draw_caret`]);
/// this only says which side newly typed text will take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaretLean {
    /// Plain caret — no lean.
    None,
    /// Leans left: newly typed text takes the run to the left.
    Left,
    /// Leans right: newly typed text takes the run to the right.
    Right,
}

// Drawing backend trait - abstracts over FLTK's drawing primitives
pub trait RenderContext {
    fn set_color(&mut self, color: u32);
    fn set_font(&mut self, font: FontType, style: FontStyle, size: u8);
    fn draw_text(&mut self, text: &str, x: i32, y: i32);
    fn draw_rect_filled(&mut self, x: i32, y: i32, w: i32, h: i32);
    fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32);
    fn text_width(&mut self, text: &str, font: FontType, style: FontStyle, size: u8) -> f64;
    fn text_height(&self, font: FontType, style: FontStyle, size: u8) -> i32;
    fn text_descent(&self, font: FontType, style: FontStyle, size: u8) -> i32;
    fn push_clip(&mut self, x: i32, y: i32, w: i32, h: i32);
    fn pop_clip(&mut self);
    fn color_average(&self, c1: u32, c2: u32, weight: f32) -> u32;
    fn color_contrast(&self, fg: u32, bg: u32) -> u32;
    fn color_inactive(&self, c: u32) -> u32;
    fn has_focus(&self) -> bool;
    fn is_active(&self) -> bool;

    /// Pen state for text decorations, consulted by the next [`Self::draw_text`]. The
    /// default ignores them — pixel backends draw underline/strikethrough as
    /// separate lines. A cell backend overrides these to fold the decoration
    /// into the glyph's attributes (see [`crate::theme::Theme::text_decoration_lines`]).
    fn set_underline(&mut self, _on: bool) {}
    fn set_strikethrough(&mut self, _on: bool) {}

    /// Whether this backend can render a caret that leans to signal inline-style
    /// affinity (see [`crate::Affinity`]). A pixel canvas can draw the sub-cell
    /// lean, so the default is `true`. A character-cell backend cannot — its caret
    /// is one indivisible cell, and it typically drives the terminal's own hardware
    /// caret rather than [`Self::draw_caret`] — so it overrides this to `false`.
    ///
    /// When it returns `false` the engine collapses the two affinity stops back
    /// into one: Left/Right step a plain grapheme, insertion stays left-biased, and
    /// no caret ever leans — regardless of the user-facing
    /// [`crate::Editor::set_style_boundary_stops`] toggle (both must be on for
    /// affinity to take effect). The renderer keeps the editor's capability in sync
    /// with this on every layout pass, so a backend only needs to answer honestly.
    fn supports_caret_affinity(&self) -> bool {
        true
    }

    /// Draw the text caret: a 2px-wide vertical bar `height` tall with its
    /// top-left at (x, y), in the active color. When `lean` is `Left`/`Right`, mark
    /// the inline-style affinity (see [`crate::Affinity`]) by leaning the caret
    /// toward that side.
    ///
    /// The *design* of the lean is deliberately backend-specific. The default is a
    /// plain, cheap indicator: the bar plus short horizontal "head" and "foot"
    /// ticks pointing toward the lean, built from filled rects only — right for any
    /// pixel canvas. A backend can override this to render something richer (e.g. a
    /// filled bracket) or, for a character cell, stamp a glyph.
    fn draw_caret(&mut self, x: i32, y: i32, height: i32, lean: CaretLean) {
        self.draw_rect_filled(x, y, 2, height);
        let tick_len = 4;
        let tick_h = 2;
        let tick_x = match lean {
            CaretLean::None => return,
            CaretLean::Left => x - tick_len,
            CaretLean::Right => x,
        };
        let tick_w = tick_len + 2;
        self.draw_rect_filled(tick_x, y, tick_w, tick_h); // head tick
        self.draw_rect_filled(tick_x, y + height - tick_h, tick_w, tick_h); // foot tick
    }

    /// Draw the shape of one reveal-codes tag: a filled, outlined box with one
    /// pointed end, the way WordPerfect (and classic Pure) drew its codes. The
    /// engine draws the label on top afterwards, so this only paints the shape.
    ///
    /// The default builds it from the basic primitives — the body and the point
    /// filled scanline by scanline, the outline from lines — which is right for
    /// any pixel canvas. A backend with a polygon primitive can override this to
    /// get an antialiased shape (see piki's FLTK context); a character-cell
    /// backend never sees this call, because it sets `Theme::reveal_tag_text`
    /// and gets the classic `[Bold>` text form instead.
    fn draw_reveal_tag(&mut self, tag: RevealTag) {
        if tag.width <= 0 || tag.height <= 0 {
            return;
        }
        // Horizontal inset of the pointed side at row `dy`: zero at the tip
        // (vertical center), the full point width at the top and bottom edges.
        let last = (tag.height - 1).max(1);
        let center = last as f32 / 2.0;
        let inset = |dy: i32| -> i32 {
            let d = ((dy as f32 - center).abs() / center).min(1.0);
            (tag.point as f32 * d).round() as i32
        };

        self.set_color(tag.fill);
        for dy in 0..tag.height {
            let cut = inset(dy);
            let w = tag.width - cut;
            if w <= 0 {
                continue;
            }
            let x = match tag.kind {
                RevealTagKind::Open => tag.x,
                RevealTagKind::Close => tag.x + cut,
            };
            self.draw_rect_filled(x, tag.y + dy, w, 1);
        }

        // Outline: the two flat edges, the blunt side, and the two slants
        // meeting at the tip.
        self.set_color(tag.border);
        let (top, bottom) = (tag.y, tag.y + tag.height - 1);
        let tip_y = tag.y + (tag.height - 1) / 2;
        let (blunt, tip, flat_near, flat_far) = match tag.kind {
            RevealTagKind::Open => (
                tag.x,
                tag.x + tag.width - 1,
                tag.x,
                tag.x + tag.width - 1 - tag.point,
            ),
            RevealTagKind::Close => (
                tag.x + tag.width - 1,
                tag.x,
                tag.x + tag.point,
                tag.x + tag.width - 1,
            ),
        };
        self.draw_line(flat_near, top, flat_far, top);
        self.draw_line(flat_near, bottom, flat_far, bottom);
        self.draw_line(blunt, top, blunt, bottom);
        self.draw_line(flat_far, top, tip, tip_y);
        self.draw_line(flat_far, bottom, tip, tip_y);
    }

    /// Draw a checklist checkbox of `size` at (x, y) in the active color.
    ///
    /// The default draws a square outline (and a check mark when `checked`) from
    /// line primitives — right for a pixel canvas. A character-cell backend can
    /// override this to stamp a single glyph instead, since a multi-line square
    /// collapses badly into one cell.
    fn draw_checkbox(&mut self, x: i32, y: i32, size: i32, checked: bool) {
        let box_right = x + size;
        let box_bottom = y + size;
        self.draw_line(x, y, box_right, y);
        self.draw_line(x, y, x, box_bottom);
        self.draw_line(x, box_bottom, box_right, box_bottom);
        self.draw_line(box_right, y, box_right, box_bottom);
        if checked {
            let mut inset = ((size as f32) * 0.2).round() as i32;
            if inset < 2 {
                inset = 2;
            }
            if inset * 2 >= size {
                inset = size / 2;
            }
            let x1 = x + inset;
            let y1 = y + inset;
            let x2 = box_right - inset;
            let y2 = box_bottom - inset;
            self.draw_line(x1, y1, x2, y2);
            self.draw_line(x1, y2, x2, y1);
        }
    }
}
