//! §17.4.39 / §17.4.59 — a floating table (`<w:tblpPr>`) that is taller
//! than the page body must paginate, not loop.
//!
//! The combination that used to hang the layout pass is narrow: a
//! floating table, `<w:tblOverlap w:val="never"/>`, and a row count that
//! overflows one page. `resolve_floating_anchor` reported `Spillover`
//! for *any* overflow, and the caller answers a spillover by pushing a
//! fresh page and re-resolving. A fresh page has no floats, so the
//! anchor collapsed to the page top and overflowed again — an unbounded
//! loop that allocated one `LayoutedPage` per iteration until the OS
//! killed the process.
//!
//! These tests render the real document end-to-end, which is what makes
//! them meaningful: the unit tests in `floating_table.rs` pin the
//! resolver's contract, and these pin that the *caller* honors it.

use std::io::Write;

use dxpdf::render::layout::draw_command::{DrawCommand, LayoutedPage};

/// Create a minimal DOCX (ZIP) in memory wrapping `document_xml`.
fn make_docx(document_xml: &str) -> Vec<u8> {
    let buf = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buf);

    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("[Content_Types].xml", options).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml"
    ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
    )
    .unwrap();

    zip.start_file("_rels/.rels", options).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1"
    Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"
    Target="word/document.xml"/>
</Relationships>"#,
    )
    .unwrap();

    zip.start_file("word/document.xml", options).unwrap();
    zip.write_all(document_xml.as_bytes()).unwrap();

    zip.finish().unwrap().into_inner()
}

/// A floating table of `rows` single-cell rows. `overlap` is spliced
/// into `<w:tblPr>` verbatim so a test can select the `never` variant.
fn floating_table_docx(rows: usize, overlap: &str) -> Vec<u8> {
    let body_rows: String = (0..rows)
        .map(|i| {
            format!(
                r#"<w:tr><w:tc><w:tcPr><w:tcW w:w="4000" w:type="dxa"/></w:tcPr>
                   <w:p><w:r><w:t>row {i}</w:t></w:r></w:p></w:tc></w:tr>"#
            )
        })
        .collect();

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>before</w:t></w:r></w:p>
    <w:tbl>
      <w:tblPr>
        <w:tblpPr w:vertAnchor="text" w:horzAnchor="margin" w:tblpX="0" w:tblpY="20"/>
        {overlap}
        <w:tblW w:w="4000" w:type="dxa"/>
      </w:tblPr>
      <w:tblGrid><w:gridCol w:w="4000"/></w:tblGrid>
      {body_rows}
    </w:tbl>
    <w:p><w:r><w:t>after</w:t></w:r></w:p>
  </w:body>
</w:document>"#
    );
    make_docx(&xml)
}

fn layout_pages(bytes: &[u8]) -> Vec<LayoutedPage> {
    let doc = dxpdf::docx::parse(bytes).expect("parse");
    let (_, pages) = dxpdf::render::resolve_and_layout(doc);
    pages
}

fn layout_page_count(bytes: &[u8]) -> usize {
    layout_pages(bytes).len()
}

