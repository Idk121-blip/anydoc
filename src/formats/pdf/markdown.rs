//! pdf-inspector's Markdown back into the document model.
//!
//! pdf-inspector writes a small, regular Markdown subset: ATX headings,
//! paragraphs whose remaining newlines are real line breaks, `-` and `1.`
//! lists, `>` quotes, fenced code, compact pipe tables, emphasis, `<s>` and
//! `<u>` runs, links, and `<!-- Page N -->` markers between pages. This
//! parser reads exactly that subset; anything else stays literal text. Every
//! lookup is precomputed, so no input shape makes it quadratic.

use crate::model::{Block, ImageSource, Inline, LinkTarget, List, ListItem, MarkerKind, Style};
use crate::model::{Cell, Table, TableKind};

/// Nesting (quotes, lists, emphasis, links) beyond this stays literal.
const MAX_DEPTH: usize = 32;

/// A top-level block and the 1-indexed page it starts on.
#[derive(Debug, Clone)]
pub(super) struct Located {
    pub block: Block,
    pub page: u32,
}

/// Resolves an image placeholder (page, XObject name) to its source.
pub(super) type ImageResolver<'r> = dyn FnMut(u32, &str) -> ImageSource + 'r;

#[derive(Clone, Copy)]
struct Line<'a> {
    text: &'a str,
    page: u32,
}

pub(super) fn parse(markdown: &str, images: &mut ImageResolver<'_>) -> Vec<Located> {
    let mut page = 1;
    let mut lines = Vec::new();
    for raw in markdown.lines() {
        if let Some(n) = page_marker(raw) {
            page = n;
            // A page boundary always ends the block before it.
            lines.push(Line { text: "", page });
            continue;
        }
        lines.push(Line { text: raw, page });
    }
    let mut parser = Parser { images };
    parser.blocks(&lines, 0)
}

fn page_marker(line: &str) -> Option<u32> {
    line.trim().strip_prefix("<!-- Page ")?.strip_suffix(" -->")?.trim().parse().ok()
}

struct Parser<'p, 'r> {
    images: &'p mut ImageResolver<'r>,
}

impl Parser<'_, '_> {
    fn blocks(&mut self, lines: &[Line<'_>], depth: usize) -> Vec<Located> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let trimmed = line.text.trim();
            if trimmed.is_empty() || is_comment(trimmed) {
                i += 1;
                continue;
            }
            let (block, next) = self.block(lines, i, depth);
            if let Some(block) = block {
                out.push(Located { block, page: line.page });
            }
            i = next.max(i + 1);
        }
        out
    }

