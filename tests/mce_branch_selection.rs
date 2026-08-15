//! §M.1.2 — one `<mc:AlternateContent>`, one live branch, one renderer-wide
//! answer about which.
//!
//! Exactly one branch of the element is live: a consumer takes the first
//! `<mc:Choice>` whose requirements it can meet and otherwise the
//! `<mc:Fallback>`, never both, because both describe the *same* object — the
//! Choice in DrawingML, the Fallback in VML for clients that predate it.
//!
//! The failure this file pins is what happens when the answer is computed more
//! than once. The DrawingML float walkers asked one predicate, the VML float
//! walkers asked none at all (they skipped the element outright), and the
//! inline collector asked a third, narrower one. A Choice this renderer cannot
//! draw plus a Fallback holding VML therefore produced *no float*: the
//! fallback's text still reached the page through the collector, but its
//! geometry reached nothing.
//!
//! These tests are written against the page, not against a predicate, because
//! the bug was never visible in any single predicate's answer — it lived in the
//! disagreement between them.

use std::io::Write;

use dxpdf::render::layout::draw_command::{DrawCommand, LayoutedPage};

/// A 1×1 red PNG. Small enough to inline, real enough to survive image
/// pre-loading — a fixture whose bytes never decode would give `rId7` no media
/// entry and quietly turn every image assertion below into a tautology.
const RED_DOT_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0,
    0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 218, 99, 248, 207, 192, 0, 0, 3, 1,
    1, 0, 247, 3, 65, 67, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

fn make_docx(document_xml: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let o = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        zip.start_file("[Content_Types].xml", o).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="png" ContentType="image/png"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
        )
        .unwrap();

        zip.start_file("_rels/.rels", o).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#,
        )
        .unwrap();

        zip.start_file("word/_rels/document.xml.rels", o).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/image1.png"/>
</Relationships>"#,
        )
        .unwrap();

        zip.start_file("word/media/image1.png", o).unwrap();
        zip.write_all(RED_DOT_PNG).unwrap();

        zip.start_file("word/document.xml", o).unwrap();
        zip.write_all(document_xml.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

/// Wrap `run_body` — whatever a `<w:r>` may contain — in a one-paragraph
/// document with every namespace these fixtures use declared on the root.
fn document(run_body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"
            xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
            xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
            xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
            xmlns:v="urn:schemas-microsoft-com:vml"
            xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
  <w:body>
    <w:p><w:r>{run_body}</w:r></w:p>
  </w:body>
</w:document>"#
    )
}

/// A `<w:pict>` holding one absolutely-positioned `<v:rect>` whose text box
/// says `label`.
///
/// This is what a `<mc:Fallback>` carries in the wild, and — pasted outside an
/// `<mc:AlternateContent>` — it is also the control: the same markup, with
/// nobody to select a branch, has to render the same way.
fn vml_rect(label: &str) -> String {
    format!(
        r##"<w:pict>
              <v:rect style="position:absolute;margin-left:100pt;margin-top:20pt;width:120pt;height:50pt"
                      fillcolor="#DDDDDD">
                <v:textbox><w:txbxContent>
                  <w:p><w:r><w:t>{label}</w:t></w:r></w:p>
                </w:txbxContent></v:textbox>
              </v:rect>
            </w:pict>"##
    )
}

/// A DrawingML `wps:wsp` anchored shape whose text body says `label` — the
/// modern half of the pair, and the branch a `Requires="wps"` Choice carries.
fn wps_shape(label: &str) -> String {
    format!(
        r#"<w:drawing>
             <wp:anchor distT="0" distB="0" distL="0" distR="0" simplePos="0"
                        relativeHeight="1" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1">
               <wp:simplePos x="0" y="0"/>
               <wp:positionH relativeFrom="margin"><wp:posOffset>1270000</wp:posOffset></wp:positionH>
               <wp:positionV relativeFrom="paragraph"><wp:posOffset>254000</wp:posOffset></wp:positionV>
               <wp:extent cx="1524000" cy="635000"/>
               <wp:wrapNone/>
               <wp:docPr id="1" name="Box"/>
               <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
                 <wps:wsp>
                   <wps:cNvSpPr/>
                   <wps:spPr>
                     <a:xfrm><a:off x="0" y="0"/><a:ext cx="1524000" cy="635000"/></a:xfrm>
                     <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                     <a:solidFill><a:srgbClr val="DDDDDD"/></a:solidFill>
                   </wps:spPr>
                   <wps:txbx><w:txbxContent>
                     <w:p><w:r><w:t>{label}</w:t></w:r></w:p>
                   </w:txbxContent></wps:txbx>
                   <wps:bodyPr rot="0" vert="horz" wrap="square" lIns="0" tIns="0" rIns="0" bIns="0" anchor="t"/>
                 </wps:wsp>
               </a:graphicData></a:graphic>
             </wp:anchor>
           </w:drawing>"#
    )
}

