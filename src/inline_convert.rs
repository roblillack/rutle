// Lossless conversion between a leaf's inline content and tdoc spans.
//
// tdoc represents inline content as a tree of `Span`s where each node carries a single
// `InlineStyle` and styling combinations are expressed by nesting. The editor/display
// work with a flat `Vec<InlineContent>` of styled `TextRun`s (a style flag-set per run),
// which is convenient for cursor math, wrapping, and hit-testing.
//
// `spans_to_inline` flattens the span tree (accumulating styles down each branch) and
// `inline_to_spans` rebuilds it in a canonical nesting order. The two are inverses up to
// that normalization, so an edit→serialize cycle is stable. Line breaks are carried as a
// literal `"\n"` in span text (matching tdoc) and as `InlineContent::HardBreak` in runs.

use super::structured_document::{InlineContent, Link, TextRun, TextStyle};
use tdoc::inline::{InlineStyle, Span};

/// Flatten a tdoc span tree into the editor's flat inline representation.
pub(crate) fn spans_to_inline(spans: &[Span]) -> Vec<InlineContent> {
    let mut content = Vec::new();
    for span in spans {
        span_to_inline_internal(span, TextStyle::plain(), &mut content);
    }
    content
}

/// Rebuild a tdoc span tree from the editor's flat inline representation.
///
/// Consecutive text runs that share an outer style are wrapped in a *single*
/// span for that style rather than one span per run. Emitting one span per run
/// would place two same-style spans side by side (e.g. `Strike{…}` next to
/// `Strike{…}`), which serializes to a colliding delimiter run (`~~…~~~~…~~`,
/// `****`) that no longer parses. Factoring shared styles out keeps the tree —
/// and the Markdown produced from it — round-trippable.
pub(crate) fn inline_to_spans(content: &[InlineContent]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut idx = 0;
    while idx < content.len() {
        match &content[idx] {
            // Gather a maximal run of adjacent text runs and rebuild them
            // together so shared styles can be factored across the boundary.
            InlineContent::Text(_) => {
                let start = idx;
                while let Some(InlineContent::Text(_)) = content.get(idx) {
                    idx += 1;
                }
                let runs: Vec<&TextRun> = content[start..idx]
                    .iter()
                    .filter_map(|item| match item {
                        InlineContent::Text(run) => Some(run),
                        _ => None,
                    })
                    .collect();
                spans.extend(runs_to_spans(&runs, 0));
            }
            InlineContent::Link { link, content } => {
                let mut span =
                    Span::new_styled(InlineStyle::Link).with_children(inline_to_spans(content));
                if !link.destination.is_empty() {
                    span = span.with_link_target(link.destination.clone());
                }
                spans.push(span);
                idx += 1;
            }
            InlineContent::HardBreak => {
                spans.push(Span::new_text("\n"));
                idx += 1;
            }
        }
    }
    spans
}

/// Emphasis styles applied by [`runs_to_spans`], ordered outermost → innermost.
/// This fixed order defines the canonical nesting and mirrors the inverse
/// flattening in `spans_to_inline`, so an edit→serialize cycle is stable.
const STYLE_LAYERS: [InlineStyle; 5] = [
    InlineStyle::Highlight,
    InlineStyle::Underline,
    InlineStyle::Strike,
    InlineStyle::Bold,
    InlineStyle::Italic,
];

fn style_has_layer(style: &TextStyle, layer: InlineStyle) -> bool {
    match layer {
        InlineStyle::Highlight => style.highlight,
        InlineStyle::Underline => style.underline,
        InlineStyle::Strike => style.strikethrough,
        InlineStyle::Bold => style.bold,
        InlineStyle::Italic => style.italic,
        _ => false,
    }
}