    /// Parse the block starting at `lines[i]`; returns it (if any) and the
    /// index of the first line after it.
    fn block(&mut self, lines: &[Line<'_>], i: usize, depth: usize) -> (Option<Block>, usize) {
        let line = lines[i];
        let text = line.text;
        if let Some(fence) = fence_open(text) {
            let mut body = Vec::new();
            let mut j = i + 1;
            while j < lines.len() && !is_fence_close(lines[j].text, fence) {
                body.push(lines[j].text);
                j += 1;
            }
            let body = body.join("\n");
            let block =
                (!body.trim().is_empty()).then_some(Block::CodeBlock { lang: None, text: body });
            return (block, j + 1);
        }
        if let Some((level, content)) = heading(text) {
            let content = self.inlines(content, line.page, depth);
            return (Some(Block::heading(level, content)), i + 1);
        }
        if is_rule(text) {
            return (Some(Block::Rule), i + 1);
        }
        if starts_table(lines, i) {
            let mut j = i;
            let mut rows = Vec::new();
            while j < lines.len() && lines[j].text.trim_start().starts_with('|') {
                if j != i + 1 {
                    rows.push((lines[j].text, lines[j].page));
                }
                j += 1;
            }
            return (self.table(&rows, depth), j);
        }
        if depth < MAX_DEPTH && quote_body(text).is_some() {
            let mut inner = Vec::new();
            let mut j = i;
            while j < lines.len() {
                match quote_body(lines[j].text) {
                    Some(body) => inner.push(Line { text: body, page: lines[j].page }),
                    None => break,
                }
                j += 1;
            }
            let blocks: Vec<Block> =
                self.blocks(&inner, depth + 1).into_iter().map(|l| l.block).collect();
            return ((!blocks.is_empty()).then_some(Block::BlockQuote(blocks)), j);
        }
        if depth < MAX_DEPTH
            && let Some(marker) = list_marker(text)
        {
            return self.list(lines, i, marker, depth);
        }
        // Paragraph: runs to a blank line or the next block's start.
        let mut j = i + 1;
        while j < lines.len() && !interrupts_paragraph(lines, j) {
            j += 1;
        }
        let joined: Vec<&str> = lines[i..j].iter().map(|l| l.text.trim()).collect();
        let content = self.inlines(&joined.join("\n"), line.page, depth);
        (Some(Block::Paragraph(content)), j)
    }

    fn list(
        &mut self,
        lines: &[Line<'_>],
        start: usize,
        first: Marker,
        depth: usize,
    ) -> (Option<Block>, usize) {
        let base = first.indent;
        let mut items: Vec<ListItem> = Vec::new();
        let mut i = start;
        while i < lines.len() {
            let Some(marker) = list_marker(lines[i].text) else { break };
            if marker.indent < base || marker.ordered != first.ordered {
                break;
            }
            // The item's own lines: its first line's content, then every
            // line indented under it (nested lists, continuations) and lazy
            // continuation lines of its paragraph.
            let mut item_lines =
                vec![Line { text: &lines[i].text[marker.content..], page: lines[i].page }];
            let content_indent = marker.content;
            let mut j = i + 1;
            while j < lines.len() {
                let text = lines[j].text;
                if text.trim().is_empty() {
                    // A blank line continues the item only when indented
                    // content follows it.
                    let next = (j + 1..lines.len()).find(|&k| !lines[k].text.trim().is_empty());
                    match next {
                        Some(k) if indent_of(lines[k].text) >= content_indent => {
                            item_lines.push(Line { text: "", page: lines[j].page });
                            j += 1;
                            continue;
                        }
                        _ => break,
                    }
                }
                let indent = indent_of(text);
                if indent >= content_indent {
                    item_lines.push(Line { text: &text[content_indent..], page: lines[j].page });
                } else if list_marker(text).is_some() {
                    break;
                } else if !item_lines.last().is_some_and(|l| l.text.trim().is_empty())
                    && !interrupts_paragraph(lines, j)
                {
                    item_lines.push(Line { text: text.trim_start(), page: lines[j].page });
                } else {
                    break;
                }
                j += 1;
            }
            let blocks = self.blocks(&item_lines, depth + 1).into_iter().map(|l| l.block).collect();
            items.push(ListItem { blocks, marker_label: None });
            i = j;
            // Blank lines between sibling items keep the list going.
            while i < lines.len() && lines[i].text.trim().is_empty() {
                let next = (i..lines.len()).find(|&k| !lines[k].text.trim().is_empty());
                match next.and_then(|k| list_marker(lines[k].text)) {
                    Some(m) if m.indent == base && m.ordered == first.ordered => i = next.unwrap(),
                    _ => break,
                }
            }
        }
        let marker = if first.ordered { MarkerKind::Decimal } else { MarkerKind::Bullet };
        let list = List { marker, start: first.number, items };
        (Some(Block::List(list)), i)
    }

    fn table(&mut self, rows: &[(&str, u32)], depth: usize) -> Option<Block> {
        let mut grid: Vec<Vec<Cell>> = rows
            .iter()
            .map(|(row, page)| {
                split_row(row)
                    .into_iter()
                    .map(|cell| {
                        let content = self.inlines(cell.trim(), *page, depth);
                        Cell::from_inlines(content)
                    })
                    .collect()
            })
            .collect();
        if grid.is_empty() {
            return None;
        }
        // pdf-inspector always writes the delimiter row GFM requires; an
        // empty first row above it is that requirement, not a header.
        let header_rows = if grid[0].iter().all(Cell::is_empty) {
            grid.remove(0);
            0
        } else {
            1
        };
        if grid.is_empty() {
            return None;
        }
        Some(Block::Table(Table::from_rows(grid, header_rows, TableKind::Data)))
    }

    fn inlines(&mut self, text: &str, page: u32, depth: usize) -> Vec<Inline> {
        let mut inline = InlineParser::new(text, page, &mut *self.images);
        inline.depth = depth;
        inline.parse()
    }
}

fn is_comment(line: &str) -> bool {
    line.starts_with("<!--") && line.ends_with("-->")
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn fence_open(line: &str) -> Option<usize> {
    let t = line.trim_start();
    let n = t.len() - t.trim_start_matches('`').len();
    (n >= 3 && !t[n..].contains('`')).then_some(n)
}

fn is_fence_close(line: &str, fence: usize) -> bool {
    let t = line.trim();
    t.len() >= fence && t.bytes().all(|b| b == b'`')
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let t = line.trim_start();
    let level = t.len() - t.trim_start_matches('#').len();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &t[level..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    let content = rest.trim();
    // An optional closing sequence of hashes, set off by a space.
    let stripped = content.trim_end_matches('#');
    let content =
        if stripped.is_empty() || stripped.ends_with(' ') { stripped.trim_end() } else { content };
    Some((level as u8, content))
}

fn is_rule(line: &str) -> bool {
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3
        && (t.bytes().all(|b| b == b'-')
            || t.bytes().all(|b| b == b'*')
            || t.bytes().all(|b| b == b'_'))
}

fn starts_table(lines: &[Line<'_>], i: usize) -> bool {
    lines[i].text.trim_start().starts_with('|')
        && lines.get(i + 1).is_some_and(|l| is_delimiter_row(l.text))
}

fn is_delimiter_row(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('|') {
        return false;
    }
    let cells = split_row(t);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim().trim_start_matches(':').trim_end_matches(':');
            !c.is_empty() && c.bytes().all(|b| b == b'-')
        })
}

/// A pipe-table row's cells, split on unescaped pipes outside code spans.
fn split_row(row: &str) -> Vec<&str> {
    let t = row.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let bytes = t.as_bytes();
    let mut cells = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut in_code = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'`' => in_code = !in_code,
            b'|' if !in_code => {
                cells.push(&t[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let last = &t[start.min(t.len())..];
    if !last.trim().is_empty() {
        cells.push(last);
    }
    cells
}

fn quote_body(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let rest = t.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

#[derive(Debug, Clone, Copy)]
struct Marker {
    indent: usize,
    /// Byte offset of the item's content within the line.
    content: usize,
    ordered: bool,
    number: u64,
}

fn list_marker(line: &str) -> Option<Marker> {
    let indent = indent_of(line);
    let rest = &line[indent..];
    let bytes = rest.as_bytes();
    let (len, ordered, number) = match bytes.first()? {
        b'-' | b'*' | b'+' => (1, false, 1),
        b'0'..=b'9' => {
            let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 9 || !matches!(bytes.get(digits), Some(b'.' | b')')) {
                return None;
            }
            (digits + 1, true, rest[..digits].parse().ok()?)
        }
        _ => return None,
    };
    let after = &rest[len..];
    let spaces = after.len() - after.trim_start_matches(' ').len();
    if spaces == 0 || after.trim().is_empty() {
        return None;
    }
    Some(Marker { indent, content: indent + len + spaces.min(4), ordered, number })
}

fn interrupts_paragraph(lines: &[Line<'_>], j: usize) -> bool {
    let text = lines[j].text;
    let t = text.trim();
    t.is_empty()
        || is_comment(t)
        || fence_open(text).is_some()
        || heading(text).is_some()
        || starts_table(lines, j)
        || quote_body(text).is_some()
        || (is_rule(text) && !t.starts_with('-'))
        || list_marker(text).is_some_and(|m| !m.ordered || m.number == 1)
}

// ---------------------------------------------------------------------------
// Inlines

/// Emphasis delimiter characters the parser pairs.
const EMPHASIS: [u8; 3] = [b'*', b'_', b'~'];

/// A maximal run of one delimiter character.
#[derive(Debug, Clone, Copy)]
struct Run {
    start: usize,
    len: usize,
}

struct InlineParser<'s, 'p, 'r> {
    s: &'s str,
    b: &'s [u8],
    page: u32,
    images: &'p mut ImageResolver<'r>,
    depth: usize,
    /// Per emphasis character, per wanted length (1..=3): starts of runs that
    /// can close emphasis of that length, ascending.
    closers: [[Vec<usize>; 3]; 3],
    /// Delimiter run starting at a byte offset.
    run_at: std::collections::HashMap<usize, Run>,
    /// Backtick run start -> start of the next run of the same length.
    code_close: std::collections::HashMap<usize, usize>,
    /// `[` offset -> matching `]` offset; `(` offset -> matching `)`.
    bracket: std::collections::HashMap<usize, usize>,
    paren: std::collections::HashMap<usize, usize>,
    /// Closing-tag name -> ascending offsets.
    closing_tags: std::collections::HashMap<&'static str, Vec<usize>>,
}

/// Formatting tags pdf-inspector (and PDFs' own text) produce, with the
/// style each adds; `None` keeps the content unstyled (underline, sup/sub).
type Tag = (&'static str, Option<fn(&mut Style)>);

const TAGS: [Tag; 11] = [
    ("s", Some(|s| s.strike = true)),
    ("del", Some(|s| s.strike = true)),
    ("strike", Some(|s| s.strike = true)),
    ("b", Some(|s| s.bold = true)),
    ("strong", Some(|s| s.bold = true)),
    ("i", Some(|s| s.italic = true)),
    ("em", Some(|s| s.italic = true)),
    ("code", Some(|s| s.code = true)),
    ("u", None),
    ("sup", None),
    ("sub", None),
];

impl<'s, 'p, 'r> InlineParser<'s, 'p, 'r> {
    fn new(s: &'s str, page: u32, images: &'p mut ImageResolver<'r>) -> Self {
        let b = s.as_bytes();
        let mut parser = InlineParser {
            s,
            b,
            page,
            images,
            depth: 0,
            closers: Default::default(),
            run_at: Default::default(),
            code_close: Default::default(),
            bracket: Default::default(),
            paren: Default::default(),
            closing_tags: Default::default(),
        };
        parser.index();
        parser
    }

    fn index(&mut self) {
        let b = self.b;
        let mut i = 0;
        let mut bracket_stack = Vec::new();
        let mut paren_stack = Vec::new();
        let mut backtick_runs: Vec<Run> = Vec::new();
        while i < b.len() {
            let c = b[i];
            if c == b'\\' {
                i += 2;
                continue;
            }
            if let Some(kind) = EMPHASIS.iter().position(|&e| e == c) {
                let len = b[i..].iter().take_while(|&&x| x == c).count();
                let run = Run { start: i, len };
                self.run_at.insert(i, run);
                let prev = self.s[..i].chars().next_back();
                let next = self.s[i + len..].chars().next();
                if prev.is_some_and(|p| !p.is_whitespace())
                    && !(c == b'_' && next.is_some_and(char::is_alphanumeric))
                {
                    for want in 1..=len.min(3) {
                        self.closers[kind][want - 1].push(i);
                    }
                }
                i += len;
                continue;
            }
            match c {
                b'`' => {
                    let len = b[i..].iter().take_while(|&&x| x == b'`').count();
                    backtick_runs.push(Run { start: i, len });
                    i += len;
                    continue;
                }
                b'[' => bracket_stack.push(i),
                b']' => {
                    if let Some(open) = bracket_stack.pop() {
                        self.bracket.insert(open, i);
                    }
                }
                b'(' => paren_stack.push(i),
                b')' => {
                    if let Some(open) = paren_stack.pop() {
                        self.paren.insert(open, i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        let mut next_by_len: std::collections::HashMap<usize, usize> = Default::default();
        for run in backtick_runs.iter().rev() {
            if let Some(&close) = next_by_len.get(&run.len) {
                self.code_close.insert(run.start, close);
            }
            next_by_len.insert(run.len, run.start);
        }
        let lower = self.s.to_ascii_lowercase();
        for (name, _) in TAGS {
            let closing = format!("</{name}>");
            let found: Vec<usize> = lower.match_indices(&closing).map(|(at, _)| at).collect();
            if !found.is_empty() {
                self.closing_tags.insert(name, found);
            }
        }
    }

    fn parse(&mut self) -> Vec<Inline> {
        let mut out = Out::default();
        self.range(0, self.b.len(), Style::PLAIN, &mut out);
        out.finish()
    }

    /// First entry of the ascending `list` inside `(after, before)`.
    fn first_between(list: &[usize], after: usize, before: usize) -> Option<usize> {
        let idx = list.partition_point(|&x| x <= after);
        list.get(idx).copied().filter(|&x| x < before)
    }

    fn range(&mut self, start: usize, end: usize, style: Style, out: &mut Out) {
        self.depth += 1;
        let mut i = start;
        while i < end {
            let c = self.b[i];
            if self.depth > MAX_DEPTH {
                out.text(&self.s[i..end], style);
                break;
            }
            match c {
                b'\\' if i + 1 < end && self.b[i + 1].is_ascii_punctuation() => {
                    out.text(&self.s[i + 1..i + 2], style);
                    i += 2;
                    continue;
                }
                b'\n' => {
                    out.push(Inline::LineBreak);
                    i += 1;
                    continue;
                }
                b'`' => {
                    let len = self.b[i..end].iter().take_while(|&&x| x == b'`').count();
                    if let Some(&close) = self.code_close.get(&i)
                        && close + len <= end
                    {
                        let code = &self.s[i + len..close];
                        let code = if code.len() > 2 && code.starts_with(' ') && code.ends_with(' ')
                        {
                            &code[1..code.len() - 1]
                        } else {
                            code
                        };
                        out.text(&code.replace('\n', " "), Style { code: true, ..style });
                        i = close + len;
                    } else {
                        out.text(&self.s[i..i + len], style);
                        i += len;
                    }
                    continue;
                }
                b'*' | b'_' | b'~' => {
                    i = self.emphasis(i, end, style, out);
                    continue;
                }
                b'!' if self.b.get(i + 1) == Some(&b'[') => {
                    if let Some(next) = self.image(i, end, out) {
                        i = next;
                        continue;
                    }
                }
                b'[' => {
                    if let Some(next) = self.link(i, end, style, out) {
                        i = next;
                        continue;
                    }
                }
                b'<' => {
                    if let Some(next) = self.tag(i, end, style, out) {
                        i = next;
                        continue;
                    }
                }
                b'&' => {
                    if let Some((decoded, len)) = entity(&self.s[i..end]) {
                        out.text(decoded.encode_utf8(&mut [0; 4]), style);
                        i += len;
                        continue;
                    }
                }
                _ => {}
            }
            // Plain text up to the next byte that could start syntax.
            let next = self.b[i + 1..end]
                .iter()
                .position(|b| b"\\\n`*_~![<&".contains(b))
                .map_or(end, |p| i + 1 + p);
            out.text(&self.s[i..next], style);
            i = next;
        }
        self.depth -= 1;
    }

    fn emphasis(&mut self, i: usize, end: usize, style: Style, out: &mut Out) -> usize {
        let c = self.b[i];
        let kind = EMPHASIS.iter().position(|&e| e == c).unwrap();
        let run = self.run_at.get(&i).copied().unwrap_or(Run { start: i, len: 1 });
        let len = run.len.min(end - i);
        let next = self.s[i + len..end].chars().next();
        let prev = self.s[..i].chars().next_back();
        let opens = next.is_some_and(|n| !n.is_whitespace())
            && !(c == b'_' && prev.is_some_and(char::is_alphanumeric));
        // `~` pairs only as the `~~` strikethrough.
        let wants: &[usize] = if c == b'~' { &[2] } else { &[3, 2, 1] };
        if opens {
            for &want in wants {
                if want > len {
                    continue;
                }
                // Extra delimiters beyond the wanted ones stay literal.
                let open_end = i + len;
                let content_start = open_end;
                let Some(close) =
                    Self::first_between(&self.closers[kind][want - 1], content_start, end)
                else {
                    continue;
                };
                let mut inner = style;
                match (c, want) {
                    (b'~', _) => inner.strike = true,
                    (_, 1) => inner.italic = true,
                    (_, 2) => inner.bold = true,
                    _ => {
                        inner.bold = true;
                        inner.italic = true;
                    }
                }
                if len > want {
                    out.text(&self.s[i..i + len - want], style);
                }
                self.range(content_start, close, inner, out);
                let close_run = self.run_at.get(&close).map_or(want, |r| r.len);
                // A longer closing run leaves its surplus as literal text.
                if close_run > want {
                    out.text(&self.s[close + want..close + close_run], style);
                }
                return close + close_run;
            }
        }
        out.text(&self.s[i..i + len], style);
        i + len
    }

    /// `[label](target)`: returns the offset after it, or `None` when the
    /// bracket is literal.
    fn link(&mut self, i: usize, end: usize, style: Style, out: &mut Out) -> Option<usize> {
        let (label_end, target, after) = self.bracketed(i, end)?;
        let target = link_target(target)?;
        let mut content = Out::default();
        self.range(i + 1, label_end, style, &mut content);
        out.push(Inline::Link { content: content.finish(), target });
        Some(after)
    }

    fn image(&mut self, i: usize, end: usize, out: &mut Out) -> Option<usize> {
        let (alt_end, target, after) = self.bracketed(i + 1, end)?;
        let alt = &self.s[i + 2..alt_end];
        let source = match target.trim() {
            // pdf-inspector's placeholder: the XObject name rides in the alt.
            "image" => {
                let name = alt.strip_prefix("Image: ").unwrap_or(alt);
                out.push(Inline::Image {
                    alt: String::new(),
                    source: (self.images)(self.page, name),
                });
                return Some(after);
            }
            url if url.contains("://") => ImageSource::External(url.to_string()),
            _ => ImageSource::Unavailable,
        };
        out.push(Inline::Image { alt: alt.to_string(), source });
        Some(after)
    }

    /// The `](...)` tail of a bracket at `i`: label end, raw target, and the
    /// offset after the closing parenthesis.
    fn bracketed(&self, i: usize, end: usize) -> Option<(usize, &'s str, usize)> {
        let close = *self.bracket.get(&i)?;
        if close + 1 >= end || self.b[close + 1] != b'(' {
            return None;
        }
        let paren_close = *self.paren.get(&(close + 1))?;
        if paren_close >= end {
            return None;
        }
        Some((close, &self.s[close + 2..paren_close], paren_close + 1))
    }

    fn tag(&mut self, i: usize, end: usize, style: Style, out: &mut Out) -> Option<usize> {
        let rest = &self.s[i..end];
        // Tags and autolinks are short; a bounded look keeps a run of stray
        // `<` linear.
        let gt = rest.as_bytes().iter().take(512).position(|&b| b == b'>')?;
        let inner = &rest[1..gt];
        let lower = inner.trim().to_ascii_lowercase();
        if matches!(lower.as_str(), "br" | "br/" | "br /") {
            out.push(Inline::LineBreak);
            return Some(i + gt + 1);
        }
        if inner.starts_with("http://")
            || inner.starts_with("https://")
            || inner.starts_with("mailto:")
        {
            if inner.contains(char::is_whitespace) {
                return None;
            }
            out.push(Inline::Link {
                content: vec![Inline::Text { text: inner.to_string(), style }],
                target: LinkTarget::External(inner.to_string()),
            });
            return Some(i + gt + 1);
        }
        let (name, apply) = TAGS.iter().find(|(name, _)| *name == lower)?;
        let close = Self::first_between(self.closing_tags.get(name)?, i + gt, end)?;
        let mut inner_style = style;
        if let Some(apply) = apply {
            apply(&mut inner_style);
        }
        self.range(i + gt + 1, close, inner_style, out);
        Some(close + name.len() + 3)
    }
}

fn link_target(raw: &str) -> Option<LinkTarget> {
    let raw = raw.trim();
    // An optional title after the destination is dropped.
    let url = raw
        .strip_prefix('<')
        .and_then(|r| r.split_once('>'))
        .map_or_else(|| raw.split_whitespace().next().unwrap_or(""), |(url, _)| url);
    if url.is_empty() {
        return None;
    }
    Some(if let Some(anchor) = url.strip_prefix('#') {
        LinkTarget::Anchor(anchor.to_string())
    } else if url.contains(':') && !url.starts_with('/') {
        LinkTarget::External(url.to_string())
    } else {
        LinkTarget::Relative(url.to_string())
    })
}

/// A character reference at the start of `s`, and its length.
fn entity(s: &str) -> Option<(char, usize)> {
    let semi = s.as_bytes().iter().take(12).position(|&b| b == b';')?;
    let name = &s[1..semi];
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code).filter(|c| *c != '\0')?
        }
    };
    Some((c, semi + 1))
}

/// Accumulates inlines, merging adjacent same-style text.
#[derive(Default)]
struct Out {
    inlines: Vec<Inline>,
}

impl Out {
    fn text(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(Inline::Text { text: prev, style: prev_style }) = self.inlines.last_mut()
            && *prev_style == style
        {
            prev.push_str(text);
            return;
        }
        self.inlines.push(Inline::Text { text: text.to_string(), style });
    }

    fn push(&mut self, inline: Inline) {
        self.inlines.push(inline);
    }

    fn finish(self) -> Vec<Inline> {
        self.inlines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CellSlot, inlines_to_plain_text};

    fn parse_str(md: &str) -> Vec<Located> {
        parse(md, &mut |_, _| ImageSource::Unavailable)
    }

    fn inl(md: &str) -> Vec<Inline> {
        InlineParser::new(md, 1, &mut |_, _| ImageSource::Unavailable).parse()
    }

    fn styled(inlines: &[Inline]) -> Vec<(String, Style)> {
        inlines
            .iter()
            .filter_map(|i| match i {
                Inline::Text { text, style } => Some((text.clone(), *style)),
                _ => None,
            })
            .collect()
    }

    const BOLD: Style = Style { bold: true, italic: false, strike: false, code: false };
    const ITALIC: Style = Style { bold: false, italic: true, strike: false, code: false };
    const STRIKE: Style = Style { bold: false, italic: false, strike: true, code: false };

    #[test]
    fn emphasis_pairs_and_unpaired_delimiters_stay_literal() {
        assert_eq!(
            styled(&inl("a **bold** and *it* and <s>gone</s>")),
            vec![
                ("a ".into(), Style::PLAIN),
                ("bold".into(), BOLD),
                (" and ".into(), Style::PLAIN),
                ("it".into(), ITALIC),
                (" and ".into(), Style::PLAIN),
                ("gone".into(), STRIKE),
            ]
        );
        assert_eq!(
            styled(&inl("2 * 3 * 4 snake_case_name")),
            vec![("2 * 3 * 4 snake_case_name".into(), Style::PLAIN)]
        );
        assert_eq!(styled(&inl("**unclosed")), vec![("**unclosed".into(), Style::PLAIN)]);
    }

    #[test]
    fn underline_tags_keep_their_text() {
        assert_eq!(inlines_to_plain_text(&inl("to <u>a file</u>.")), "to a file.");
    }

    #[test]
    fn links_escapes_and_entities() {
        let parsed = inl(r"see [the site](https://example.com) \*not\* &amp; <https://x.y>");
        assert!(
            matches!(&parsed[1], Inline::Link { target: LinkTarget::External(u), .. } if u == "https://example.com")
        );
        assert_eq!(inlines_to_plain_text(&parsed), "see the site *not* & https://x.y");
    }

    #[test]
    fn many_unpaired_delimiters_parse_in_linear_time() {
        let text = "a * [ ( ~ _ < & ** x".repeat(50_000);
        let start = std::time::Instant::now();
        let parsed = inl(&text);
        assert!(start.elapsed().as_secs() < 5);
        assert_eq!(inlines_to_plain_text(&parsed), text);
    }

    #[test]
    fn blocks_carry_their_page() {
        let md = "<!-- Page 1 -->\n\n# Title\n\nfirst line\nsecond line\n\n<!-- Page 2 -->\n\n- one\n- two\n  - nested\n\n|a|b|\n|---|---|\n|1|2|\n";
        let blocks = parse_str(md);
        assert_eq!(blocks.iter().map(|b| b.page).collect::<Vec<_>>(), vec![1, 1, 2, 2]);
        assert!(
            matches!(&blocks[1].block, Block::Paragraph(i) if i.iter().any(|x| matches!(x, Inline::LineBreak)))
        );
        let Block::List(list) = &blocks[2].block else { panic!("{:?}", blocks[2].block) };
        assert_eq!(list.items.len(), 2);
        assert!(matches!(list.items[1].blocks.last(), Some(Block::List(_))));
        let Block::Table(table) = &blocks[3].block else { panic!() };
        assert_eq!(table.header_rows, 1);
        assert_eq!(table.grid.len(), 2);
        assert!(matches!(&table.grid[1][1], CellSlot::Origin(c) if !c.is_empty()));
    }

    #[test]
    fn empty_header_row_is_not_a_header() {
        let blocks = parse_str("||||\n|---|---|---|\n|a|b|c|\n");
        let Block::Table(table) = &blocks[0].block else { panic!() };
        assert_eq!(table.header_rows, 0);
        assert_eq!(table.grid.len(), 1);
    }

    #[test]
    fn image_placeholders_resolve_with_their_page() {
        let mut seen = Vec::new();
        let blocks = parse("<!-- Page 3 -->\n\n![Image: Im7](image)\n", &mut |page, name| {
            seen.push((page, name.to_string()));
            ImageSource::Unavailable
        });
        assert_eq!(seen, vec![(3, "Im7".to_string())]);
        assert!(
            matches!(&blocks[0].block, Block::Paragraph(i) if matches!(i[0], Inline::Image { .. }))
        );
    }

    #[test]
    fn code_fences_and_quotes() {
        let blocks = parse_str("```\nfn main() {}\n```\n\n> quoted **text**\n");
        assert!(
            matches!(&blocks[0].block, Block::CodeBlock { text, .. } if text == "fn main() {}")
        );
        assert!(matches!(&blocks[1].block, Block::BlockQuote(inner) if inner.len() == 1));
    }
}
