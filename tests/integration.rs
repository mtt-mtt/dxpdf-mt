use std::io::Write;

/// Create a minimal DOCX (ZIP) file in memory with the given document.xml content.
fn make_docx(document_xml: &str) -> Vec<u8> {
    let buf = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buf);

    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // Content types
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

    // Relationships
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

    // Main document
    zip.start_file("word/document.xml", options).unwrap();
    zip.write_all(document_xml.as_bytes()).unwrap();

    let cursor = zip.finish().unwrap();
    cursor.into_inner()
}

fn simple_docx(body_content: &str) -> Vec<u8> {
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>{body_content}</w:body>
</w:document>"#
    );
    make_docx(&xml)
}

/// A malformed optional chart part must not make otherwise valid body text
/// unreadable. Chart relationships are intentionally best-effort content.
fn make_docx_with_malformed_chart() -> Vec<u8> {
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
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/charts/chart1.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>
</Types>"#,
    )
    .unwrap();

    zip.start_file("_rels/.rels", options).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#,
    )
    .unwrap();

    zip.start_file("word/document.xml", options).unwrap();
    zip.write_all(
        br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body><w:p><w:r><w:t>Body survives malformed chart</w:t></w:r></w:p></w:body>
</w:document>"#,
    )
    .unwrap();

    zip.start_file("word/_rels/document.xml.rels", options)
        .unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="charts/chart1.xml"/>
</Relationships>"#,
    )
    .unwrap();

    zip.start_file("word/charts/chart1.xml", options).unwrap();
    zip.write_all(br#"<c:chartSpace"#).unwrap();

    zip.finish().unwrap().into_inner()
}

fn make_numbered_docx(document_xml: &str, numbering_xml: &str) -> Vec<u8> {
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
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
  <Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>
</Types>"#,
    )
    .unwrap();

    zip.start_file("_rels/.rels", options).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#,
    )
    .unwrap();

    zip.start_file("word/document.xml", options).unwrap();
    zip.write_all(document_xml.as_bytes()).unwrap();

    zip.start_file("word/_rels/document.xml.rels", options)
        .unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rIdNumbering" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>
</Relationships>"#,
    )
    .unwrap();

    zip.start_file("word/numbering.xml", options).unwrap();
    zip.write_all(numbering_xml.as_bytes()).unwrap();

    zip.finish().unwrap().into_inner()
}

