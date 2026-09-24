//! Table recognition passes over the parsed PDF blocks.
//!
//! - Tagged tables replace the running text pdf-inspector made of them: a
//!   borderless table with merged cells reads as one loose paragraph, but its
//!   cells' text still appears there in order, so the table takes exactly
//!   that span's place.
//! - A table that runs over a page break arrives as two tables; the second
//!   joins the first, minus its repeated header row.
//! - Columns and rows with nothing in them are dropped.

use super::markdown::Located;
use super::structure::Recovered;
use crate::model::{Block, CellSlot, Inline, Table, TableKind, inlines_to_plain_text};

/// Put each recovered table where its text sits in `blocks`. A table whose
/// text cannot be found in one piece is left out: the running text still
/// carries its content.
pub(super) fn place_tagged(blocks: &mut Vec<Located>, tables: Vec<Recovered>) {
    for recovered in tables {
        if recovered.signature.chars().count() < 2 {
            continue;
        }
        if !place_one(blocks, recovered) {
            log::debug!("tagged table not found in the extracted text; kept as text");
        }
    }
}

fn place_one(blocks: &mut Vec<Located>, recovered: Recovered) -> bool {
    let (Some(&first), Some(&last)) = (recovered.pages.first(), recovered.pages.last()) else {
        return false;
    };
    let Some(lo) = blocks.iter().position(|b| b.page >= first && b.page <= last) else {
        return false;
    };
    let hi = blocks.iter().rposition(|b| b.page >= first && b.page <= last).unwrap_or(lo);

    // Each block's text, whitespace dropped, and where it starts in the whole.
    let mut haystack = String::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut lens: Vec<usize> = Vec::new();
    for located in &blocks[lo..=hi] {
        starts.push(haystack.chars().count());
        let text = block_signature(&located.block);
        lens.push(text.chars().count());
        haystack.push_str(&text);
    }
    let Some(byte) = haystack.find(&recovered.signature) else {
        return false;
    };
    let start = haystack[..byte].chars().count();
    let end = start + recovered.signature.chars().count();
    let owner = |pos: usize| (0..starts.len()).rev().find(|&i| starts[i] <= pos && lens[i] > 0);
    let (Some(first_block), Some(last_block)) = (owner(start), owner(end - 1)) else {
        return false;
    };
    let before = start - starts[first_block];
    let after = starts[last_block] + lens[last_block] - end;
    if (before > 0 && !splittable(&blocks[lo + first_block].block))
        || (after > 0 && !splittable(&blocks[lo + last_block].block))
    {
        return false;
    }

    let page = blocks[lo + first_block].page;
    let mut replacement = Vec::new();
    if before > 0 {
        let (head, _) = split_block(&blocks[lo + first_block].block, before);
        replacement.extend(head.map(|block| Located { block, page }));
    }
    replacement.push(Located { block: Block::Table(recovered.table), page });
    if after > 0 {
        let tail_block = &blocks[lo + last_block];
        let (_, tail) = split_block(&tail_block.block, lens[last_block] - after);
        replacement.extend(tail.map(|block| Located { block, page: tail_block.page }));
    }
    blocks.splice(lo + first_block..=lo + last_block, replacement);
    true
}

fn splittable(block: &Block) -> bool {
    matches!(block, Block::Paragraph(_) | Block::Heading { .. })
}

/// A block's text with whitespace dropped, in reading order.
fn block_signature(block: &Block) -> String {
    let mut out = String::new();
    collect_text(block, &mut out);
    out.retain(|c| !c.is_whitespace());
    out
}

fn collect_text(block: &Block, out: &mut String) {
    match block {
        Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
            out.push_str(&inlines_to_plain_text(inlines))
        }
        Block::List(list) => {
            for item in &list.items {
                for b in &item.blocks {
                    collect_text(b, out);
                }
            }
        }
        Block::Table(table) => {
            for row in &table.grid {
                for slot in row {
                    if let CellSlot::Origin(cell) = slot {
                        for b in &cell.blocks {
                            collect_text(b, out);
                        }
                    }
                }
            }
        }
        Block::BlockQuote(inner) => inner.iter().for_each(|b| collect_text(b, out)),
        Block::CodeBlock { text, .. } | Block::Math(text) => out.push_str(text),
        Block::Rule => {}
    }
}