/// Rebuild spans for a slice of consecutive text runs, factoring the style at
/// `layer` (and, recursively, the inner layers) out of maximal groups of runs
/// that share it.
fn runs_to_spans(runs: &[&TextRun], layer: usize) -> Vec<Span> {
    if runs.is_empty() {
        return Vec::new();
    }

    let Some(&style) = STYLE_LAYERS.get(layer) else {
        // All emphasis layers consumed; emit the leaves.
        return runs.iter().flat_map(|run| leaf_run_to_spans(run)).collect();
    };

    let mut spans = Vec::new();
    let mut i = 0;
    while i < runs.len() {
        let active = style_has_layer(&runs[i].style, style);
        let start = i;
        while i < runs.len() && style_has_layer(&runs[i].style, style) == active {
            i += 1;
        }
        let group = &runs[start..i];
        if active {
            let children = runs_to_spans(group, layer + 1);
            spans.push(Span::new_styled(style).with_children(children));
        } else {
            spans.extend(runs_to_spans(group, layer + 1));
        }
    }
    spans
}

/// Emit the leaf spans for a fully-unwrapped run: a code span, or plain text
/// with any embedded newlines split out (matching tdoc's representation).
fn leaf_run_to_spans(run: &TextRun) -> Vec<Span> {
    if run.style.code {
        return vec![Span::new_styled(InlineStyle::Code).with_text(&run.text)];
    }

    let mut spans = Vec::new();
    let mut buffer = String::new();
    for ch in run.text.chars() {
        if ch == '\n' {
            if !buffer.is_empty() {
                spans.push(Span::new_text(std::mem::take(&mut buffer)));
            }
            spans.push(Span::new_text("\n"));
        } else {
            buffer.push(ch);
        }
    }
    if !buffer.is_empty() {
        spans.push(Span::new_text(buffer));
    }

    spans
}

fn span_to_inline_internal(span: &Span, active: TextStyle, out: &mut Vec<InlineContent>) {
    let mut style = active;
    match span.style {
        InlineStyle::Bold => style.bold = true,
        InlineStyle::Italic => style.italic = true,
        InlineStyle::Underline => style.underline = true,
        InlineStyle::Strike => style.strikethrough = true,
        InlineStyle::Highlight => style.highlight = true,
        InlineStyle::Code => style.code = true,
        InlineStyle::Link | InlineStyle::None => {}
    }

    if span.style == InlineStyle::Link {
        let mut inner = Vec::new();
        if !span.text.is_empty() {
            push_text(&mut inner, &span.text, style);
        }
        for child in &span.children {
            span_to_inline_internal(child, style, &mut inner);
        }
        // A link nested inside a link is degenerate — markdown's
        // `[![alt](img)](url)` (an image inside a link) parses to exactly this,
        // since tdoc has no image type. Flatten the inner link(s) so the outer
        // link wraps plain styled runs: the content stays editable/stylable and
        // renders (and reveals) as a single link instead of an opaque, unstylable
        // blob. The outer link's destination (the clickable target) is kept.
        let inner = flatten_nested_links(inner);
        let link = Link {
            destination: span.link_target.clone().unwrap_or_default(),
            title: None,
        };
        out.push(InlineContent::Link {
            link,
            content: inner,
        });
        return;
    }

    if !span.text.is_empty() {
        push_text(out, &span.text, style);
    }
    for child in &span.children {
        span_to_inline_internal(child, style, out);
    }
}

/// Replace any link nested within `items` with its content (recursively), so the
/// flat inline model never contains a link inside a link.
fn flatten_nested_links(items: Vec<InlineContent>) -> Vec<InlineContent> {
    let mut out = Vec::new();
    for item in items {
        match item {
            InlineContent::Link { content, .. } => out.extend(flatten_nested_links(content)),
            other => out.push(other),
        }
    }
    out
}

