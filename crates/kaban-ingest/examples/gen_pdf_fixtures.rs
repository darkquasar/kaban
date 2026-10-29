//! Generates the PDF fixtures under `tests/fixtures/pdf/` (docs-ingest-plan.md DI2.1, OD-D5:
//! born-digital fixtures from a deterministic script using `pdf_oxide`'s creation API).
//!
//! ```text
//!   cargo run --offline -p kaban-ingest --example gen_pdf_fixtures
//! ```
//!
//! Fixed content, no timestamps, no random ids: running it twice yields byte-identical
//! files (`cmp` them; the committed bytes come from this program). `pdf_oxide`'s writer keeps
//! its objects in a `HashMap` and emits them in iteration order, which changes from run to
//! run, so [`canonical`] rewrites each file with its objects in id order and a rebuilt xref
//! table before it is written; the content of every object is untouched. The five fixtures:
//!
//! - `born1.pdf`: two pages written with `PdfWriter` (a bold heading and two paragraphs on
//!   page 1, a heading and one paragraph on page 2; base-14 Helvetica).
//! - `born2.pdf`: one page rendered by `Pdf::from_markdown` from a heading, two paragraphs
//!   and a 3x3 GFM table.
//! - `born3.pdf`: one `PdfWriter` page like `born1` with a heading, one paragraph and three
//!   hand-placed rows whose cells are separated by runs of spaces inside one text string per
//!   row (no pipes, no fence): the uncertain-table shape (RD3-4). On 0.3.78 the Markdown
//!   export collapses the runs to single spaces and joins the rows (`di24-probe.log`), so the
//!   fixture pins what the export yields, not a `table_unknown` block.
//! - `scanned.pdf`: one page with a stroked rectangle and no text operators (the no-text-layer
//!   fixture; stands in for a scanned page without an image decoder in the fixture).
//! - `truncated.pdf`: `born1.pdf` cut at 60 percent (the corrupted-PDF fixture, D8).

#![allow(clippy::print_stderr)] // an example run by hand: the one line names the output dir

use pdf_oxide::api::Pdf;
use pdf_oxide::writer::PdfWriter;
use std::path::Path;

const BODY: &str = "Helvetica";
const BOLD: &str = "Helvetica-Bold";

/// Page 1 of `born1.pdf`: heading, two paragraphs.
const BORN1_P1_PARA1: [&str; 3] = [
    "The first fixture is a born-digital document with a real text layer.",
    "Its lines are placed one by one so the page layout is fixed by this program",
    "and not by a layout engine whose output could drift between versions.",
];
const BORN1_P1_PARA2: [&str; 2] = [
    "A second paragraph follows after a wider vertical gap, so the paragraph",
    "grouping of the extractor has something to separate on this page.",
];
/// Page 2 of `born1.pdf`: heading, one paragraph.
const BORN1_P2_PARA: [&str; 2] = [
    "The second page carries its own heading and a single paragraph, which the",
    "extractor must attribute to page two and nest under that page.",
];

const BORN2_MARKDOWN: &str = "# Born Two

The second fixture is rendered from Markdown by the library itself, so its text layer
comes from a layout engine rather than from hand-placed lines.

Below the two paragraphs sits a small table whose header row names three columns.

| Part | Qty | Unit |
|------|-----|------|
| Bolt | 12 | pcs |
| Nut | 30 | pcs |
";

/// `born3.pdf`: heading, one paragraph, three rows (cells separated by runs of spaces).
const BORN3_PARA: [&str; 2] = [
    "The third fixture places three rows below this paragraph; the cells of each",
    "row sit in one text string, separated by runs of spaces and by no pipe.",
];
const BORN3_ROWS: [&str; 3] = [
    "Part      Qty      Unit",
    "Bolt      12       pcs",
    "Nut       30       pcs",
];

fn born1() -> pdf_oxide::error::Result<Vec<u8>> {
    let mut writer = PdfWriter::new();
    {
        let mut page = writer.add_a4_page();
        page.add_text("Born One", 72.0, 760.0, BOLD, 18.0);
        let mut y = 720.0;
        for line in BORN1_P1_PARA1 {
            page.add_text(line, 72.0, y, BODY, 11.0);
            y -= 14.0;
        }
        y -= 20.0;
        for line in BORN1_P1_PARA2 {
            page.add_text(line, 72.0, y, BODY, 11.0);
            y -= 14.0;
        }
        page.finish();
    }
    {
        let mut page = writer.add_a4_page();
        page.add_text("Second Page", 72.0, 760.0, BOLD, 18.0);
        let mut y = 720.0;
        for line in BORN1_P2_PARA {
            page.add_text(line, 72.0, y, BODY, 11.0);
            y -= 14.0;
        }
        page.finish();
    }
    writer.finish()
}

fn born2() -> pdf_oxide::error::Result<Vec<u8>> {
    Pdf::from_markdown(BORN2_MARKDOWN)?.to_bytes()
}