#[test]
fn convert_simple_docx_to_pdf() {
    let docx = simple_docx(r#"<w:p><w:r><w:t>Hello World</w:t></w:r></w:p>"#);
    let pdf = dxpdf::convert(&docx).unwrap();

    // PDF should start with the magic bytes
    assert!(pdf.len() > 4);
    assert_eq!(&pdf[..5], b"%PDF-");
}

#[test]
fn malformed_chart_relationship_does_not_abort_document_parse() {
    let parsed = dxpdf::docx::parse(&make_docx_with_malformed_chart()).unwrap();
    assert_eq!(parsed.body.len(), 1);
    assert!(parsed.charts.is_empty());
}

#[test]
fn convert_docx_with_two_numbering_definitions() {
    use dxpdf::model::{AbstractNumId, NumId};

    let document = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
      <w:body>
        <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="11"/></w:numPr></w:pPr><w:r><w:t>Alpha</w:t></w:r></w:p>
        <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="12"/></w:numPr></w:pPr><w:r><w:t>Beta</w:t></w:r></w:p>
      </w:body>
    </w:document>"#;
    let numbering = r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
      <w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="A%1."/></w:lvl></w:abstractNum>
      <w:num w:numId="11"><w:abstractNumId w:val="1"/></w:num>
      <w:abstractNum w:abstractNumId="2"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="B%1."/></w:lvl></w:abstractNum>
      <w:num w:numId="12"><w:abstractNumId w:val="2"/></w:num>
    </w:numbering>"#;
    let docx = make_numbered_docx(document, numbering);
    let parsed = dxpdf::docx::parse(&docx).unwrap();
    assert_eq!(parsed.numbering.abstract_nums.len(), 2);
    assert_eq!(parsed.numbering.numbering_instances.len(), 2);
    assert_eq!(
        parsed.numbering.numbering_instances[&NumId::new(11)].abstract_num_id,
        AbstractNumId::new(1)
    );
    assert_eq!(
        parsed.numbering.numbering_instances[&NumId::new(12)].abstract_num_id,
        AbstractNumId::new(2)
    );

    let pdf = dxpdf::convert(&docx).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
}

/// §17.9.28: `w:start` is an `ST_DecimalNumber`, so a list may legally start at
/// zero. Seeding the running counter with `start - 1` underflowed `u32` and
/// panicked ("attempt to subtract with overflow") in any build with overflow
/// checks on; release silently wrapped through `u32::MAX` back to the right
/// answer, hiding the crash outside debug/test builds.
#[test]
fn numbering_starting_at_zero_converts() {
    let document = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
      <w:body>
        <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>First</w:t></w:r></w:p>
        <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Second</w:t></w:r></w:p>
      </w:body>
    </w:document>"#;
    let numbering = r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
      <w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="0"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum>
      <w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
    </w:numbering>"#;
    let pdf = dxpdf::convert(&make_numbered_docx(document, numbering)).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
}

/// §17.9.27: `w:startOverride` reaches the same counter seed as `w:start`, so
/// a zero override must not underflow either.
#[test]
fn numbering_start_override_of_zero_converts() {
    let document = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
      <w:body>
        <w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:t>Only</w:t></w:r></w:p>
      </w:body>
    </w:document>"#;
    let numbering = r#"<w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
      <w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="5"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl></w:abstractNum>
      <w:num w:numId="1"><w:abstractNumId w:val="0"/><w:lvlOverride w:ilvl="0"><w:startOverride w:val="0"/></w:lvlOverride></w:num>
    </w:numbering>"#;
    let pdf = dxpdf::convert(&make_numbered_docx(document, numbering)).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
}

/// §17.4.14: a row may address more grid columns than `<w:tblGrid>` declares —
/// producers emit this and Word recovers by extending the grid. The cell-width
/// slice clamped only its end index, so the range inverted and panicked
/// ("range start index 3 out of range for slice of length 2"). Unlike an
/// arithmetic overflow this fires in release too, since slice bounds are
/// always checked.
#[test]
fn table_row_with_more_cells_than_grid_columns_converts() {
    let cell = |t: &str| format!("<w:tc><w:p><w:r><w:t>{t}</w:t></w:r></w:p></w:tc>");
    let docx = simple_docx(&format!(
        r#"<w:tbl>
          <w:tblGrid><w:gridCol w:w="2000"/><w:gridCol w:w="2000"/></w:tblGrid>
          <w:tr>{}{}{}{}</w:tr>
        </w:tbl><w:p/>"#,
        cell("a"),
        cell("b"),
        cell("c"),
        cell("d")
    ));
    let pdf = dxpdf::convert(&docx).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
}

/// §17.4.17: same clamp, reached via `<w:gridBefore>` pointing past the grid.
#[test]
fn table_grid_before_past_declared_columns_converts() {
    let docx = simple_docx(
        r#"<w:tbl>
          <w:tblGrid><w:gridCol w:w="2000"/><w:gridCol w:w="2000"/></w:tblGrid>
          <w:tr><w:trPr><w:gridBefore w:val="5"/></w:trPr>
            <w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc>
          </w:tr>
        </w:tbl><w:p/>"#,
    );
    let pdf = dxpdf::convert(&docx).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
}

#[test]
fn decimal_integer_measurements_convert_without_preprocessing() {
    let docx = simple_docx(
        r#"<w:p>
              <w:pPr><w:spacing w:before="252.00000000000003"/></w:pPr>
              <w:r><w:t>Measured paragraph</w:t></w:r>
            </w:p>
            <w:tbl>
              <w:tblPr><w:tblW w:w="5000.0" w:type="dxa"/></w:tblPr>
              <w:tblGrid><w:gridCol w:w="5000.0"/></w:tblGrid>
              <w:tr><w:tc><w:tcPr><w:tcW w:w="5000.0" w:type="dxa"/></w:tcPr>
                <w:p><w:r><w:t>Measured cell</w:t></w:r></w:p>
              </w:tc></w:tr>
            </w:tbl>"#,
    );
    let pdf = dxpdf::convert(&docx).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
}

#[test]
fn convert_formatted_docx_to_pdf() {
    let docx = simple_docx(
        r#"<w:p>
            <w:pPr><w:jc w:val="center"/></w:pPr>
            <w:r>
                <w:rPr>
                    <w:b/>
                    <w:i/>
                    <w:sz w:val="36"/>
                    <w:color w:val="0000FF"/>
                    <w:rFonts w:ascii="Times New Roman"/>
                </w:rPr>
                <w:t>Formatted Title</w:t>
            </w:r>
        </w:p>
        <w:p>
            <w:r><w:t>Normal paragraph text.</w:t></w:r>
        </w:p>"#,
    );
    let pdf = dxpdf::convert(&docx).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
}

#[test]
fn convert_table_docx_to_pdf() {
    let docx = simple_docx(
        r#"<w:tbl>
            <w:tr>
                <w:tc><w:p><w:r><w:t>Cell A1</w:t></w:r></w:p></w:tc>
                <w:tc><w:p><w:r><w:t>Cell B1</w:t></w:r></w:p></w:tc>
            </w:tr>
            <w:tr>
                <w:tc><w:p><w:r><w:t>Cell A2</w:t></w:r></w:p></w:tc>
                <w:tc><w:p><w:r><w:t>Cell B2</w:t></w:r></w:p></w:tc>
            </w:tr>
        </w:tbl>"#,
    );
    let pdf = dxpdf::convert(&docx).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
}

#[test]
fn convert_empty_document() {
    let docx = simple_docx("");
    let pdf = dxpdf::convert(&docx).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
}

/// Per ECMA-376 §17.4.38 (CT_Tbl), `<w:bookmarkStart>` and `<w:bookmarkEnd>`
/// may appear interleaved with `<w:tr>` elements (they're part of
/// EG_RangeMarkupElements in the choice group). The parser must not split
/// the row sequence across non-row siblings.
#[test]
fn convert_table_with_bookmarks_between_rows() {
    let docx = simple_docx(
        r#"<w:tbl>
            <w:tblPr/>
            <w:tblGrid><w:gridCol w:w="2880"/></w:tblGrid>
            <w:tr><w:tc><w:p><w:r><w:t>R1</w:t></w:r></w:p></w:tc></w:tr>
            <w:bookmarkStart w:id="0" w:name="anchor"/>
            <w:tr><w:tc><w:p><w:r><w:t>R2</w:t></w:r></w:p></w:tc></w:tr>
            <w:tr><w:tc><w:p><w:r><w:t>R3</w:t></w:r></w:p></w:tc></w:tr>
            <w:bookmarkEnd w:id="0"/>
            <w:tr><w:tc><w:p><w:r><w:t>R4</w:t></w:r></w:p></w:tc></w:tr>
        </w:tbl>"#,
    );
    let document = dxpdf::docx::parse(&docx).unwrap();
    let table = match &document.body[0] {
        dxpdf::model::Block::Table(t) => t,
        b => panic!("expected table, got {b:?}"),
    };
    assert_eq!(
        table.rows.len(),
        4,
        "all four rows must survive a bookmarked split",
    );
    let pdf = dxpdf::convert(&docx).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
}

/// `<w:proofErr>`, `<w:permStart>`, `<w:permEnd>` are EG_RunLevelElts that
/// CT_Tbl admits between rows. Word emits these around proofreading marks
/// and document protection ranges; they have no rendered effect but must
/// not break row parsing.
#[test]
fn convert_table_with_proof_err_between_rows() {
    let docx = simple_docx(
        r#"<w:tbl>
            <w:tblPr/>
            <w:tblGrid><w:gridCol w:w="2880"/></w:tblGrid>
            <w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc></w:tr>
            <w:proofErr w:type="spellStart"/>
            <w:tr><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr>
            <w:proofErr w:type="spellEnd"/>
        </w:tbl>"#,
    );
    let document = dxpdf::docx::parse(&docx).unwrap();
    let table = match &document.body[0] {
        dxpdf::model::Block::Table(t) => t,
        b => panic!("expected table, got {b:?}"),
    };
    assert_eq!(table.rows.len(), 2);
}

/// `<w:ins>` at table level wraps inserted rows (CT_RowTrackChange,
/// §17.13.5.16). The wrapped `<w:tr>` children must surface as ordinary
/// rows in the model — the renderer ignores the revision metadata, but
/// the rows themselves must render.
#[test]
fn convert_table_with_revision_tracked_inserted_rows() {
    let docx = simple_docx(
        r#"<w:tbl>
            <w:tblPr/>
            <w:tblGrid><w:gridCol w:w="2880"/></w:tblGrid>
            <w:tr><w:tc><w:p><w:r><w:t>existing</w:t></w:r></w:p></w:tc></w:tr>
            <w:ins w:id="1" w:author="a" w:date="2026-01-01T00:00:00Z">
                <w:tr><w:tc><w:p><w:r><w:t>inserted</w:t></w:r></w:p></w:tc></w:tr>
            </w:ins>
        </w:tbl>"#,
    );
    let document = dxpdf::docx::parse(&docx).unwrap();
    let table = match &document.body[0] {
        dxpdf::model::Block::Table(t) => t,
        b => panic!("expected table, got {b:?}"),
    };
    assert_eq!(
        table.rows.len(),
        2,
        "rows wrapped in <w:ins> must surface alongside ordinary rows",
    );
}

/// `<w:sdt>` at table level (CT_SdtRow, §17.5.2.30) wraps rows in a
/// content control. Rows live under `<w:sdtContent>`; they must still
/// render.
#[test]
fn convert_table_with_sdt_wrapping_rows() {
    let docx = simple_docx(
        r#"<w:tbl>
            <w:tblPr/>
            <w:tblGrid><w:gridCol w:w="2880"/></w:tblGrid>
            <w:tr><w:tc><w:p><w:r><w:t>plain</w:t></w:r></w:p></w:tc></w:tr>
            <w:sdt>
                <w:sdtContent>
                    <w:tr><w:tc><w:p><w:r><w:t>controlled</w:t></w:r></w:p></w:tc></w:tr>
                </w:sdtContent>
            </w:sdt>
        </w:tbl>"#,
    );
    let document = dxpdf::docx::parse(&docx).unwrap();
    let table = match &document.body[0] {
        dxpdf::model::Block::Table(t) => t,
        b => panic!("expected table, got {b:?}"),
    };
    assert_eq!(table.rows.len(), 2);
}

#[test]
fn convert_writes_to_file() {
    let docx = simple_docx(r#"<w:p><w:r><w:t>File test</w:t></w:r></w:p>"#);
    let pdf = dxpdf::convert(&docx).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let out_path = dir.path().join("test.pdf");
    std::fs::write(&out_path, &pdf).unwrap();

    let read_back = std::fs::read(&out_path).unwrap();
    assert_eq!(read_back, pdf);
}

#[test]
fn parse_invalid_zip_returns_error() {
    let result = dxpdf::convert(b"not a zip file");
    assert!(result.is_err());
}

#[test]
fn parse_zip_without_document_xml_returns_error() {
    // Create a valid ZIP but without word/document.xml
    let buf = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buf);
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("dummy.txt", options).unwrap();
    zip.write_all(b"hello").unwrap();
    let cursor = zip.finish().unwrap();
    let bytes = cursor.into_inner();

    let result = dxpdf::convert(&bytes);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("missing required part"),
        "Error should mention a missing part: {err}"
    );
}