/// The same rect with no `position:absolute`, so it resolves to no float at
/// all — a `<w:pict>` that parses but yields no geometry. Its text box is
/// still collected inline, which is the Tier-0 placeholder behaviour.
fn unpositioned_vml_rect(label: &str) -> String {
    format!(
        r##"<w:pict>
              <v:rect style="width:120pt;height:50pt" fillcolor="#DDDDDD">
                <v:textbox><w:txbxContent>
                  <w:p><w:r><w:t>{label}</w:t></w:r></w:p>
                </w:txbxContent></v:textbox>
              </v:rect>
            </w:pict>"##
    )
}

/// §14.1.2.19 `<v:shape type="#_x0000_t75">` wrapping a `<v:imagedata>` — the
/// standard pre-DrawingML way to place an image, and the other half of what a
/// `<mc:Fallback>` carries. It reaches the page through a *different* walker
/// than the rect above, so the two need separate coverage.
fn vml_image() -> String {
    r##"<w:pict>
          <v:shape type="#_x0000_t75"
                   style="position:absolute;margin-left:60pt;margin-top:10pt;width:40pt;height:40pt">
            <v:imagedata r:id="rId7"/>
          </v:shape>
        </w:pict>"##
        .to_string()
}

/// A `wps:wsp` in a `<wp:inline>`, not a `<wp:anchor>` — a drawing this
/// renderer parses fine and still places no float, because liveness asks
/// whether the branch yields an *anchored* object.
fn inline_wps_shape() -> String {
    r#"<w:drawing>
         <wp:inline distT="0" distB="0" distL="0" distR="0">
           <wp:extent cx="1524000" cy="635000"/>
           <wp:docPr id="2" name="InlineBox"/>
           <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape">
             <wps:wsp>
               <wps:cNvSpPr/>
               <wps:spPr>
                 <a:xfrm><a:off x="0" y="0"/><a:ext cx="1524000" cy="635000"/></a:xfrm>
                 <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
               </wps:spPr>
             </wps:wsp>
           </a:graphicData></a:graphic>
         </wp:inline>
       </w:drawing>"#
        .to_string()
}

/// A DrawingML picture in `<wp:inline>`. This is the modern branch used by
/// ONLYOFFICE's table-cell image previews even though the Choice declares
/// `Requires="wpg"` rather than `pic`.
fn inline_picture() -> String {
    r#"<w:drawing>
         <wp:inline distT="0" distB="0" distL="0" distR="0">
           <wp:extent cx="1260000" cy="792000"/>
           <wp:docPr id="3" name="InlinePicture"/>
           <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture">
             <pic:pic xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture">
               <pic:nvPicPr><pic:cNvPr id="3" name="picture"/></pic:nvPicPr>
               <pic:blipFill><a:blip r:embed="rId7"/><a:stretch/></pic:blipFill>
             </pic:pic>
           </a:graphicData></a:graphic>
         </wp:inline>
       </w:drawing>"#
        .to_string()
}

/// An inline Word 2010 group. The outer drawing parses, but `wpg:wgp` has no
/// fragment renderer yet, so its VML fallback must remain live.
fn inline_wpg_group() -> String {
    r#"<w:drawing>
         <wp:inline distT="0" distB="0" distL="0" distR="0">
           <wp:extent cx="1260000" cy="792000"/>
           <wp:docPr id="4" name="InlineGroup"/>
           <a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingGroup">
             <wpg:wgp xmlns:wpg="http://schemas.microsoft.com/office/word/2010/wordprocessingGroup"/>
           </a:graphicData></a:graphic>
         </wp:inline>
       </w:drawing>"#
        .to_string()
}

