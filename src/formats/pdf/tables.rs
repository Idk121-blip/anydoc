//! Table recognition passes over the parsed PDF blocks.
//!
//! - Tagged tables replace the running text pdf-inspector made of them: a
//!   borderless table with merged cells reads as one loose paragraph, but its
//!   cells' text still appears there in order, so the table takes exactly
//!   that span's place.
//! - A table that runs over a page break arrives as two tables; the second
//!   joins the first, minus its repeated header row.
//! - Columns and rows with nothing in them are dropped.

use super::layout::Found;
use super::markdown::Located;
use super::structure::Recovered;
use crate::model::{Block, CellSlot, Inline, Table, TableKind, inlines_to_plain_text};
use std::collections::HashMap;

/// Put each recovered table where its text sits in `blocks`. A table whose
/// text cannot be found in one piece is left out: the running text still
/// carries its content.
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
        // Blocks and cells end in a space so their words stay apart.
        Block::Paragraph(inlines) | Block::Heading { content: inlines, .. } => {
            out.push_str(&inlines_to_plain_text(inlines));
            out.push(' ');
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
        Block::CodeBlock { text, .. } | Block::Math(text) => {
            out.push_str(text);
            out.push(' ');
        }
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

/// Put each region read from page geometry in place of what pdf-inspector
/// made of its text: the blocks on the region's page whose words mostly
/// come from the region give way to it. A table that runs on from the
/// previous page joins the part before the break.
/// A line of a page's text, top to bottom, for the loss check.
pub(super) struct PageLine {
    /// Baseline.
    pub y: f32,
    pub text: String,
}

pub(super) fn place_found(
    blocks: &mut Vec<Located>,
    mut found: Vec<Found>,
    lines: &HashMap<u32, Vec<PageLine>>,
) {
    found.sort_by(|a, b| a.page.cmp(&b.page).then(b.region.y1.total_cmp(&a.region.y1)));
    // Continuations: fold a table at the top of a page into the one ending
    // the page before, and keep only its words, to claim its blocks.
    let mut absorbed = vec![false; found.len()];
    let mut last_table: Option<usize> = None;
    for i in 0..found.len() {
        let joins = last_table.is_some_and(|prev| {
            let (a, b) = (&found[prev], &found[i]);
            b.page == a.page + 1
                && a.at_bottom
                && b.at_top
                && !b.has_caption
                && !a.is_figure
                && !b.is_figure
                && table_width(a) == table_width(b)
                && table_width(a) > 1
        });
        if joins {
            let prev = last_table.unwrap_or(i);
            let next_blocks = std::mem::take(&mut found[i].blocks);
            let next_table = found[i].table.take();
            let mut notes = Vec::new();
            for (n, block) in next_blocks.into_iter().enumerate() {
                match (Some(n) == next_table, block) {
                    (true, Block::Table(table)) => {
                        let host = &mut found[prev];
                        let Some(Block::Table(first)) =
                            host.table.and_then(|t| host.blocks.get_mut(t))
                        else {
                            continue;
                        };
                        if let Err(table) = append(first, table) {
                            // Spans reach into a dropped header: keep it apart.
                            notes.push(Block::Table(table));
                        }
                    }
                    (_, block) => notes.push(block),
                }
            }
            found[prev].blocks.extend(notes);
            found[prev].at_bottom = found[i].at_bottom;
            absorbed[i] = true;
            last_table = Some(prev);
            continue;
        }
        last_table = if found[i].is_figure {
            last_table.filter(|&p| found[p].page == found[i].page)
        } else {
            Some(i)
        };
    }
    let figure_pages: Vec<u32> = found.iter().filter(|f| f.is_figure).map(|f| f.page).collect();
    for (f, absorbed) in found.into_iter().zip(absorbed) {
        let page_lines = lines.get(&f.page).map(Vec::as_slice).unwrap_or(&[]);
        place_region(blocks, f, absorbed, page_lines);
    }
    // Text set along curves that no figure region reached still comes
    // through as a "table" of word fragments; the figure's own text above
    // already carries it.
    blocks.retain(|b| {
        !(figure_pages.contains(&b.page) && matches!(&b.block, Block::Table(t) if fragmented(t)))
    });
}

/// A table whose words are mostly pieces of words.
fn fragmented(table: &Table) -> bool {
    let mut text = String::new();
    collect_text(&Block::Table(table.clone()), &mut text);
    let words: Vec<String> =
        tokens(&text).into_iter().filter(|w| w.chars().all(char::is_alphabetic)).collect();
    let short = words.iter().filter(|w| w.chars().count() <= 3).count();
    words.len() >= 10 && short * 100 >= words.len() * 55
}

fn table_width(found: &Found) -> usize {
    match found.table.and_then(|t| found.blocks.get(t)) {
        Some(Block::Table(table)) => width(table),
        _ => 0,
    }
}

/// Word tokens: letters and digits only, lowercased, so spacing and
/// punctuation differences between the two readings do not matter.
fn tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Page lines outside `found`'s region whose words only the claimed blocks
/// held: paragraphs to keep, above and below the region.
fn orphans(
    blocks: &[Located],
    claimed: &[usize],
    found: &Found,
    lines: &[PageLine],
) -> (Vec<Block>, Vec<Block>) {
    let mut removed: HashMap<String, usize> = HashMap::new();
    for &i in claimed {
        let mut text = String::new();
        collect_text(&blocks[i].block, &mut text);
        for word in tokens(&text) {
            *removed.entry(word).or_default() += 1;
        }
    }
    for word in tokens(&found.text) {
        if let Some(n) = removed.get_mut(&word) {
            *n = n.saturating_sub(1);
        }
    }
    let (mut before, mut after): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    for line in lines {
        if line.y <= found.region.y1 + 2.0 && line.y >= found.region.y0 - 2.0 {
            continue;
        }
        let words = tokens(&line.text);
        if words.len() < 2 {
            continue;
        }
        let mut trial = removed.clone();
        let hits = words
            .iter()
            .filter(|w| match trial.get_mut(*w) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    true
                }
                _ => false,
            })
            .count();
        if hits * 10 >= words.len() * 6 {
            removed = trial;
            if line.y > found.region.y1 {
                before.push(line.text.clone())
            } else {
                after.push(line.text.clone())
            }
        }
    }
    let paragraph = |lines: Vec<String>| -> Vec<Block> {
        if lines.is_empty() {
            Vec::new()
        } else {
            vec![Block::Paragraph(vec![Inline::plain(lines.join(" "))])]
        }
    };
    (paragraph(before), paragraph(after))
}