/// Split a paragraph or heading after its first `n` non-whitespace
/// characters; halves left with no text are dropped.
fn split_block(block: &Block, n: usize) -> (Option<Block>, Option<Block>) {
    let rebuild = |inlines: Vec<Inline>| -> Option<Block> {
        if crate::model::inlines_are_empty(&inlines) {
            return None;
        }
        Some(match block {
            Block::Heading { level, .. } => Block::heading(*level, inlines),
            _ => Block::Paragraph(inlines),
        })
    };
    let inlines = match block {
        Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => inlines,
        _ => return (None, None),
    };
    let (head, tail) = split_inlines(inlines, n);
    (rebuild(trim_breaks(head)), rebuild(trim_breaks(tail)))
}

fn split_inlines(inlines: &[Inline], n: usize) -> (Vec<Inline>, Vec<Inline>) {
    let mut head = Vec::new();
    let mut tail = Vec::new();
    let mut seen = 0;
    for inline in inlines {
        if seen >= n {
            tail.push(inline.clone());
            continue;
        }
        match inline {
            Inline::Text { text, style } => {
                let mut cut = text.len();
                let mut count = seen;
                for (at, c) in text.char_indices() {
                    if count == n {
                        cut = at;
                        break;
                    }
                    if !c.is_whitespace() {
                        count += 1;
                    }
                }
                seen = count;
                let (a, b) = text.split_at(cut);
                if !a.is_empty() {
                    head.push(Inline::Text { text: a.to_string(), style: *style });
                }
                if !b.is_empty() {
                    tail.push(Inline::Text { text: b.to_string(), style: *style });
                }
            }
            other => {
                seen += inlines_to_plain_text(std::slice::from_ref(other))
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .count();
                head.push(other.clone());
            }
        }
    }
    (head, tail)
}

/// Line breaks and whitespace left dangling at a split edge.
fn trim_breaks(mut inlines: Vec<Inline>) -> Vec<Inline> {
    while matches!(inlines.first(), Some(Inline::LineBreak)) {
        inlines.remove(0);
    }
    while matches!(inlines.last(), Some(Inline::LineBreak)) {
        inlines.pop();
    }
    if let Some(Inline::Text { text, .. }) = inlines.first_mut() {
        *text = text.trim_start().to_string();
    }
    if let Some(Inline::Text { text, .. }) = inlines.last_mut() {
        *text = text.trim_end().to_string();
    }
    inlines.retain(|i| !matches!(i, Inline::Text { text, .. } if text.is_empty()));
    inlines
}

/// Join a table continued on the next page to the part before the break.
pub(super) fn merge_continued(blocks: &mut Vec<Located>) {
    let mut i = 0;
    while i + 1 < blocks.len() {
        let continues = blocks[i + 1].page == blocks[i].page + 1
            && matches!(
                (&blocks[i].block, &blocks[i + 1].block),
                (Block::Table(a), Block::Table(b)) if a.kind == TableKind::Data
                    && b.kind == TableKind::Data
                    && width(a) == width(b)
                    && width(a) > 1
            );
        if continues {
            let Located { block: Block::Table(next), .. } = blocks.remove(i + 1) else {
                unreachable!()
            };
            let Block::Table(table) = &mut blocks[i].block else { unreachable!() };
            if let Err(next) = append(table, next) {
                blocks
                    .insert(i + 1, Located { block: Block::Table(next), page: blocks[i].page + 1 });
                i += 1;
            }
            // Stay on the merged table: it may continue onto a third page.
            continue;
        }
        i += 1;
    }
}

fn width(table: &Table) -> usize {
    table.grid.iter().map(Vec::len).max().unwrap_or(0)
}

fn row_text(row: &[CellSlot]) -> Vec<String> {
    row.iter()
        .map(|slot| match slot {
            CellSlot::Origin(cell) => {
                let mut s = String::new();
                cell.blocks.iter().for_each(|b| collect_text(b, &mut s));
                s.split_whitespace().collect::<Vec<_>>().join(" ")
            }
            CellSlot::Covered { .. } => String::new(),
        })
        .collect()
}

/// Append `next`'s rows to `table`, dropping a header that repeats
/// `table`'s own. Hands `next` back when its spans reach into the rows
/// being dropped.
fn append(table: &mut Table, mut next: Table) -> Result<(), Table> {
    let repeats = table.header_rows > 0
        && next.grid.len() > table.header_rows
        && (0..table.header_rows).all(|r| row_text(&table.grid[r]) == row_text(&next.grid[r]));
    let skip = if repeats { table.header_rows } else { 0 };
    let dangling = next.grid[skip..]
        .iter()
        .flatten()
        .any(|slot| matches!(slot, CellSlot::Covered { origin_row, .. } if *origin_row < skip));
    if dangling {
        return Err(next);
    }
    let offset = table.grid.len();
    for mut row in next.grid.drain(skip..) {
        for slot in &mut row {
            if let CellSlot::Covered { origin_row, .. } = slot {
                *origin_row = *origin_row - skip + offset;
            }
        }
        table.grid.push(row);
    }
    Ok(())
}

