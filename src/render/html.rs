//! HTML serializer for the document model.
//!
//! Where Markdown has to approximate, HTML says it outright: merged table
//! cells keep their `colspan`/`rowspan`, list markers keep their numbering
//! style, and embedded images show inline as `data:` URIs (or link to files
//! the caller writes out). Headings get the same ids the Markdown writer's
//! anchors use, so internal links resolve the same way in both.
//!
//! Output is safe to open from an untrusted document: all text and
//! attributes are escaped, and only inert URL schemes survive as links.

use crate::model::{
    Asset, Block, Cell, CellSlot, Document, ImageSource, Inline, LinkTarget, List, MarkerKind,
    Note, Style, Table, TableKind, inlines_are_empty, inlines_to_plain_text,
};
use crate::render::markdown::anchors::{AnchorMap, resolve_anchors};
use crate::render::markdown::{NoteNumbers, number_notes};
use std::collections::HashSet;
use std::fmt::Write as _;

/// How [`document_to_html`](crate::document_to_html) writes a document.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct HtmlOptions {
    /// Write only the body content, without the `<!DOCTYPE html>` page
    /// around it, for embedding in a page of your own.
    pub fragment: bool,
    /// Where embedded images point. `None` (the default) inlines their bytes
    /// as `data:` URIs, so the page is self-contained. `Some(prefix)` links
    /// each to `{prefix}{index}.{extension}` instead, with the index into
    /// [`Document::assets`] and [`Asset::extension`], for callers that write
    /// the assets out as files.
    pub asset_prefix: Option<String>,
}

struct Ctx<'d> {
    nums: NoteNumbers,
    anchors: AnchorMap,
    assets: &'d [Asset],
    options: &'d HtmlOptions,
    /// Notes whose first reference already carries the back-link id.
    referenced: std::cell::RefCell<HashSet<usize>>,
}

pub fn document_to_html(doc: &Document, options: &HtmlOptions) -> String {
    let rc = Ctx {
        nums: number_notes(doc),
        anchors: resolve_anchors(doc),
        assets: &doc.assets,
        options,
        referenced: Default::default(),
    };
    let mut body = String::new();
    render_blocks(&doc.blocks, &rc, &mut body);
    render_notes(doc, &rc, &mut body);
    if options.fragment {
        return body;
    }
    let title = doc
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Heading { content, .. } => {
                let text = inlines_to_plain_text(content);
                let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        })
        .unwrap_or_else(|| "Document".to_string());
    let mut page = String::with_capacity(body.len() + 1024);
    page.push_str("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
    page.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(page, "<title>{}</title>", escape(&title));
    page.push_str(STYLE);
    page.push_str("</head>\n<body>\n");
    page.push_str(&body);
    page.push_str("</body>\n</html>\n");
    page
}

