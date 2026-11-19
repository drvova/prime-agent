//! PDF text for `web.read`: one `## Page N {#page=N}` section per page (the
//! `#page=N` fragment every PDF viewer opens at), so a quote cites its page.

use anyhow::{anyhow, bail};

/// The pages' markdown and how many pages had text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PdfText {
    pub(crate) markdown: String,
    pub(crate) pages: usize,
    pub(crate) empty_pages: usize,
}

/// Stack for the extraction thread: a hostile PDF nests objects deeply, and
/// a stack overflow aborts the process instead of unwinding.
const EXTRACTION_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Extract `bytes` page by page on a dedicated big-stack thread.
///
/// # Errors
///
/// The thread could not start, or [`pdf_to_markdown`] failed.
pub(crate) async fn extract(bytes: Vec<u8>) -> anyhow::Result<PdfText> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("web-read-pdf".to_string())
        .stack_size(EXTRACTION_STACK_BYTES)
        .spawn(move || {
            let _ = sender.send(pdf_to_markdown(&bytes));
        })
        .map_err(|error| anyhow!("PDF extraction thread: {error}"))?;
    receiver
        .await
        .map_err(|_| anyhow!("the PDF extraction thread stopped"))?
}

/// Extract `bytes` page by page. Blocking.
///
/// # Errors
///
/// The PDF cannot be parsed (the extractor's panics on malformed input are
/// caught and reported the same way), or no page has extractable text (a
/// scanned document).
fn pdf_to_markdown(bytes: &[u8]) -> anyhow::Result<PdfText> {
    // pdf-extract panics on some malformed streams instead of erroring.
    let pages = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem_by_pages(bytes))
        .map_err(|_| anyhow!("the PDF could not be parsed"))?
        .map_err(|error| anyhow!("the PDF could not be parsed: {error}"))?;
    let mut sections = Vec::with_capacity(pages.len());
    let mut empty_pages = 0;
    for (index, page) in pages.iter().enumerate() {
        let number = index + 1;
        let mut text = String::with_capacity(page.len());
        let mut blank_run = 0;
        for line in page.lines() {
            let line = line.trim_end();
            if line.trim().is_empty() {
                blank_run += 1;
                if blank_run > 1 || text.is_empty() {
                    continue;
                }
            } else {
                blank_run = 0;
            }
            text.push_str(line);
            text.push('\n');
        }
        let text = text.trim_end();
        if text.is_empty() {
            empty_pages += 1;
            sections.push(format!(
                "## Page {number} {{#page={number}}}\n\n(no extractable text)"
            ));
        } else {
            sections.push(format!("## Page {number} {{#page={number}}}\n\n{text}"));
        }
    }
    if pages.is_empty() || empty_pages == pages.len() {
        bail!("the PDF has no extractable text (a scanned document needs OCR)");
    }
    Ok(PdfText {
        markdown: sections.join("\n\n"),
        pages: pages.len(),
        empty_pages,
    })
}