fn page_has_text(page: &LayoutedPage, needle: &str) -> bool {
    let text: String = page
        .commands
        .iter()
        .filter_map(|command| match command {
            DrawCommand::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect();
    text.contains(needle)
}

fn text_y(page: &LayoutedPage, needle: &str) -> f32 {
    page.commands
        .iter()
        .find_map(|command| match command {
            DrawCommand::Text { text, position, .. } if text.contains(needle) => {
                Some(position.y.raw())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing text {needle:?}"))
}

/// A small, fixed-height page makes anchor-page admission deterministic:
/// 200pt page height minus 20pt top/bottom margins leaves a 160pt body.
fn placement_docx(
    leading_paragraphs: usize,
    rows: &str,
    tblp_y_twips: i32,
    overlap: &str,
) -> Vec<u8> {
    let leading: String = (0..leading_paragraphs)
        .map(|i| {
            format!(
                r#"<w:p><w:pPr><w:spacing w:before="0" w:after="0"
                          w:line="400" w:lineRule="exact"/></w:pPr>
                     <w:r><w:t>lead {i}</w:t></w:r></w:p>"#
            )
        })
        .collect();

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    {leading}
    <w:tbl>
      <w:tblPr>
        <w:tblpPr w:vertAnchor="text" w:horzAnchor="margin"
                  w:tblpX="0" w:tblpY="{tblp_y_twips}"/>
        {overlap}
        <w:tblW w:w="4000" w:type="dxa"/>
      </w:tblPr>
      <w:tblGrid><w:gridCol w:w="4000"/></w:tblGrid>
      {rows}
    </w:tbl>
    <w:p><w:r><w:t>after</w:t></w:r></w:p>
    <w:sectPr>
      <w:pgSz w:w="6000" w:h="4000"/>
      <w:pgMar w:top="400" w:right="400" w:bottom="400" w:left="400"
               w:header="0" w:footer="0" w:gutter="0"/>
    </w:sectPr>
  </w:body>
</w:document>"#
    );
    make_docx(&xml)
}

fn exact_row(label: &str, height_twips: u32) -> String {
    format!(
        r#"<w:tr>
             <w:trPr><w:trHeight w:val="{height_twips}" w:hRule="exact"/></w:trPr>
             <w:tc><w:tcPr><w:tcW w:w="4000" w:type="dxa"/></w:tcPr>
               <w:p><w:pPr><w:spacing w:before="0" w:after="0"/></w:pPr>
                 <w:r><w:t>{label}</w:t></w:r></w:p>
             </w:tc>
           </w:tr>"#
    )
}

/// The regression. Before the fix this never returned — it allocated
/// pages until the process was OOM-killed, so the assertion below is
/// secondary to the test completing at all.
#[test]
fn tall_floating_table_with_overlap_never_terminates() {
    let pages = layout_page_count(&floating_table_docx(80, r#"<w:tblOverlap w:val="never"/>"#));
    assert!(pages > 1, "80 rows must paginate across pages, got {pages}");
    assert!(
        pages < 20,
        "80 short rows should need a handful of pages, not {pages} — \
         a page count this high means the anchor is being re-pushed \
         rather than the table being sliced"
    );
}

/// The permitted-overlap case always worked; it is the control that
/// shows `never` now paginates the same way rather than differently.
#[test]
fn tall_floating_table_paginates_the_same_with_and_without_overlap_never() {
    let with_never =
        layout_page_count(&floating_table_docx(80, r#"<w:tblOverlap w:val="never"/>"#));
    let default_overlap = layout_page_count(&floating_table_docx(80, ""));
    assert_eq!(
        with_never, default_overlap,
        "tblOverlap only governs collision with prior floats; with no \
         prior float to collide with, both must paginate identically"
    );
}

/// A short floating table still fits on one page — the fix must not
/// have turned every floating table into a paginating one.
#[test]
fn short_floating_table_with_overlap_never_stays_on_one_page() {
    let pages = layout_page_count(&floating_table_docx(3, r#"<w:tblOverlap w:val="never"/>"#));
    assert_eq!(pages, 1, "3 rows fit on the anchor page");
}

/// A multi-page float is admitted by its first legal slice, not by its total
/// height. Six 20pt paragraphs leave 40pt in the 160pt body; the 160pt table
/// does not fit in full, but its first 20pt row does.
#[test]
fn floating_table_uses_remaining_anchor_page_when_first_group_fits() {
    let rows: String = (0..8)
        .map(|i| exact_row(&format!("row {i}"), 400))
        .collect();
    let pages = layout_pages(&placement_docx(6, &rows, 1, ""));

    assert!(pages.len() >= 2, "the 160pt table must paginate");
    assert!(page_has_text(&pages[0], "lead 5"));
    assert!(
        page_has_text(&pages[0], "row 0"),
        "a fitting first row must use the anchor page instead of pre-pushing the whole table"
    );
}

/// If the first row is atomic and taller than the remaining anchor-page
/// space, the paginator emits an empty first slice and moves it to page 2.
#[test]
fn floating_table_moves_when_leading_group_cannot_fit() {
    let rows = r#"<w:tr>
      <w:trPr><w:cantSplit/></w:trPr>
      <w:tc><w:tcPr><w:tcW w:w="4000" w:type="dxa"/></w:tcPr>
        <w:p><w:r><w:t>atomic row</w:t><w:br/><w:t>line 2</w:t>
          <w:br/><w:t>line 3</w:t><w:br/><w:t>line 4</w:t></w:r></w:p>
      </w:tc>
    </w:tr>"#;
    let pages = layout_pages(&placement_docx(6, rows, 1, ""));

    assert!(!page_has_text(&pages[0], "atomic row"));
    assert!(page_has_text(&pages[1], "atomic row"));
}

/// `vertAnchor=text` uses the next regular paragraph's flow position plus
/// `tblpY`. Four 20pt paragraphs put that position at 100pt; the 60pt offset
/// leaves only 20pt, so a 30pt atomic row must move. Measuring from the prior
/// paragraph's 80pt start would incorrectly place the row at 140pt.
#[test]
fn floating_table_text_anchor_uses_next_flow_position() {
    let rows = r#"<w:tr>
      <w:trPr><w:cantSplit/><w:trHeight w:val="600" w:hRule="exact"/></w:trPr>
      <w:tc><w:tcPr><w:tcW w:w="4000" w:type="dxa"/></w:tcPr>
        <w:p><w:r><w:t>offset row</w:t></w:r></w:p>
      </w:tc>
    </w:tr>"#;
    let pages = layout_pages(&placement_docx(4, rows, 1200, ""));

    assert!(!page_has_text(&pages[0], "offset row"));
    assert!(page_has_text(&pages[1], "offset row"));
}

/// The text-relative offset begins at the following paragraph's flow cursor,
/// not inside the preceding paragraph. With one 20pt leading paragraph, a
/// 20pt `tblpY` must move the same table down by exactly 20pt.
#[test]
fn floating_table_text_anchor_offset_starts_at_flow_cursor() {
    let rows = exact_row("anchorrow", 400);
    let unshifted = layout_pages(&placement_docx(1, &rows, 0, ""));
    let shifted = layout_pages(&placement_docx(1, &rows, 400, ""));

    let delta = text_y(&shifted[0], "anchorrow") - text_y(&unshifted[0], "anchorrow");
    assert!(
        (delta - 20.0).abs() < 0.01,
        "400 twips must shift the anchor by 20pt, got {delta:.3}pt"
    );
}

/// A leading vertical merge without explicit row heights remains one atomic
/// paginator group. Its first physical row would fit in the 40pt remainder,
/// but the two-row merged group cannot, so neither row may start there.
#[test]
fn floating_table_keeps_leading_vmerge_group_atomic() {
    let rows = r#"<w:tr>
      <w:tc><w:tcPr><w:tcW w:w="4000" w:type="dxa"/><w:vMerge w:val="restart"/></w:tcPr>
        <w:p><w:r><w:t>merged row</w:t><w:br/><w:t>row 0 line 2</w:t>
          <w:br/><w:t>row 0 line 3</w:t><w:br/><w:t>row 0 line 4</w:t></w:r></w:p>
      </w:tc>
    </w:tr>
    <w:tr>
      <w:tc><w:tcPr><w:tcW w:w="4000" w:type="dxa"/><w:vMerge/></w:tcPr>
        <w:p><w:r><w:t>row 1</w:t><w:br/><w:t>row 1 line 2</w:t></w:r></w:p>
      </w:tc>
    </w:tr>"#;
    let pages = layout_pages(&placement_docx(6, rows, 1, ""));

    assert!(!page_has_text(&pages[0], "merged row"));
    assert!(page_has_text(&pages[1], "merged row"));
}