/// Just enough styling for tables to read as tables and images to fit.
const STYLE: &str = "<style>
body { font-family: system-ui, sans-serif; line-height: 1.5; max-width: 50rem; margin: 2rem auto; padding: 0 1rem; }
table { border-collapse: collapse; margin: 1rem 0; }
th, td { border: 1px solid #999; padding: 0.25rem 0.5rem; vertical-align: top; }
table.layout, table.layout td { border: none; }
img { max-width: 100%; }
pre { overflow-x: auto; }
</style>
";

fn render_blocks(blocks: &[Block], rc: &Ctx, out: &mut String) {
    for block in blocks {
        render_block(block, rc, out);
    }
}

fn render_block(block: &Block, rc: &Ctx, out: &mut String) {
    match block {
        Block::Heading { level, content, .. } => {
            if inlines_are_empty(content) {
                return;
            }
            let level = (*level).clamp(1, 6);
            let _ = write!(out, "<h{level}");
            if let Some(slug) = rc.anchors.heading_slug(block) {
                let _ = write!(out, " id=\"{}\"", escape(slug));
            }
            out.push('>');
            render_inlines(content, rc, out);
            let _ = writeln!(out, "</h{level}>");
        }
        Block::Paragraph(inlines) => {
            if inlines_are_empty(inlines) && !has_anchor(inlines, rc) {
                return;
            }
            out.push_str("<p>");
            render_inlines(inlines, rc, out);
            out.push_str("</p>\n");
        }
        Block::List(list) => render_list(list, rc, out),
        // Trivial layout tables are scaffolding; render their content directly.
        Block::Table(t) if t.kind == TableKind::Layout && t.is_single_cell() => {
            if let CellSlot::Origin(cell) = &t.grid[0][0] {
                render_blocks(&cell.blocks, rc, out);
            }
        }
        Block::Table(t) => render_table(t, rc, out),
        Block::BlockQuote(blocks) => {
            out.push_str("<blockquote>\n");
            render_blocks(blocks, rc, out);
            out.push_str("</blockquote>\n");
        }
        Block::CodeBlock { lang, text } => {
            out.push_str("<pre><code");
            if let Some(lang) = lang.as_deref().filter(|l| !l.trim().is_empty()) {
                let _ = write!(out, " class=\"language-{}\"", escape(lang.trim()));
            }
            out.push('>');
            out.push_str(&escape(text.trim_end_matches('\n')));
            out.push_str("</code></pre>\n");
        }
        Block::Rule => out.push_str("<hr>\n"),
        Block::Math(tex) => {
            if !tex.trim().is_empty() {
                let _ =
                    writeln!(out, "<div class=\"math display\">\\[{}\\]</div>", escape(tex.trim()));
            }
        }
    }
}

/// Whether an anchor here renders: only those something links to do.
fn has_anchor(inlines: &[Inline], rc: &Ctx) -> bool {
    inlines.iter().any(|i| matches!(i, Inline::Anchor(id) if rc.anchors.html_id(id).is_some()))
}

fn render_list(list: &List, rc: &Ctx, out: &mut String) {
    if list.items.is_empty() {
        return;
    }
    if list.ordered() {
        out.push_str("<ol");
        let kind = match list.marker {
            MarkerKind::LowerAlpha => Some("a"),
            MarkerKind::UpperAlpha => Some("A"),
            MarkerKind::LowerRoman => Some("i"),
            MarkerKind::UpperRoman => Some("I"),
            _ => None,
        };
        if let Some(kind) = kind {
            let _ = write!(out, " type=\"{kind}\"");
        }
        if list.start != 1 {
            let _ = write!(out, " start=\"{}\"", list.start);
        }
        out.push_str(">\n");
    } else {
        out.push_str("<ul>\n");
    }
    for item in &list.items {
        match &item.marker_label {
            // A composite label the list type cannot number is shown as is.
            Some(label) => {
                let _ = write!(out, "<li style=\"list-style-type: none\">{} ", escape(label));
            }
            None => out.push_str("<li>"),
        }
        render_flow(&item.blocks, rc, out);
        out.push_str("</li>\n");
    }
    out.push_str(if list.ordered() { "</ol>\n" } else { "</ul>\n" });
}

/// Container content: a lone paragraph inline, anything else as blocks.
fn render_flow(blocks: &[Block], rc: &Ctx, out: &mut String) {
    match blocks {
        [] => {}
        [Block::Paragraph(inlines)] => render_inlines(inlines, rc, out),
        _ => {
            out.push('\n');
            render_blocks(blocks, rc, out);
        }
    }
}

fn render_table(table: &Table, rc: &Ctx, out: &mut String) {
    if table
        .grid
        .iter()
        .all(|row| row.iter().all(|s| matches!(s, CellSlot::Origin(c) if c.is_empty())))
    {
        return;
    }
    out.push_str(if table.kind == TableKind::Layout {
        "<table class=\"layout\">\n"
    } else {
        "<table>\n"
    });
    let header_rows = table.header_rows.min(table.grid.len());
    let (head, body) = table.grid.split_at(header_rows);
    if !head.is_empty() {
        out.push_str("<thead>\n");
        for row in head {
            render_row(row, "th", rc, out);
        }
        out.push_str("</thead>\n");
    }
    if !body.is_empty() {
        out.push_str("<tbody>\n");
        for row in body {
            render_row(row, "td", rc, out);
        }
        out.push_str("</tbody>\n");
    }
    out.push_str("</table>\n");
}

fn render_row(row: &[CellSlot], tag: &str, rc: &Ctx, out: &mut String) {
    out.push_str("<tr>");
    for slot in row {
        let CellSlot::Origin(cell) = slot else { continue };
        render_cell(cell, tag, rc, out);
    }
    out.push_str("</tr>\n");
}

fn render_cell(cell: &Cell, tag: &str, rc: &Ctx, out: &mut String) {
    let _ = write!(out, "<{tag}");
    if cell.col_span > 1 {
        let _ = write!(out, " colspan=\"{}\"", cell.col_span);
    }
    if cell.row_span > 1 {
        let _ = write!(out, " rowspan=\"{}\"", cell.row_span);
    }
    out.push('>');
    render_flow(&cell.blocks, rc, out);
    let _ = write!(out, "</{tag}>");
}

fn render_notes(doc: &Document, rc: &Ctx, out: &mut String) {
    let mut ordered: Vec<(&Note, usize)> =
        doc.notes.iter().filter_map(|n| rc.nums.get(&n.id).map(|&num| (n, num))).collect();
    ordered.sort_by_key(|(_, num)| *num);
    let mut rendered: HashSet<usize> = HashSet::new();
    let mut notes = String::new();
    for (note, num) in ordered {
        // The first definition for a duplicated id wins.
        if !rendered.insert(num) {
            continue;
        }
        let _ = write!(notes, "<li id=\"fn-{num}\">");
        render_flow(&note.blocks, rc, &mut notes);
        if rc.referenced.borrow().contains(&num) {
            let _ = write!(notes, " <a href=\"#fnref-{num}\" class=\"footnote-back\">\u{21a9}</a>");
        }
        notes.push_str("</li>\n");
    }
    if !notes.is_empty() {
        out.push_str("<section class=\"footnotes\">\n<hr>\n<ol>\n");
        out.push_str(&notes);
        out.push_str("</ol>\n</section>\n");
    }
}

fn render_inlines(inlines: &[Inline], rc: &Ctx, out: &mut String) {
    // Adjacent runs of one style share one set of tags.
    let mut pending: Option<(String, Style)> = None;
    let flush = |pending: &mut Option<(String, Style)>, out: &mut String| {
        if let Some((text, style)) = pending.take() {
            push_styled(&text, style, out);
        }
    };
    for inline in inlines {
        match inline {
            Inline::Text { text, style } => {
                if text.is_empty() {
                    continue;
                }
                match &mut pending {
                    Some((prev, prev_style)) if prev_style == style => prev.push_str(text),
                    _ => {
                        flush(&mut pending, out);
                        pending = Some((text.clone(), *style));
                    }
                }
                continue;
            }
            _ => flush(&mut pending, out),
        }
        match inline {
            Inline::Text { .. } => unreachable!(),
            Inline::Link { content, target } => render_link(content, target, rc, out),
            Inline::Image { alt, source } => render_image(alt, source, rc, out),
            Inline::Anchor(id) => {
                if let Some(html_id) = rc.anchors.html_id(id) {
                    let _ = write!(out, "<a id=\"{}\"></a>", escape(html_id));
                }
            }
            Inline::NoteRef(id) => {
                if let Some(&num) = rc.nums.get(id) {
                    let first = rc.referenced.borrow_mut().insert(num);
                    let id_attr =
                        if first { format!(" id=\"fnref-{num}\"") } else { String::new() };
                    let _ = write!(
                        out,
                        "<sup class=\"footnote-ref\"><a href=\"#fn-{num}\"{id_attr}>{num}</a></sup>"
                    );
                }
            }
            Inline::LineBreak => out.push_str("<br>\n"),
            Inline::Math(tex) => {
                if !tex.trim().is_empty() {
                    let _ = write!(
                        out,
                        "<span class=\"math inline\">\\({}\\)</span>",
                        escape(tex.trim())
                    );
                }
            }
            Inline::Checkbox(checked) => {
                out.push_str(if *checked {
                    "<input type=\"checkbox\" disabled checked>"
                } else {
                    "<input type=\"checkbox\" disabled>"
                });
            }
        }
    }
    flush(&mut pending, out);
}

fn push_styled(text: &str, style: Style, out: &mut String) {
    let mut tags: Vec<&str> = Vec::new();
    if style.strike {
        tags.push("s");
    }
    if style.bold {
        tags.push("strong");
    }
    if style.italic {
        tags.push("em");
    }
    if style.code {
        tags.push("code");
    }
    // Styling whitespace alone shows nothing.
    if text.trim().is_empty() {
        tags.clear();
    }
    for tag in &tags {
        let _ = write!(out, "<{tag}>");
    }
    out.push_str(&escape(text));
    for tag in tags.iter().rev() {
        let _ = write!(out, "</{tag}>");
    }
}

fn render_link(content: &[Inline], target: &LinkTarget, rc: &Ctx, out: &mut String) {
    let href = match target {
        LinkTarget::External(url) | LinkTarget::Relative(url) => safe_url(url),
        LinkTarget::Anchor(id) => rc.anchors.fragment(id).map(|f| format!("#{f}")),
    };
    let Some(href) = href.filter(|h| !h.is_empty()) else {
        // No usable destination: the content stands on its own.
        render_inlines(content, rc, out);
        return;
    };
    let _ = write!(out, "<a href=\"{}\">", escape(&href));
    if inlines_are_empty(content) {
        out.push_str(&escape(&href));
    } else {
        render_inlines(content, rc, out);
    }
    out.push_str("</a>");
}

fn render_image(alt: &str, source: &ImageSource, rc: &Ctx, out: &mut String) {
    let src = match source {
        ImageSource::External(url) => safe_url(url),
        ImageSource::Asset(id) => {
            rc.assets.get(id.0).and_then(|asset| asset_src(asset, rc.options))
        }
        ImageSource::Unavailable => None,
    };
    match src {
        Some(src) => {
            let _ = write!(out, "<img src=\"{}\" alt=\"{}\">", escape(&src), escape(alt.trim()));
        }
        // Nothing a browser can show: the alt text is what remains.
        None => out.push_str(&escape(alt.trim())),
    }
}

/// Where an embedded asset's image loads from, when a browser can show it.
fn asset_src(asset: &Asset, options: &HtmlOptions) -> Option<String> {
    if !is_web_image(&asset.media_type) {
        return None;
    }
    Some(match &options.asset_prefix {
        Some(prefix) => format!("{prefix}{}.{}", asset.id.0, asset.extension()),
        None => format!("data:{};base64,{}", asset.media_type, base64(&asset.bytes)),
    })
}

fn is_web_image(media_type: &str) -> bool {
    matches!(
        media_type,
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/bmp"
            | "image/svg+xml"
            | "image/avif"
    )
}

/// The URL as an attribute value, or `None` for a scheme that could run
/// script (`javascript:`, `vbscript:`, `data:`, ...).
fn safe_url(url: &str) -> Option<String> {
    let url = url.trim();
    let scheme_end = url.find(':');
    let path_start = url.find(['/', '?', '#']).unwrap_or(url.len());
    match scheme_end {
        Some(end) if end < path_start => {
            let scheme: String = url[..end]
                .chars()
                .filter(|c| !c.is_whitespace() && !c.is_control())
                .collect::<String>()
                .to_ascii_lowercase();
            matches!(scheme.as_str(), "http" | "https" | "mailto" | "ftp" | "tel" | "news")
                .then(|| url.to_string())
        }
        _ => Some(url.to_string()),
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() && !matches!(c, '\n' | '\t') => {}
            c => out.push(c),
        }
    }
    out
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssetId, ListItem, NoteKind, TableKind};

    fn fragment(doc: &Document) -> String {
        document_to_html(doc, &HtmlOptions { fragment: true, ..Default::default() })
    }

    fn doc(blocks: Vec<Block>) -> Document {
        Document { blocks, ..Default::default() }
    }

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn text_is_escaped_and_styled() {
        let html = fragment(&doc(vec![Block::Paragraph(vec![
            Inline::plain("a < b & "),
            Inline::Text { text: "bold".into(), style: Style { bold: true, ..Style::PLAIN } },
            Inline::LineBreak,
            Inline::plain("<script>"),
        ])]));
        assert_eq!(html, "<p>a &lt; b &amp; <strong>bold</strong><br>\n&lt;script&gt;</p>\n");
    }

    #[test]
    fn script_urls_are_not_links() {
        let html = fragment(&doc(vec![Block::Paragraph(vec![
            Inline::Link {
                content: vec![Inline::plain("click")],
                target: LinkTarget::External("javascript:alert(1)".into()),
            },
            Inline::Link {
                content: vec![Inline::plain("ok")],
                target: LinkTarget::External("https://example.com/?a=1&b=2".into()),
            },
        ])]));
        assert_eq!(html, "<p>click<a href=\"https://example.com/?a=1&amp;b=2\">ok</a></p>\n");
    }

    #[test]
    fn tables_keep_spans_and_headers() {
        let mut builder = crate::model::GridBuilder::new();
        builder.next_row();
        builder
            .place(Cell::spanning(vec![Block::Paragraph(vec![Inline::plain("wide")])], 2, 1))
            .unwrap();
        builder.next_row();
        builder.place(Cell::from_inlines(vec![Inline::plain("a")])).unwrap();
        builder.place(Cell::from_inlines(vec![Inline::plain("b")])).unwrap();
        let mut table = builder.finish(TableKind::Data);
        table.header_rows = 1;
        let html = fragment(&doc(vec![Block::Table(table)]));
        assert_eq!(
            html,
            "<table>\n<thead>\n<tr><th colspan=\"2\">wide</th></tr>\n</thead>\n<tbody>\n<tr><td>a</td><td>b</td></tr>\n</tbody>\n</table>\n"
        );
    }

    #[test]
    fn embedded_images_become_data_uris_or_files() {
        let mut d = doc(vec![Block::Paragraph(vec![Inline::Image {
            alt: "dot".into(),
            source: ImageSource::Asset(AssetId(0)),
        }])]);
        d.assets.push(Asset {
            id: AssetId(0),
            media_type: "image/png".into(),
            origin_part: "x".into(),
            bytes: b"foo".to_vec(),
        });
        assert_eq!(fragment(&d), "<p><img src=\"data:image/png;base64,Zm9v\" alt=\"dot\"></p>\n");
        let options = HtmlOptions { fragment: true, asset_prefix: Some("media/".into()) };
        assert_eq!(
            document_to_html(&d, &options),
            "<p><img src=\"media/0.png\" alt=\"dot\"></p>\n"
        );
        // A format no browser shows keeps its alt text.
        d.assets[0].media_type = "image/emf".into();
        assert_eq!(fragment(&d), "<p>dot</p>\n");
    }

    #[test]
    fn headings_get_ids_links_resolve_and_notes_list() {
        let d = Document {
            blocks: vec![
                Block::Heading {
                    level: 2,
                    anchor: Some("h".into()),
                    content: vec![Inline::plain("Intro Part")],
                },
                Block::Paragraph(vec![
                    Inline::Link {
                        content: vec![Inline::plain("back")],
                        target: LinkTarget::Anchor("h".into()),
                    },
                    Inline::NoteRef("n1".into()),
                ]),
                Block::List(List {
                    marker: MarkerKind::LowerRoman,
                    start: 3,
                    items: vec![ListItem {
                        blocks: vec![Block::Paragraph(vec![Inline::plain("x")])],
                        marker_label: None,
                    }],
                }),
            ],
            notes: vec![Note {
                id: "n1".into(),
                kind: NoteKind::Footnote,
                blocks: vec![Block::Paragraph(vec![Inline::plain("note")])],
            }],
            assets: vec![],
        };
        let html = fragment(&d);
        assert!(html.contains("<h2 id=\"intro-part\">Intro Part</h2>"), "{html}");
        assert!(html.contains("<a href=\"#intro-part\">back</a>"), "{html}");
        assert!(html.contains("<a href=\"#fn-1\" id=\"fnref-1\">1</a>"), "{html}");
        assert!(html.contains("<ol type=\"i\" start=\"3\">\n<li>x</li>"), "{html}");
        assert!(html.contains("<li id=\"fn-1\">note <a href=\"#fnref-1\""), "{html}");
    }

    #[test]
    fn standalone_pages_are_titled() {
        let html = document_to_html(
            &doc(vec![Block::heading(1, vec![Inline::plain("Report & <Co>")])]),
            &HtmlOptions::default(),
        );
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<title>Report &amp; &lt;Co&gt;</title>"));
        assert!(html.ends_with("</html>\n"));
    }
}