/// Drop rows and columns that hold nothing, in tables without spans (a span
/// makes an empty-looking position meaningful).
pub(super) fn tidy(blocks: &mut [Located]) {
    for located in blocks {
        if let Block::Table(table) = &mut located.block {
            tidy_table(table);
        }
    }
}

fn tidy_table(table: &mut Table) {
    let spanless = table.grid.iter().flatten().all(|slot| matches!(slot, CellSlot::Origin(_)));
    if !spanless {
        return;
    }
    let empty = |slot: &CellSlot| matches!(slot, CellSlot::Origin(c) if c.is_empty());
    let mut header_rows = table.header_rows;
    let mut kept = Vec::with_capacity(table.grid.len());
    for (r, row) in std::mem::take(&mut table.grid).into_iter().enumerate() {
        if row.iter().all(empty) {
            if r < table.header_rows {
                header_rows -= 1;
            }
            continue;
        }
        kept.push(row);
    }
    table.grid = kept;
    table.header_rows = header_rows;
    let width = width(table);
    let used: Vec<bool> = (0..width)
        .map(|c| table.grid.iter().any(|row| row.get(c).is_some_and(|s| !empty(s))))
        .collect();
    if used.iter().all(|&u| u) {
        return;
    }
    for row in &mut table.grid {
        let mut c = 0;
        row.retain(|_| {
            let keep = used.get(c).copied().unwrap_or(true);
            c += 1;
            keep
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Cell;

    fn text_table(rows: &[&[&str]], header_rows: usize) -> Table {
        let rows = rows
            .iter()
            .map(|r| r.iter().map(|t| Cell::from_inlines(vec![Inline::plain(*t)])).collect())
            .collect();
        Table::from_rows(rows, header_rows, TableKind::Data)
    }

    fn located(block: Block, page: u32) -> Located {
        Located { block, page }
    }

    #[test]
    fn continued_table_drops_its_repeated_header() {
        let mut blocks = vec![
            located(Block::Table(text_table(&[&["Name", "Qty"], &["a", "1"]], 1)), 1),
            located(Block::Table(text_table(&[&["Name", "Qty"], &["b", "2"]], 1)), 2),
        ];
        merge_continued(&mut blocks);
        assert_eq!(blocks.len(), 1);
        let Block::Table(t) = &blocks[0].block else { panic!() };
        assert_eq!(t.grid.len(), 3);
        assert_eq!(row_text(&t.grid[2]), vec!["b", "2"]);
    }

    #[test]
    fn tables_on_the_same_page_stay_apart() {
        let mut blocks = vec![
            located(Block::Table(text_table(&[&["x", "y"]], 1)), 1),
            located(Block::Table(text_table(&[&["x", "y"]], 1)), 1),
        ];
        merge_continued(&mut blocks);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn tagged_table_replaces_its_running_text() {
        let mut blocks = vec![located(
            Block::Paragraph(vec![Inline::plain("Before. Wide head End Tall B2 After.")]),
            1,
        )];
        let table = text_table(&[&["Wide head", "End"], &["Tall", "B2"]], 0);
        let signature = "WideheadEndTallB2".to_string();
        place_tagged(&mut blocks, vec![Recovered { table, pages: vec![1], signature }]);
        assert_eq!(blocks.len(), 3);
        assert!(
            matches!(&blocks[0].block, Block::Paragraph(i) if inlines_to_plain_text(i) == "Before.")
        );
        assert!(matches!(&blocks[1].block, Block::Table(_)));
        assert!(
            matches!(&blocks[2].block, Block::Paragraph(i) if inlines_to_plain_text(i) == "After.")
        );
    }

    #[test]
    fn empty_columns_and_rows_drop() {
        let mut table = text_table(&[&["a", "", "b"], &["", "", ""], &["c", "", "d"]], 1);
        tidy_table(&mut table);
        assert_eq!(table.grid.len(), 2);
        assert_eq!(width(&table), 2);
    }
}
