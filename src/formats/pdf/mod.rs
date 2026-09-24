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
//! - tables broken across pages rejoin, and empty rows and columns drop
//!   ([`tables`]).
//!
//! OCR is out of scope here: a document with scanned or image-only pages
//! errors naming them, whether that is every page or one of a hundred,
//! because output missing those pages would read as complete.
//!
//! [pdf-inspector]: https://github.com/firecrawl/pdf-inspector

mod images;
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
        let tagged = structure::tagged_tables(doc);
        if !tagged.is_empty() {
            // Only the pages the tables sit on need their text again.
            let pages: std::collections::HashSet<u32> = tagged
                .iter()
                .flat_map(|t| {
                    t.rows.iter().flatten().flat_map(|c| c.content.iter().map(|(p, _)| *p))
                })
                .collect();
            match pdf_inspector::extractor::extract_text_with_positions_mem_pages(
                bytes,
                Some(&pages),
            ) {
                Ok(items) => tables::place_tagged(&mut blocks, structure::recover(tagged, &items)),
                Err(e) => log::debug!("tagged tables skipped: {e}"),
            }
        }
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

fn map_error(e: PdfError) -> ConvertError {
    match e {
        PdfError::Encrypted => ConvertError::Encrypted,
        PdfError::Io(e) => ConvertError::Io(e),
        PdfError::NotAPdf(detail) => ConvertError::malformed(format!("not a PDF: {detail}")),
        PdfError::InvalidStructure => ConvertError::malformed("invalid PDF structure"),
        PdfError::Parse(detail) => ConvertError::malformed(detail),
    }
}