fn place_region(blocks: &mut Vec<Located>, found: Found, absorbed: bool, lines: &[PageLine]) {
    let mut words: HashMap<String, usize> = HashMap::new();
    for word in tokens(&found.text) {
        *words.entry(word).or_default() += 1;
    }
    let mut letters: HashMap<char, usize> = HashMap::new();
    if found.is_figure {
        for c in found.text.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase) {
            *letters.entry(c).or_default() += 1;
        }
    }
    let mut claimed: Vec<usize> = Vec::new();
    for (i, located) in blocks.iter().enumerate() {
        if located.page != found.page {
            continue;
        }
        let mut text = String::new();
        collect_text(&located.block, &mut text);
        let block_words = tokens(&text);
        if block_words.is_empty() {
            continue;
        }
        let mut trial = words.clone();
        let mut unmatched: Vec<&str> = Vec::new();
        let originals: Vec<&str> = text.split_whitespace().collect();
        let mut matched = 0;
        let mut k = 0;
        for original in &originals {
            let token: String = original
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect();
            if token.is_empty() {
                continue;
            }
            k += 1;
            match trial.get_mut(&token) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    matched += 1;
                }
                _ => unmatched.push(original),
            }
        }
        // Most of the block's words, and all of a short block's: a heading
        // sharing a word or two with a table is not part of it.
        let by_words = matched * 10 >= k * 6 && (k > 4 || matched == k);
        // Text set along curves reaches pdf-inspector in pieces its words
        // no longer match; for figures the letters decide.
        let by_letters = found.is_figure
            && !by_words
            && matches!(&located.block, Block::Table(t) if fragmented(t))
            && text.chars().count() <= 3000
            && {
                let mut trial = letters.clone();
                let mut total = 0;
                let mut hit = 0;
                for c in text.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase) {
                    total += 1;
                    if let Some(n) = trial.get_mut(&c)
                        && *n > 0
                    {
                        *n -= 1;
                        hit += 1;
                    }
                }
                if total > 0 && hit * 4 >= total * 3 {
                    letters = trial;
                    true
                } else {
                    false
                }
            };
        if by_words || by_letters {
            if by_words {
                words = trial;
            }
            claimed.push(i);
        }
    }
    // Short blocks next to the claimed ones that hold nothing but the
    // region's words are its pieces too (a header line pdf-inspector set
    // apart), even where the words were counted already.
    if !claimed.is_empty() {
        let vocabulary: std::collections::HashSet<String> =
            tokens(&found.text).into_iter().collect();
        let page_blocks: Vec<usize> =
            (0..blocks.len()).filter(|&i| blocks[i].page == found.page).collect();
        let mut grew = true;
        while grew {
            grew = false;
            for &i in &page_blocks {
                if claimed.contains(&i) {
                    continue;
                }
                let adjacent =
                    page_blocks.iter().any(|&j| claimed.contains(&j) && j.abs_diff(i) == 1);
                let mut text = String::new();
                collect_text(&blocks[i].block, &mut text);
                let words = tokens(&text);
                if adjacent
                    && !words.is_empty()
                    && words.len() <= 8
                    && words.iter().all(|w| vocabulary.contains(w))
                {
                    claimed.push(i);
                    grew = true;
                }
            }
        }
        claimed.sort_unstable();
    }
    // A note under the region that pdf-inspector ran into the paragraph
    // after it: that paragraph keeps only its own text.
    if let (Some(&last), Some(Block::Paragraph(note))) = (claimed.last(), found.blocks.last())
        && found.table.is_some_and(|t| t + 1 < found.blocks.len())
        && let Some(next) = blocks.get_mut(last + 1)
        && next.page == found.page
    {
        let note: String =
            inlines_to_plain_text(note).chars().filter(|c| !c.is_whitespace()).collect();
        let text = block_signature(&next.block);
        if !note.is_empty() && text.starts_with(&note) && splittable(&next.block) {
            let (_, rest) = split_block(&next.block, note.chars().count());
            match rest {
                Some(rest) => next.block = rest,
                None => claimed.push(last + 1),
            }
        }
    }
    // Lines outside the region that pdf-inspector ran into the blocks given
    // up above (a paragraph merged into its table) go back in, before or
    // after the region by where they sit.
    let (before, after) = orphans(blocks, &claimed, &found, lines);
    let page = found.page;
    let mut inserted: Vec<Located> =
        before.into_iter().map(|block| Located { block, page }).collect();
    if !absorbed {
        inserted.extend(found.blocks.into_iter().map(|block| Located { block, page }));
    }
    inserted.extend(after.into_iter().map(|block| Located { block, page }));
    let at = match claimed.first() {
        Some(&first) => first,
        // pdf-inspector left the region's text out: it goes at the end of
        // its page.
        None => blocks.iter().position(|b| b.page > page).unwrap_or(blocks.len()),
    };
    let mut kept = Vec::with_capacity(blocks.len() + inserted.len());
    let mut inserted = Some(inserted);
    for (i, located) in std::mem::take(blocks).into_iter().enumerate() {
        if i == at {
            kept.extend(inserted.take().unwrap_or_default());
        }
        if !claimed.contains(&i) {
            kept.push(located);
        }
    }
    kept.extend(inserted.take().unwrap_or_default());
    *blocks = kept;
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