fn push_text(out: &mut Vec<InlineContent>, text: &str, style: TextStyle) {
    if text.is_empty() {
        return;
    }

    if style.code {
        append_text_run(out, text.to_string(), style);
        return;
    }

    let mut start = 0;
    let chars: Vec<char> = text.chars().collect();
    for (idx, ch) in chars.iter().enumerate() {
        if *ch == '\n' {
            if idx > start {
                let segment: String = chars[start..idx].iter().collect();
                append_text_run(out, segment, style);
            }
            out.push(InlineContent::HardBreak);
            start = idx + 1;
        }
    }

    if start < chars.len() {
        let segment: String = chars[start..].iter().collect();
        append_text_run(out, segment, style);
    }
}

fn append_text_run(out: &mut Vec<InlineContent>, text: String, style: TextStyle) {
    if text.is_empty() {
        return;
    }

    if let Some(InlineContent::Text(run)) = out.last_mut()
        && run.style == style
    {
        run.text.push_str(&text);
        return;
    }

    out.push(InlineContent::Text(TextRun::new(text, style)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bold_italic() -> TextStyle {
        TextStyle {
            bold: true,
            italic: true,
            ..Default::default()
        }
    }

    #[test]
    fn combined_style_round_trips() {
        let content = vec![InlineContent::Text(TextRun::new("hi", bold_italic()))];
        let spans = inline_to_spans(&content);
        let back = spans_to_inline(&spans);
        assert_eq!(back, content);
    }

    #[test]
    fn styled_link_round_trips() {
        let content = vec![InlineContent::Link {
            link: Link {
                destination: "https://example.test".to_string(),
                title: None,
            },
            content: vec![InlineContent::Text(TextRun::new(
                "click",
                TextStyle::bold(),
            ))],
        }];
        let spans = inline_to_spans(&content);
        let back = spans_to_inline(&spans);
        assert_eq!(back, content);
    }

    #[test]
    fn hardbreak_round_trips() {
        let content = vec![
            InlineContent::Text(TextRun::plain("a")),
            InlineContent::HardBreak,
            InlineContent::Text(TextRun::plain("b")),
        ];
        let spans = inline_to_spans(&content);
        let back = spans_to_inline(&spans);
        assert_eq!(back, content);
    }

    /// Adjacent runs sharing an outer style must rebuild into a single wrapping
    /// span, not two siblings — otherwise the Markdown serializer emits a
    /// colliding `~~…~~~~…~~` delimiter run. This models `~~**durch**gestrichen~~`,
    /// where only the first run is also bold.
    #[test]
    fn adjacent_shared_style_is_factored() {
        let strike = TextStyle {
            strikethrough: true,
            ..Default::default()
        };
        let strike_bold = TextStyle {
            strikethrough: true,
            bold: true,
            ..Default::default()
        };
        let content = vec![
            InlineContent::Text(TextRun::new("durch", strike_bold)),
            InlineContent::Text(TextRun::new("gestrichen", strike)),
        ];

        let spans = inline_to_spans(&content);

        // One Strike span wrapping both runs, with the bold nested inside it.
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style, InlineStyle::Strike);
        assert_eq!(spans[0].children.len(), 2);
        assert_eq!(spans[0].children[0].style, InlineStyle::Bold);
        assert_eq!(spans[0].children[1].style, InlineStyle::None);

        // The rebuild is the exact inverse of the flatten.
        assert_eq!(spans_to_inline(&spans), content);
    }

    #[test]
    fn adjacent_bold_runs_share_one_span() {
        let content = vec![
            InlineContent::Text(TextRun::new("a", TextStyle::bold())),
            InlineContent::Text(TextRun::new("b", TextStyle::bold())),
        ];
        let spans = inline_to_spans(&content);
        // A single bold span, not two siblings (which would emit `**a****b**`).
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style, InlineStyle::Bold);
        // Flattening coalesces the two same-style runs into one.
        assert_eq!(
            spans_to_inline(&spans),
            vec![InlineContent::Text(TextRun::new("ab", TextStyle::bold()))]
        );
    }
}