/// An `<mc:AlternateContent>` with one Choice declaring `requires`.
///
/// `requires="wps"` is a namespace this renderer understands, so the Choice
/// survives parsing and its *content* decides whether the branch is live.
/// An unknown token (`"x99"`) drops the whole Choice at parse per §M.2.2 —
/// the shortest honest way to write "a Choice this consumer cannot meet".
///
/// §M.1.2's content model for both branches is `drawing | pict`, which is why
/// nothing here nests an `<mc:AlternateContent>` inside another: the parser
/// rejects it, so the nesting `live_mc_branch` recurses through is pinned in
/// its own unit tests rather than from a document.
fn alternate_content(requires: &str, choice: &str, fallback: &str) -> String {
    format!(
        r#"<mc:AlternateContent>
             <mc:Choice Requires="{requires}">{choice}</mc:Choice>
             <mc:Fallback>{fallback}</mc:Fallback>
           </mc:AlternateContent>"#
    )
}

fn layout(run_body: &str) -> Vec<LayoutedPage> {
    let doc = dxpdf::docx::parse(&make_docx(&document(run_body))).expect("parse");
    dxpdf::render::resolve_and_layout(doc).1
}

/// How many times `label` is drawn anywhere on the document.
fn text_count(pages: &[LayoutedPage], label: &str) -> usize {
    pages
        .iter()
        .flat_map(|p| p.commands.iter())
        .filter(|c| matches!(c, DrawCommand::Text { text, .. } if text.trim() == label))
        .count()
}

/// How many filled/stroked paths are drawn — one per shape that reached the
/// page as geometry.
fn path_count(pages: &[LayoutedPage]) -> usize {
    pages
        .iter()
        .flat_map(|p| p.commands.iter())
        .filter(|c| matches!(c, DrawCommand::Path { .. }))
        .count()
}

/// How many placed images are drawn.
fn image_count(pages: &[LayoutedPage]) -> usize {
    pages
        .iter()
        .flat_map(|p| p.commands.iter())
        .filter(|c| matches!(c, DrawCommand::Image { .. }))
        .count()
}

// --- The control: a fallback is a `w:pict`, and must render like one ---------

/// The same VML rect outside any `<mc:AlternateContent>`. Whatever branch
/// selection does, it cannot do *better* than this — so this is the number the
/// fallback cases are measured against, not a hand-written constant.
#[test]
fn a_bare_vml_rect_draws_its_geometry_and_its_text_once() {
    let pages = layout(&vml_rect("Fallback"));

    assert_eq!(path_count(&pages), 1, "the rect's own geometry");
    assert_eq!(text_count(&pages, "Fallback"), 1, "its text box, once");
}

// --- No drawable Choice: the Fallback is live, for every walker -------------

/// The bug. A Choice this renderer cannot meet, and a Fallback holding VML.
/// The VML float walkers used to answer `AlternateContent(_) => {}`, so the
/// rect's geometry reached nothing at all — while the inline collector
/// happily emitted its text, leaving a floating label over blank page.
#[test]
fn an_unmeetable_choice_hands_the_whole_fallback_over() {
    let pages = layout(&alternate_content("x99", "", &vml_rect("Fallback")));

    assert_eq!(
        path_count(&pages),
        1,
        "the Fallback's rect is the live branch — its geometry must be drawn",
    );
    assert_eq!(
        text_count(&pages, "Fallback"),
        1,
        "and its text exactly once"
    );
}

/// The same, for the other VML float walker. Shape geometry and image
/// placement are extracted by two separate passes over the fallback, and both
/// used to skip `<mc:AlternateContent>` outright; a test that only covered the
/// rect would leave half the fix unpinned.
#[test]
fn an_unmeetable_choice_hands_over_a_vml_image_too() {
    let bare = layout(&vml_image());
    let wrapped = layout(&alternate_content("x99", "", &vml_image()));

    assert_eq!(
        image_count(&bare),
        1,
        "precondition: the bare pict places it"
    );
    assert_eq!(
        image_count(&wrapped),
        1,
        "the Fallback's image is the live branch",
    );
}

/// …and a drawable Choice keeps that image inert, so the fix to the image
/// walker cannot reintroduce the double render on the other side.
#[test]
fn a_drawable_choice_leaves_a_vml_fallback_image_inert() {
    let pages = layout(&alternate_content(
        "wps",
        &wps_shape("Choice"),
        &vml_image(),
    ));

    assert_eq!(image_count(&pages), 0, "the Fallback's image is not live");
    assert_eq!(text_count(&pages, "Choice"), 1);
}

