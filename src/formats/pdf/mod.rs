//! PDF via [pdf-inspector], into the shared document model.
//!
//! pdf-inspector classifies the document, orders its text, and writes it as
//! Markdown with headings, lists and the tables it can see. That Markdown is
//! read back into the document model ([`markdown`]), so PDFs render through
//! the same writers as every other format. Around it:
//!
//! - images drawn on a page become assets ([`images`]), in place;
//! - a tagged PDF's own table structure replaces what text extraction made
//!   of those tables ([`structure`]), spans included;
//! - the rules and shapes a page draws ([`geometry`]) locate the tables
//!   and figures layout analysis misreads: horizontally ruled tables with
//!   grouped headers, two-column layouts split by a rule, charts whose
//!   labels would otherwise become table cells ([`layout`]);
//! - tables broken across pages rejoin, and empty rows and columns drop
//!   ([`tables`]).
//!
//! OCR is out of scope here: a document with scanned or image-only pages
//! errors naming them, whether that is every page or one of a hundred,
//! because output missing those pages would read as complete.
//!
//! [pdf-inspector]: https://github.com/firecrawl/pdf-inspector

mod geometry;
mod images;
mod layout;
mod markdown;
mod structure;
mod tables;

use crate::error::ConvertError;
use crate::model::Document;
use pdf_inspector::{MarkdownOptions, PdfError, PdfOptions};

pub fn parse(bytes: &[u8]) -> Result<Document, ConvertError> {
    // Placeholders mark where images sit; page markers tell which page's
    // resources name them, and where a table meets a page break.
    let md_options = MarkdownOptions {
        include_images: true,
        include_page_numbers: true,
        ..MarkdownOptions::default()
    };
    let result =
        pdf_inspector::process_pdf_mem_with_options(bytes, PdfOptions::new().markdown(md_options))
            .map_err(map_error)?;
    if !result.pages_needing_ocr.is_empty() {
        // Detection samples content streams and over-reports short or
        // image-heavy text pages; extraction knows which of them yielded none.
        let flagged: Vec<u32> = result.pages_needing_ocr.iter().map(|page| page - 1).collect();
        let pages = pdf_inspector::extract_pages_markdown_mem(bytes, Some(&flagged))
            .map_err(map_error)?
            .pages_needing_ocr;
        if !pages.is_empty() {
            return Err(ConvertError::NeedsOcr { pages, page_count: result.page_count });
        }
    }
    if result.has_encoding_issues {
        log::warn!("broken font encodings detected; extracted text may be garbled");
    }
    let markdown = result.markdown.unwrap_or_default();

    // pdf-inspector keeps its parsed document to itself; images and the
    // structure tree need another look. A file only pdf-inspector's repairs
    // can open still converts, without them.
    let doc = lopdf::Document::load_mem(bytes)
        .inspect_err(|e| log::debug!("PDF assets and structure unavailable: {e}"))
        .ok();
    let mut images = images::Images::new(doc.as_ref());
    let mut blocks = markdown::parse(&markdown, &mut |page, name| images.resolve(page, name));
    let assets = images.finish()?;

    if let Some(doc) = &doc {
        recognize(bytes, doc, &mut blocks);
    }
    tables::merge_continued(&mut blocks);
    tables::tidy(&mut blocks);

    let document = Document {
        blocks: blocks.into_iter().map(|b| b.block).collect(),
        notes: Vec::new(),
        assets,
    };
    let has_text = document.blocks.iter().any(|b| match b {
        crate::model::Block::Paragraph(inlines) => !crate::model::inlines_are_empty(inlines),
        crate::model::Block::Rule => false,
        _ => true,
    });
    if !has_text {
        return Err(ConvertError::Unsupported(format!(
            "PDF has no extractable text ({:?}, {} pages)",
            result.pdf_type, result.page_count
        )));
    }
    Ok(document)
}

/// Tables and figures pdf-inspector's layout analysis misreads: a tagged
/// PDF's own tables, and regions the page's rules and shapes delimit.
fn recognize(bytes: &[u8], doc: &lopdf::Document, blocks: &mut Vec<markdown::Located>) {
    use std::collections::{HashMap, HashSet};
    let tagged = structure::tagged_tables(doc);
    let tagged_pages: HashSet<u32> = tagged
        .iter()
        .flat_map(|t| t.rows.iter().flatten().flat_map(|c| c.content.iter().map(|(p, _)| *p)))
        .collect();
    // Pages that draw something a table or figure could be made of.
    let geometries: Vec<(u32, geometry::PageGeometry)> = doc
        .get_pages()
        .into_iter()
        .map(|(page, id)| (page, geometry::page_geometry(doc, id)))
        .filter(|(_, g)| layout::worth_reading(g))
        .collect();
    // Only the pages that need it have their text read again.
    let pages: HashSet<u32> =
        tagged_pages.iter().copied().chain(geometries.iter().map(|(p, _)| *p)).collect();
    if pages.is_empty() {
        return;
    }
    let items = match pdf_inspector::extractor::extract_text_with_positions_mem_pages(
        bytes,
        Some(&pages),
    ) {
        Ok(items) => items,
        Err(e) => {
            log::debug!("table and figure recognition skipped: {e}");
            return;
        }
    };
    // A tagged table says which cells there are, but producers often leave
    // out the spans and fold captions into it: where the page's rules frame
    // a table too, the one read from them replaces it below.
    if !tagged.is_empty() {
        tables::place_tagged(blocks, structure::recover(tagged, &items));
    }
    let mut by_page: HashMap<u32, Vec<pdf_inspector::TextItem>> = HashMap::new();
    for item in items {
        by_page.entry(item.page).or_default().push(item);
    }
    let mut found = Vec::new();
    let mut lines = HashMap::new();
    for (page, geometry) in &geometries {
        let Some(items) = by_page.get(page) else { continue };
        let chunks = layout::chunks(items);
        let on_page = layout::find(*page, geometry, &chunks);
        if !on_page.is_empty() {
            lines.insert(*page, layout::page_lines(&chunks));
        }
        found.extend(on_page);
    }
    tables::place_found(blocks, found, &lines);
}

fn map_error(e: PdfError) -> ConvertError {
    match e {
        PdfError::Encrypted => ConvertError::Encrypted,
        PdfError::Io(e) => ConvertError::Io(e),
        PdfError::NotAPdf(detail) => ConvertError::malformed(format!("not a PDF: {detail}")),
        PdfError::InvalidStructure => ConvertError::malformed("invalid PDF structure"),
        PdfError::Parse(detail) => ConvertError::malformed(detail),
    }
}
