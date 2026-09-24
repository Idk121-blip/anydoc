//! Tables from a tagged PDF's structure tree.
//!
//! A tagged PDF says outright which marked content is a table cell, in which
//! row, spanning how many rows and columns. That beats any geometric guess,
//! and it is the only way to see a borderless table with merged cells: the
//! layout alone reads as loose text. Each `Table` element becomes a grid
//! whose cells are the text pdf-inspector extracted for the cell's marked
//! content ids.

use crate::model::{Block, Cell, GridBuilder, Inline, Style, Table, TableKind};
use crate::shared::header::resolve_header_rows;
use lopdf::{Dictionary, Document, Object, ObjectId};
use pdf_inspector::TextItem;
use std::collections::{HashMap, HashSet};

/// Structure elements visited before giving up on the tree.
const MAX_ELEMENTS: usize = 500_000;
/// Nesting depth of the structure tree walked.
const MAX_DEPTH: usize = 128;

/// One table the structure tree declares, before its text is attached.
#[derive(Debug, Default)]
pub(super) struct TaggedTable {
    pub rows: Vec<Vec<TaggedCell>>,
}

#[derive(Debug, Default)]
pub(super) struct TaggedCell {
    pub header: bool,
    pub col_span: u32,
    pub row_span: u32,
    /// (1-indexed page, marked content id), in content order.
    pub content: Vec<(u32, i64)>,
}

/// A table ready for the document, with the pages its cells sit on.
#[derive(Debug)]
pub(super) struct Recovered {
    pub table: Table,
    pub pages: Vec<u32>,
    /// Every cell's text in row order, whitespace dropped: what the table
    /// reads as in pdf-inspector's running text.
    pub signature: String,
}

/// Every table the structure tree declares. Empty for untagged documents.
pub(super) fn tagged_tables(doc: &Document) -> Vec<TaggedTable> {
    let Some(root) = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"StructTreeRoot").ok())
        .and_then(|r| doc.dereference(r).ok())
        .and_then(|(_, r)| r.as_dict().ok())
    else {
        return Vec::new();
    };
    let page_numbers: HashMap<ObjectId, u32> =
        doc.get_pages().into_iter().map(|(number, id)| (id, number)).collect();
    let role_map = root
        .get(b"RoleMap")
        .ok()
        .and_then(|m| doc.dereference(m).ok())
        .and_then(|(_, m)| m.as_dict().ok());
    let mut walker = Walker {
        doc,
        page_numbers,
        role_map,
        visited: HashSet::new(),
        budget: MAX_ELEMENTS,
        tables: Vec::new(),
    };
    if let Ok(kids) = root.get(b"K") {
        walker.find_tables(kids, 0);
    }
    walker.tables
}

struct Walker<'d> {
    doc: &'d Document,
    page_numbers: HashMap<ObjectId, u32>,
    role_map: Option<&'d Dictionary>,
    visited: HashSet<ObjectId>,
    budget: usize,
    tables: Vec<TaggedTable>,
}