/// Geometry and text of one VML fallback come from *different* walkers — the
/// float extractor draws the rect, the inline collector draws its text box —
/// and that is not a double render, it is the two halves of one. Pinned
/// against the bare-pict control so a future "the fallback has one owner"
/// refactor cannot quietly drop either half.
#[test]
fn a_live_fallback_renders_exactly_like_the_bare_pict_it_is() {
    let bare = layout(&vml_rect("Fallback"));
    let wrapped = layout(&alternate_content("x99", "", &vml_rect("Fallback")));

    assert_eq!(path_count(&wrapped), path_count(&bare));
    assert_eq!(
        text_count(&wrapped, "Fallback"),
        text_count(&bare, "Fallback"),
    );
}

/// A Choice whose namespace we *do* understand but whose content yields no
/// anchored object is still not drawable — liveness is a question about
/// content, not about the `Requires` attribute. Widening it to a namespace
/// check would light up this Choice and strand the Fallback's rect again.
#[test]
fn a_meetable_choice_with_nothing_drawable_still_yields_to_the_fallback() {
    let pages = layout(&alternate_content(
        "wps",
        &inline_wps_shape(),
        &vml_rect("Fallback"),
    ));

    assert_eq!(text_count(&pages, "Fallback"), 1, "the Fallback is live");
    assert!(
        path_count(&pages) >= 1,
        "and its rect reaches the page as geometry",
    );
}

/// A supported inline picture is owned by the fragment collector. Selecting
/// the Choice must make every part of the VML fallback inert, otherwise the
/// fallback rect and label are rendered alongside the picture.
#[test]
fn an_inline_picture_choice_reaches_the_page_without_its_fallback() {
    let pages = layout(&alternate_content(
        "wpg",
        &inline_picture(),
        &vml_rect("Fallback"),
    ));

    assert_eq!(image_count(&pages), 1, "the Choice's inline picture");
    assert_eq!(path_count(&pages), 0, "the fallback rect stays inert");
    assert_eq!(text_count(&pages, "Fallback"), 0);
}

#[test]
fn an_inline_wpg_group_still_yields_to_the_fallback() {
    let pages = layout(&alternate_content(
        "wpg",
        &inline_wpg_group(),
        &vml_rect("Fallback"),
    ));

    assert_eq!(image_count(&pages), 0);
    assert!(path_count(&pages) >= 1, "the fallback rect is live");
    assert_eq!(text_count(&pages, "Fallback"), 1);
}

/// A Fallback with no anchored content at all keeps its long-standing
/// behaviour: the text reaches the page inline, at the host paragraph, as the
/// Tier-0 placeholder it has always been. Nothing about extending the float
/// walkers may cost this case its text.
#[test]
fn a_fallback_with_no_anchored_content_still_reaches_the_page_inline() {
    let pages = layout(&alternate_content(
        "x99",
        "",
        &unpositioned_vml_rect("Fallback"),
    ));

    assert_eq!(path_count(&pages), 0, "there is no geometry to draw");
    assert_eq!(text_count(&pages, "Fallback"), 1, "but the text survives");
}

// --- A drawable Choice: the Fallback is inert, for every walker -------------

/// The invariant a naive fix breaks. Both branches describe one rectangle;
/// giving the VML walkers a Fallback arm without asking about the Choice draws
/// it twice — once from each branch.
#[test]
fn a_drawable_choice_leaves_its_vml_fallback_completely_inert() {
    let pages = layout(&alternate_content(
        "wps",
        &wps_shape("Choice"),
        &vml_rect("Fallback"),
    ));

    assert_eq!(path_count(&pages), 1, "one rectangle, drawn once");
    assert_eq!(text_count(&pages, "Choice"), 1, "the Choice's text body");
    assert_eq!(
        text_count(&pages, "Fallback"),
        0,
        "the Fallback describes the same object — none of it is live",
    );
}

// --- Neither branch drawable ------------------------------------------------

#[test]
fn neither_branch_drawable_draws_nothing_and_does_not_panic() {
    let pages = layout(&alternate_content("x99", "", ""));

    assert_eq!(path_count(&pages), 0);
    assert!(!pages.is_empty(), "the document still lays out");
}

/// No `<mc:Fallback>` element at all — the `Neither` case, reached through a
/// different route than an empty one.
#[test]
fn an_unmeetable_choice_with_no_fallback_draws_nothing() {
    let pages = layout(
        r#"<mc:AlternateContent><mc:Choice Requires="x99"></mc:Choice></mc:AlternateContent>"#,
    );

    assert_eq!(path_count(&pages), 0);
    assert!(!pages.is_empty());
}