fn born3() -> pdf_oxide::error::Result<Vec<u8>> {
    let mut writer = PdfWriter::new();
    {
        let mut page = writer.add_a4_page();
        page.add_text("Born Three", 72.0, 760.0, BOLD, 18.0);
        let mut y = 720.0;
        for line in BORN3_PARA {
            page.add_text(line, 72.0, y, BODY, 11.0);
            y -= 14.0;
        }
        y -= 20.0;
        for row in BORN3_ROWS {
            page.add_text(row, 72.0, y, BODY, 11.0);
            y -= 14.0;
        }
        page.finish();
    }
    writer.finish()
}

fn scanned() -> pdf_oxide::error::Result<Vec<u8>> {
    let mut writer = PdfWriter::new();
    {
        let mut page = writer.add_a4_page();
        page.draw_rect(72.0, 500.0, 450.0, 250.0);
        page.finish();
    }
    writer.finish()
}

/// Position of `needle` in `hay` at or after `from`.
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// The decimal number ending at `end` (exclusive) in `hay`, read backwards.
fn number_before(hay: &[u8], end: usize) -> Option<u32> {
    let start = hay[..end]
        .iter()
        .rposition(|b| !b.is_ascii_digit())
        .map_or(0, |p| p + 1);
    std::str::from_utf8(&hay[start..end]).ok()?.parse().ok()
}

/// The same PDF with its indirect objects in id order and a rebuilt cross-reference table
/// (the writer's `HashMap` order is the only run-to-run difference). Objects are located by
/// their `N 0 obj` header at a line start; a stream body is skipped by its direct `/Length`
/// so binary data is never scanned for `endobj`. The trailer dictionary is copied as written.
fn canonical(pdf: &[u8]) -> Result<Vec<u8>, String> {
    let mut objects: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut at = 0usize;
    while let Some(p) = find(pdf, b" 0 obj\n", at) {
        let header_start = pdf[..p]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |n| n + 1);
        let Some(id) = number_before(pdf, p) else {
            return Err("object header without a number".to_owned());
        };
        let body_start = p + " 0 obj\n".len();
        let mut search_from = body_start;
        if let Some(s) = find(pdf, b"stream\n", body_start) {
            let dict_end = find(pdf, b"endobj", body_start).unwrap_or(pdf.len());
            if s < dict_end {
                let len_at = find(&pdf[..s], b"/Length ", body_start)
                    .ok_or_else(|| format!("object {id}: stream without /Length"))?;
                let digits_end = pdf[len_at + "/Length ".len()..]
                    .iter()
                    .position(|b| !b.is_ascii_digit())
                    .map_or(pdf.len(), |n| n + len_at + "/Length ".len());
                let len: usize = std::str::from_utf8(&pdf[len_at + "/Length ".len()..digits_end])
                    .map_err(|e| e.to_string())?
                    .parse()
                    .map_err(|e: std::num::ParseIntError| e.to_string())?;
                search_from = s + "stream\n".len() + len;
            }
        }
        let end = find(pdf, b"endobj\n", search_from)
            .ok_or_else(|| format!("object {id}: no endobj"))?
            + "endobj\n".len();
        objects.push((id, pdf[header_start..end].to_vec()));
        at = end;
    }
    objects.sort_by_key(|(id, _)| *id);
    let trailer_at = find(pdf, b"trailer\n", 0).ok_or("no trailer")? + "trailer\n".len();
    let trailer_end = find(pdf, b"\n", trailer_at).ok_or("unterminated trailer")?;
    let trailer = &pdf[trailer_at..trailer_end];

    let mut out = Vec::with_capacity(pdf.len());
    out.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");
    let size = objects.last().map_or(0, |(id, _)| id + 1);
    let mut offsets = vec![None; size as usize];
    for (id, bytes) in &objects {
        offsets[*id as usize] = Some(out.len());
        out.extend_from_slice(bytes);
    }
    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in offsets.iter().skip(1) {
        match off {
            Some(o) => out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes()),
            None => out.extend_from_slice(b"0000000000 65535 f \n"),
        }
    }
    out.extend_from_slice(b"trailer\n");
    out.extend_from_slice(trailer);
    out.extend_from_slice(format!("\nstartxref\n{xref_at}\n%%EOF\n").as_bytes());
    Ok(out)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pdf");
    std::fs::create_dir_all(&dir)?;
    let born1 = canonical(&born1()?)?;
    let truncated = born1[..born1.len() * 60 / 100].to_vec();
    std::fs::write(dir.join("born1.pdf"), &born1)?;
    std::fs::write(dir.join("born2.pdf"), canonical(&born2()?)?)?;
    std::fs::write(dir.join("born3.pdf"), canonical(&born3()?)?)?;
    std::fs::write(dir.join("scanned.pdf"), canonical(&scanned()?)?)?;
    std::fs::write(dir.join("truncated.pdf"), truncated)?;
    eprintln!("wrote five fixtures under {}", dir.display());
    Ok(())
}