impl<'d> Walker<'d> {
    /// Resolve `object` to a structure element dictionary, once per object.
    fn element(&mut self, object: &'d Object) -> Option<&'d Dictionary> {
        if self.budget == 0 {
            return None;
        }
        self.budget -= 1;
        let (id, resolved) = self.doc.dereference(object).ok()?;
        if let Some(id) = id
            && !self.visited.insert(id)
        {
            return None;
        }
        resolved.as_dict().ok()
    }

    /// The standard role of an element, following the role map.
    fn role(&self, element: &Dictionary) -> Vec<u8> {
        let mut role =
            element.get(b"S").and_then(Object::as_name).map(<[u8]>::to_vec).unwrap_or_default();
        for _ in 0..8 {
            match self.role_map.and_then(|m| m.get(&role).ok()).and_then(|r| r.as_name().ok()) {
                Some(mapped) if mapped != role.as_slice() => role = mapped.to_vec(),
                _ => break,
            }
        }
        role
    }

    fn children(&self, element: &'d Dictionary) -> Vec<&'d Object> {
        match element.get(b"K") {
            Ok(Object::Array(items)) => items.iter().collect(),
            Ok(other) => vec![other],
            Err(_) => Vec::new(),
        }
    }

    fn find_tables(&mut self, kids: &'d Object, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let kids: Vec<&Object> = match kids {
            Object::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for kid in kids {
            if matches!(kid, Object::Integer(_)) {
                continue;
            }
            let Some(element) = self.element(kid) else { continue };
            if self.role(element) == b"Table" {
                let mut table = TaggedTable::default();
                self.rows(element, &mut table, depth + 1);
                if !table.rows.is_empty() {
                    self.tables.push(table);
                }
            } else if let Ok(inner) = element.get(b"K") {
                self.find_tables(inner, depth + 1);
            }
        }
    }

    /// Rows of a table, through `THead`/`TBody`/`TFoot` groups.
    fn rows(&mut self, element: &'d Dictionary, table: &mut TaggedTable, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        for kid in self.children(element) {
            let Some(child) = self.element(kid) else { continue };
            match self.role(child).as_slice() {
                b"TR" => {
                    let mut row = Vec::new();
                    for cell in self.children(child) {
                        let Some(cell) = self.element(cell) else { continue };
                        let role = self.role(cell);
                        if role != b"TD" && role != b"TH" {
                            continue;
                        }
                        let (col_span, row_span) = self.spans(cell);
                        let mut content = Vec::new();
                        self.content(cell, None, &mut content, depth + 1);
                        row.push(TaggedCell { header: role == b"TH", col_span, row_span, content });
                    }
                    table.rows.push(row);
                }
                b"THead" | b"TBody" | b"TFoot" => self.rows(child, table, depth + 1),
                _ => {}
            }
        }
    }

    /// `/ColSpan` and `/RowSpan` from the element's table attributes.
    fn spans(&self, cell: &Dictionary) -> (u32, u32) {
        let mut spans = (1, 1);
        let attributes: Vec<&Object> = match cell.get(b"A").map(|a| self.deref(a)) {
            Ok(Object::Array(items)) => items.iter().collect(),
            Ok(other) => vec![other],
            Err(_) => Vec::new(),
        };
        for attribute in attributes {
            let Ok(dict) = self.deref(attribute).as_dict() else { continue };
            let span = |key: &[u8]| {
                dict.get(key)
                    .ok()
                    .and_then(|v| self.deref(v).as_i64().ok())
                    .map(|v| v.clamp(1, 1000) as u32)
            };
            if let Some(c) = span(b"ColSpan") {
                spans.0 = c;
            }
            if let Some(r) = span(b"RowSpan") {
                spans.1 = r;
            }
        }
        spans
    }

    fn deref(&self, object: &'d Object) -> &'d Object {
        self.doc.dereference(object).map(|(_, o)| o).unwrap_or(object)
    }

    fn page_of(&self, element: &Dictionary) -> Option<u32> {
        let page = element.get(b"Pg").ok()?.as_reference().ok()?;
        self.page_numbers.get(&page).copied()
    }

    /// Marked-content ids under `element`, in order, with their pages.
    fn content(
        &mut self,
        element: &'d Dictionary,
        page: Option<u32>,
        out: &mut Vec<(u32, i64)>,
        depth: usize,
    ) {
        if depth > MAX_DEPTH {
            return;
        }
        let page = self.page_of(element).or(page);
        for kid in self.children(element) {
            match self.deref(kid) {
                Object::Integer(mcid) => {
                    if let Some(page) = page {
                        out.push((page, *mcid));
                    }
                }
                Object::Dictionary(dict) if dict.get(b"MCID").is_ok() => {
                    let mcid = dict.get(b"MCID").ok().and_then(|m| m.as_i64().ok());
                    if let (Some(mcid), Some(page)) = (mcid, self.page_of(dict).or(page)) {
                        out.push((page, mcid));
                    }
                }
                _ => {
                    if let Some(child) = self.element(kid) {
                        self.content(child, page, out, depth + 1);
                    }
                }
            }
        }
    }
}

/// Attach extracted text to the tagged tables. Tables with too little
/// matched text to trust are dropped.
pub(super) fn recover(tables: Vec<TaggedTable>, items: &[TextItem]) -> Vec<Recovered> {
    let mut by_mcid: HashMap<(u32, i64), Vec<&TextItem>> = HashMap::new();
    for item in items {
        if let Some(mcid) = item.mcid
            && matches!(item.item_type, pdf_inspector::types::ItemType::Text)
        {
            by_mcid.entry((item.page, mcid)).or_default().push(item);
        }
    }
    let mut recovered = Vec::new();
    for tagged in tables {
        let mut builder = GridBuilder::new();
        let mut pages: Vec<u32> = Vec::new();
        let mut signature = String::new();
        let mut filled = 0;
        let mut header_rows = 0;
        let mut counting_headers = true;
        let widest =
            tagged.rows.iter().map(|r| r.iter().map(|c| c.col_span as usize).sum::<usize>()).max();
        for row in &tagged.rows {
            builder.next_row();
            counting_headers &= !row.is_empty() && row.iter().all(|c| c.header);
            if counting_headers {
                header_rows += 1;
            }
            for cell in row {
                let fragments: Vec<&TextItem> = cell
                    .content
                    .iter()
                    .flat_map(|key| by_mcid.get(key).into_iter().flatten().copied())
                    .collect();
                for (page, _) in &cell.content {
                    if !pages.contains(page) {
                        pages.push(*page);
                    }
                }
                let inlines = cell_inlines(&fragments);
                let text = crate::model::inlines_to_plain_text(&inlines);
                if !text.trim().is_empty() {
                    filled += 1;
                }
                signature.extend(text.chars().filter(|c| !c.is_whitespace()));
                let blocks =
                    if inlines.is_empty() { Vec::new() } else { vec![Block::Paragraph(inlines)] };
                if builder.place(Cell::spanning(blocks, cell.col_span, cell.row_span)).is_err() {
                    break;
                }
            }
        }
        // A one-cell "table" is a layout box, and an unmatched one has no
        // text to show.
        if filled < 2 || widest.unwrap_or(0) < 2 && tagged.rows.len() < 2 {
            continue;
        }
        let mut table = builder.finish(TableKind::Data);
        table.header_rows = resolve_header_rows(&table, header_rows);
        pages.sort_unstable();
        recovered.push(Recovered { table, pages, signature });
    }
    recovered
}

/// A cell's text fragments as styled inlines: fragments on one line join
/// directly (or with a space across a visible gap), lines join with spaces.
fn cell_inlines(fragments: &[&TextItem]) -> Vec<Inline> {
    let mut inlines: Vec<Inline> = Vec::new();
    let mut prev: Option<&TextItem> = None;
    for item in fragments {
        let text = item.text.as_str();
        if text.is_empty() {
            continue;
        }
        if let Some(p) = prev {
            let same_line = (p.y - item.y).abs() < p.height.max(item.height) * 0.5;
            let gap = item.x - (p.x + p.width);
            let needs_space = !same_line || gap > p.font_size.max(1.0) * 0.2;
            let has_space =
                p.text.ends_with(char::is_whitespace) || text.starts_with(char::is_whitespace);
            if needs_space && !has_space {
                push_text(&mut inlines, " ", Style::PLAIN);
            }
        }
        let style = Style {
            bold: item.is_bold,
            italic: item.is_italic,
            strike: item.is_strikeout,
            code: false,
        };
        push_text(&mut inlines, text, style);
        prev = Some(item);
    }
    // Edge whitespace is padding, never content.
    if let Some(Inline::Text { text, .. }) = inlines.first_mut() {
        *text = text.trim_start().to_string();
    }
    if let Some(Inline::Text { text, .. }) = inlines.last_mut() {
        *text = text.trim_end().to_string();
    }
    inlines.retain(|i| !matches!(i, Inline::Text { text, .. } if text.is_empty()));
    inlines
}

fn push_text(inlines: &mut Vec<Inline>, text: &str, style: Style) {
    if let Some(Inline::Text { text: prev, style: prev_style }) = inlines.last_mut()
        && *prev_style == style
    {
        prev.push_str(text);
        return;
    }
    inlines.push(Inline::Text { text: text.to_string(), style });
}