/// Regression: a whitespace-only WordprocessingML text run must survive even
/// when the document uses a namespace prefix other than the conventional `w`.
/// quick-xml's serde Deserializer drops this separator before run conversion.
fn assert_whitespace_only_run_roundtrips(wml_namespace: &str) {
    use dxpdf::model::{Block, Inline, RunElement};
    use dxpdf::render::layout::draw_command::DrawCommand;

    // OOXML namespace prefixes are arbitrary. This uses a generated `ns0`
    // prefix instead of the conventional `w` prefix.
    let docx = make_docx(&format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ns0:document xmlns:ns0="{wml_namespace}">
  <ns0:body>
    <ns0:p>
      <ns0:r><ns0:t>Normal</ns0:t></ns0:r>
      <ns0:r><ns0:t xml:space="preserve"> </ns0:t></ns0:r>
      <ns0:r><ns0:rPr><ns0:b/></ns0:rPr><ns0:t>Styled</ns0:t></ns0:r>
      <ns0:r><ns0:t xml:space="preserve"> </ns0:t></ns0:r>
      <ns0:hyperlink ns0:anchor="destination">
        <ns0:r><ns0:rPr><ns0:u ns0:val="single"/></ns0:rPr><ns0:t>Hyperlink</ns0:t></ns0:r>
      </ns0:hyperlink>
    </ns0:p>
  </ns0:body>
</ns0:document>"#,
    ));

    let document = dxpdf::docx::parse(&docx).expect("parse");
    let para = match document.body.first().expect("at least one block") {
        Block::Paragraph(p) => p,
        other => panic!("expected paragraph, got {other:?}"),
    };

    fn collect_text(inlines: &[Inline]) -> String {
        inlines
            .iter()
            .map(|inline| match inline {
                Inline::TextRun(run) => run
                    .content
                    .iter()
                    .filter_map(|element| match element {
                        RunElement::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
                Inline::Hyperlink(link) => collect_text(&link.content),
                _ => String::new(),
            })
            .collect()
    }

    assert_eq!(
        collect_text(&para.content),
        "Normal Styled Hyperlink",
        "whitespace-only runs must survive normal, styled, and hyperlink boundaries"
    );

    let (_, pages) = dxpdf::render::resolve_and_layout(document);
    let laid_out_text: String = pages
        .iter()
        .flat_map(|page| &page.commands)
        .filter_map(|command| match command {
            DrawCommand::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect();

    assert_eq!(
        laid_out_text, "Normal Styled Hyperlink",
        "layout must retain whitespace-only text runs"
    );

    let pdf = dxpdf::convert(&docx).expect("convert");
    let parsed_pdf = lopdf::Document::load_mem(&pdf).expect("parse PDF");
    let page_numbers: Vec<u32> = parsed_pdf.get_pages().into_keys().collect();
    let pdf_text = parsed_pdf
        .extract_text(&page_numbers)
        .expect("extract PDF text");
    assert_eq!(
        pdf_text.replace('\n', ""),
        "Normal Styled Hyperlink",
        "PDF text must retain whitespace-only runs"
    );
}

#[test]
fn whitespace_only_run_with_an_alternate_transitional_wml_prefix_roundtrips() {
    assert_whitespace_only_run_roundtrips(
        "http://schemas.openxmlformats.org/wordprocessingml/2006/main",
    );
}

#[test]
fn whitespace_only_run_with_an_alternate_strict_wml_prefix_roundtrips() {
    assert_whitespace_only_run_roundtrips("http://purl.oclc.org/ooxml/wordprocessingml/main");
}

#[test]
fn convert_multi_paragraph_docx() {
    let mut body = String::new();
    for i in 0..50 {
        body.push_str(&format!(
            r#"<w:p>
                <w:pPr><w:spacing w:before="100" w:after="100"/></w:pPr>
                <w:r><w:t>Paragraph number {i} with some text content to make it wider.</w:t></w:r>
            </w:p>"#
        ));
    }
    let docx = simple_docx(&body);
    let pdf = dxpdf::convert(&docx).unwrap();
    assert_eq!(&pdf[..5], b"%PDF-");
    // Should produce a non-trivial PDF
    assert!(pdf.len() > 100);
}

/// §17.4.17 / §17.4.16: a row whose cells start with `<w:gridBefore>` (and/or
/// end with `<w:gridAfter>`) must offset its first cell to the right by that
/// many grid columns. Word emits this pattern for tables whose visible columns
/// are a subset of the `<w:tblGrid>` columns — typically a thin "phantom"
/// column at each edge to provide consistent table indentation.
///
/// Real-world repro: the "Sanitärräume" table in
/// `test-cases/ÜP - 27.02.2026-Rübenhofstr. 57, 22335 Hamburg, Nr. 7 OG2 Mitte.docx`
/// uses `<w:tblGrid>` `[38, 2905, 6872, 250]` with every row carrying
/// `<w:trPr><w:gridBefore val="1"/><w:gridAfter val="1"/>...</w:trPr>` and 2
/// cells. Without gridBefore handling, cells were placed in grid columns 0-1
/// instead of 1-2, leaving the wide "label" column rendered into the 38-twip
/// column and visually overlapping the value column.
#[test]
fn grid_before_offsets_each_row_first_cell() {
    use dxpdf::render::layout::draw_command::DrawCommand;

    let docx = simple_docx(
        r#"<w:tbl>
            <w:tblPr>
                <w:tblW w:w="10065" w:type="dxa"/>
                <w:tblLayout w:type="fixed"/>
            </w:tblPr>
            <w:tblGrid>
                <w:gridCol w:w="38"/>
                <w:gridCol w:w="2905"/>
                <w:gridCol w:w="6872"/>
                <w:gridCol w:w="250"/>
            </w:tblGrid>
            <w:tr>
                <w:trPr>
                    <w:gridBefore w:val="1"/><w:gridAfter w:val="1"/>
                    <w:wBefore w:w="38" w:type="dxa"/>
                    <w:wAfter w:w="250" w:type="dxa"/>
                </w:trPr>
                <w:tc>
                    <w:tcPr><w:tcW w:w="2905" w:type="dxa"/></w:tcPr>
                    <w:p><w:r><w:t>LeftA</w:t></w:r></w:p>
                </w:tc>
                <w:tc>
                    <w:tcPr><w:tcW w:w="6872" w:type="dxa"/></w:tcPr>
                    <w:p><w:r><w:t>RightA</w:t></w:r></w:p>
                </w:tc>
            </w:tr>
            <w:tr>
                <w:trPr>
                    <w:gridBefore w:val="1"/><w:gridAfter w:val="1"/>
                    <w:wBefore w:w="38" w:type="dxa"/>
                    <w:wAfter w:w="250" w:type="dxa"/>
                </w:trPr>
                <w:tc>
                    <w:tcPr><w:tcW w:w="2905" w:type="dxa"/></w:tcPr>
                    <w:p><w:r><w:t>LeftB</w:t></w:r></w:p>
                </w:tc>
                <w:tc>
                    <w:tcPr><w:tcW w:w="6872" w:type="dxa"/></w:tcPr>
                    <w:p><w:r><w:t>RightB</w:t></w:r></w:p>
                </w:tc>
            </w:tr>
        </w:tbl>"#,
    );

    let document = dxpdf::docx::parse(&docx).expect("parse");
    let (_, pages) = dxpdf::render::resolve_and_layout(document);
    let cmds: Vec<&DrawCommand> = pages.iter().flat_map(|p| p.commands.iter()).collect();

    let position_of = |needle: &str| -> Option<(f32, f32)> {
        cmds.iter().find_map(|c| match c {
            DrawCommand::Text { position, text, .. } if text.as_ref() == needle => {
                Some((position.x.raw(), position.y.raw()))
            }
            _ => None,
        })
    };

    let (x_la, y_la) = position_of("LeftA").expect("LeftA present");
    let (x_ra, y_ra) = position_of("RightA").expect("RightA present");
    let (x_lb, y_lb) = position_of("LeftB").expect("LeftB present");
    let (x_rb, y_rb) = position_of("RightB").expect("RightB present");

    // Both rows align to the same columns — no per-row drift from gridBefore.
    assert!(
        (x_la - x_lb).abs() < 0.01,
        "LeftA ({x_la}) and LeftB ({x_lb}) must share the same x — both \
         rows declare gridBefore=1 so the first cell starts at the same \
         absolute grid column"
    );
    assert!(
        (x_ra - x_rb).abs() < 0.01,
        "RightA ({x_ra}) and RightB ({x_rb}) must share the same x"
    );
    assert!(y_la == y_ra, "row 0 cells share the same y baseline");
    assert!(y_lb == y_rb, "row 1 cells share the same y baseline");
    assert!(y_lb > y_la, "row 1 sits below row 0");

    // Right column must sit clearly to the right of the left column. The
    // 2905-twip left column is ~145pt wide once scaled down to fit page
    // width, so the right column's x should exceed the left column's x by
    // at least ~50pt — well beyond any rounding noise. Without gridBefore
    // handling, the right column would land just ~1.9pt past the left
    // column (the phantom 38-twip first grid column) and overlap.
    assert!(
        x_ra - x_la > 50.0,
        "RightA ({x_ra}) must be well to the right of LeftA ({x_la}) — \
         small separation indicates gridBefore was ignored and the right \
         column overlapped the left"
    );
}

/// §17.3.1.30 — a `<w:ptab>` must parse (it previously aborted the whole parse
/// with an "unknown variant `ptab`" error) and lay content out by position.
#[test]
fn position_tab_parses_and_converts() {
    let docx = simple_docx(
        r#"<w:p>
            <w:r><w:t>PtabLeft</w:t></w:r>
            <w:r><w:ptab w:relativeTo="margin" w:alignment="center" w:leader="none"/></w:r>
            <w:r><w:t>PtabCenter</w:t></w:r>
            <w:r><w:ptab w:relativeTo="margin" w:alignment="right" w:leader="none"/></w:r>
            <w:r><w:t>PtabRight</w:t></w:r>
        </w:p>"#,
    );

    // The regression: this must not error at parse time.
    let pdf = dxpdf::convert(&docx).expect("ptab document should convert");
    assert!(pdf.len() > 4 && &pdf[..5] == b"%PDF-");
}

#[test]
fn position_tab_lays_out_by_position() {
    use dxpdf::render::layout::draw_command::DrawCommand;
    let docx = simple_docx(
        r#"<w:p>
            <w:r><w:t>PtabLeft</w:t></w:r>
            <w:r><w:ptab w:relativeTo="margin" w:alignment="center" w:leader="none"/></w:r>
            <w:r><w:t>PtabCenter</w:t></w:r>
            <w:r><w:ptab w:relativeTo="margin" w:alignment="right" w:leader="none"/></w:r>
            <w:r><w:t>PtabRight</w:t></w:r>
        </w:p>"#,
    );

    let document = dxpdf::docx::parse(&docx).expect("parse");
    let (_, pages) = dxpdf::render::resolve_and_layout(document);
    let cmds: Vec<&DrawCommand> = pages.iter().flat_map(|p| p.commands.iter()).collect();
    let x_of = |needle: &str| -> f32 {
        cmds.iter()
            .find_map(|c| match c {
                DrawCommand::Text { position, text, .. } if text.as_ref() == needle => {
                    Some(position.x.raw())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{needle} present"))
    };

    let (x_left, x_center, x_right) = (x_of("PtabLeft"), x_of("PtabCenter"), x_of("PtabRight"));
    // The full pipeline offsets by the page's left margin, so the left run
    // sits at that margin (not 0); the two ptabs then drive the following
    // content strictly rightward across the page (center, then right).
    assert!(
        x_left < x_center && x_center < x_right,
        "ptabs advance content across the page: {x_left} < {x_center} < {x_right}"
    );
    // The right-aligned ptab pushes its content far past center — well beyond
    // where simple left-to-right flow (left + center widths) would land.
    assert!(
        x_right - x_center > 100.0,
        "right ptab should right-align near the margin, got center={x_center} right={x_right}"
    );
}

/// Regression: a body-anchored `wps:wsp` shape's text-box content (`wps:txbx`)
/// must render. `layout_section` emitted the shape's fill/stroke but dropped its
/// `text_commands`, so shape text vanished in the body (it only worked for
/// paragraph-anchored-in-cell and header/footer shapes, via the stacker).
#[test]
fn body_shape_txbx_text_is_rendered() {
    use dxpdf::render::layout::draw_command::DrawCommand;

    let doc_xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
            xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
            xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
  <w:body><w:p><w:r><w:drawing>
    <wp:anchor distT="0" distB="0" distL="0" distR="0" relativeHeight="1" behindDoc="0" locked="0" allowOverlap="1">
      <wp:simplePos x="0" y="0"/>
      <wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH>
      <wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV>
      <wp:extent cx="2743200" cy="914400"/><wp:wrapNone/><wp:docPr id="1" name="tb"/>
      <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
        <wps:wsp><wps:cNvSpPr/>
          <wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="2743200" cy="914400"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:noFill/></wps:spPr>
          <wps:txbx><w:txbxContent><w:p><w:r><w:t>SHAPETEXTZZ</w:t></w:r></w:p></w:txbxContent></wps:txbx>
          <wps:bodyPr/>
        </wps:wsp>
      </a:graphicData></a:graphic>
    </wp:anchor>
  </w:drawing></w:r></w:p></w:body>
</w:document>"#;

    let docx = make_docx(doc_xml);
    let doc = dxpdf::docx::parse(&docx).expect("parse");
    let (_, pages) = dxpdf::render::resolve_and_layout(doc);
    let found = pages
        .iter()
        .flat_map(|p| &p.commands)
        .any(|c| matches!(c, DrawCommand::Text { text, .. } if text.contains("SHAPETEXTZZ")));
    assert!(
        found,
        "wps:txbx text-box content must render as a Text draw command"
    );
}
