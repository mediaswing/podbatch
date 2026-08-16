//! Writing a transcript out as something readable.
//!
//! A `.txt` is the honest minimum and the worst thing to actually read: it
//! opens at whatever size and weight the text editor feels like, which on a
//! Mac is small and light. So the transcript can also be written as a Word
//! document or a PDF, both carrying the size and weight with them, so the file
//! arrives readable rather than needing to be made readable every time.
//!
//! Both formats are written by hand here rather than pulled in as crates. A
//! `.docx` is four short XML files in a ZIP, and a PDF of flowed text is a
//! handful of objects and a cross-reference table; between them that is less
//! code than the dependency trees would have added, and it means the exact
//! font, size and weight are ours to set rather than something to persuade a
//! library into.
//!
//! The ZIP is written with stored (uncompressed) entries. Deflate would need a
//! compressor; stored needs only a CRC, and a transcript is tens of kilobytes,
//! so the saving would be invisible and the code would not be.

use std::path::Path;

/// What to write. `Text` is the original plain file, kept because it is the one
/// you can grep, diff and paste anywhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Format {
    Text,
    #[default]
    Docx,
    Pdf,
}

impl Format {
    pub fn all() -> [Format; 3] {
        [Format::Docx, Format::Pdf, Format::Text]
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Text => "Plain text (.txt)",
            Format::Docx => "Word (.docx)",
            Format::Pdf => "PDF (.pdf)",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Text => "txt",
            Format::Docx => "docx",
            Format::Pdf => "pdf",
        }
    }

    /// What this format is good and bad at, said plainly in the Settings pane.
    pub fn note(self) -> &'static str {
        match self {
            Format::Text => {
                "Opens anywhere, but at whatever size and weight your text editor uses — the \
                 size and boldness below are ignored."
            }
            Format::Docx => {
                "Carries the font, size and weight with it, and the text reflows when you make \
                 it bigger. The best of the three to read on screen."
            }
            Format::Pdf => {
                "Fixed layout, so it looks the same everywhere and prints predictably. Enlarging \
                 it means scrolling sideways, and it uses a typewriter font — the only one a PDF \
                 can rely on without embedding."
            }
        }
    }
}

/// How the text should look. The defaults are the point of this module.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Style {
    /// Points. 16 rather than the usual 11 or 12: this is meant to be read, not
    /// to fit on as few pages as possible.
    pub size: u32,
    pub bold: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self { size: 16, bold: true }
    }
}

/// The screen font named in the Word document.
///
/// Verdana rather than the bundled Ubuntu: a `.docx` names a font, it does not
/// carry one, so naming a face the reader almost certainly hasn't got means
/// their word processor quietly substitutes something arbitrary. Verdana ships
/// with both macOS and Windows, and was drawn for reading on screen — wide,
/// open counters, and unusually distinct letterforms.
const DOCX_FONT: &str = "Verdana";

/// Write `body` to `path` in the chosen format.
pub fn write(path: &Path, format: Format, style: Style, body: &str) -> Result<(), String> {
    let bytes = match format {
        Format::Text => body.as_bytes().to_vec(),
        Format::Docx => docx(body, style),
        Format::Pdf => pdf(body, style),
    };
    std::fs::write(path, bytes).map_err(|e| format!("could not write the transcript — {e}"))
}

// ---- Word -----------------------------------------------------------------

/// A minimal WordprocessingML package: content types, two relationship parts,
/// the style defaults and the body.
fn docx(body: &str, style: Style) -> Vec<u8> {
    let mut zip = Zip::new();
    zip.add("[Content_Types].xml", CONTENT_TYPES.as_bytes());
    zip.add("_rels/.rels", ROOT_RELS.as_bytes());
    zip.add("word/_rels/document.xml.rels", DOC_RELS.as_bytes());
    zip.add("word/styles.xml", styles_xml(style).as_bytes());
    zip.add("word/document.xml", document_xml(body).as_bytes());
    zip.finish()
}

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#;

const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

const DOC_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#;

/// The document defaults, which is where the size and weight actually live.
///
/// Word measures type in half-points, so 16pt is `w:sz w:val="32"`. Setting it
/// in `docDefaults` rather than on each run means the reader can restyle the
/// whole document from one place, which somebody who needs 20pt will want to.
fn styles_xml(style: Style) -> String {
    let half_points = style.size * 2;
    let bold = if style.bold { "<w:b/><w:bCs/>" } else { "" };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="{DOCX_FONT}" w:hAnsi="{DOCX_FONT}" w:cs="{DOCX_FONT}"/>{bold}<w:sz w:val="{half_points}"/><w:szCs w:val="{half_points}"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="160" w:line="276" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults></w:styles>"#
    )
}

fn document_xml(body: &str) -> String {
    let mut out = String::with_capacity(body.len() * 2);
    out.push_str(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
    );
    // One paragraph per line. Blank lines in the source become empty
    // paragraphs, which is what keeps the gap between one speaker and the next.
    for line in body.lines() {
        out.push_str("<w:p><w:r><w:t xml:space=\"preserve\">");
        escape_xml_into(line, &mut out);
        out.push_str("</w:t></w:r></w:p>");
    }
    out.push_str("<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/></w:sectPr></w:body></w:document>");
    out
}

fn escape_xml_into(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // Control characters are not permitted in XML at all, and a stray
            // one makes the whole document unopenable rather than slightly odd.
            c if (c as u32) < 0x20 && c != '\t' => out.push(' '),
            c => out.push(c),
        }
    }
}

// ---- PDF ------------------------------------------------------------------

/// A4 in points, and the margins around the text.
const PAGE_WIDTH: f64 = 595.0;
const PAGE_HEIGHT: f64 = 842.0;
const MARGIN_X: f64 = 50.0;
const MARGIN_Y: f64 = 60.0;

/// Every glyph in a Courier is exactly 600/1000 of an em.
///
/// This is the whole reason the PDF is set in Courier-Bold. A PDF can use the
/// standard fonts without embedding anything, but wrapping text means knowing
/// how wide it is, and Courier is the only one of them whose widths can be
/// known without shipping a metrics table for the face.
const COURIER_ADVANCE: f64 = 0.6;

fn pdf(body: &str, style: Style) -> Vec<u8> {
    let size = style.size as f64;
    let leading = (size * 1.35).round();
    let columns = ((PAGE_WIDTH - 2.0 * MARGIN_X) / (size * COURIER_ADVANCE)).floor() as usize;
    let rows = ((PAGE_HEIGHT - 2.0 * MARGIN_Y) / leading).floor() as usize;

    let wrapped: Vec<String> = body
        .lines()
        .flat_map(|line| wrap(line, columns.max(1)))
        .collect();

    let pages: Vec<&[String]> = wrapped.chunks(rows.max(1)).collect();
    // A document with no pages is not a valid PDF, and an episode with nothing
    // in it should still open and say so by being empty.
    let pages = if pages.is_empty() { vec![&[][..]] } else { pages };

    // Object 1 catalog, 2 page tree, 3 font, then a page and a content stream
    // for each page.
    let font = if style.bold { "Courier-Bold" } else { "Courier" };
    let mut objects: Vec<Vec<u8>> = Vec::new();

    let kids: Vec<String> = (0..pages.len())
        .map(|i| format!("{} 0 R", 4 + i * 2))
        .collect();

    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    objects.push(
        format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            pages.len()
        )
        .into_bytes(),
    );
    objects.push(
        format!(
            "<< /Type /Font /Subtype /Type1 /BaseFont /{font} /Encoding /WinAnsiEncoding >>"
        )
        .into_bytes(),
    );

    for (i, page) in pages.iter().enumerate() {
        let contents_id = 5 + i * 2;
        objects.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_WIDTH} {PAGE_HEIGHT}] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {contents_id} 0 R >>"
            )
            .into_bytes(),
        );

        let mut stream = String::new();
        stream.push_str(&format!(
            "BT\n/F1 {size} Tf\n{leading} TL\n{MARGIN_X} {} Td\n",
            PAGE_HEIGHT - MARGIN_Y - size
        ));
        for line in page.iter() {
            stream.push('(');
            escape_pdf_into(line, &mut stream);
            stream.push_str(") Tj\nT*\n");
        }
        stream.push_str("ET");

        let mut object = format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes();
        object.extend_from_slice(stream.as_bytes());
        object.extend_from_slice(b"\nendstream");
        objects.push(object);
    }

    assemble_pdf(&objects)
}

/// Lay the objects out and build the cross-reference table.
///
/// The xref offsets have to be the byte position of each object in the finished
/// file, so the body is built first and measured as it goes.
fn assemble_pdf(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    out.extend_from_slice(b"%PDF-1.4\n");
    // A comment of high bytes, which tells anything treating the file as text
    // that it is binary.
    out.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");

    let mut offsets = Vec::with_capacity(objects.len());
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
    }

    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    // Entry zero is the head of the free list and is always exactly this.
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    out
}

/// Escape a string for a PDF literal, and force it into WinAnsi.
///
/// The standard fonts are single-byte, so anything outside WinAnsi has no glyph
/// to be drawn with. The handful of characters a transcript actually picks up —
/// curly quotes, dashes, ellipsis — are mapped to the WinAnsi bytes that carry
/// them; anything else becomes a question mark, which is visibly a substitution
/// rather than silently the wrong letter.
fn escape_pdf_into(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            // WinAnsi positions, written as octal escapes so the file stays
            // seven-bit clean and cannot be mangled in transit.
            '\u{2018}' => out.push_str("\\221"),
            '\u{2019}' => out.push_str("\\222"),
            '\u{201C}' => out.push_str("\\223"),
            '\u{201D}' => out.push_str("\\224"),
            '\u{2013}' => out.push_str("\\226"),
            '\u{2014}' => out.push_str("\\227"),
            '\u{2026}' => out.push_str("\\205"),
            '\u{00A3}' => out.push_str("\\243"),
            '\u{00E9}' => out.push_str("\\351"),
            _ => out.push('?'),
        }
    }
}

/// Break a line to fit `columns`, splitting between words where possible.
///
/// A word longer than the line — a URL, usually — is cut rather than allowed to
/// run off the edge, because a fixed-layout page has no edge to run off into.
fn wrap(line: &str, columns: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }

    let mut out = Vec::new();
    let mut current = String::new();

    for word in line.split_whitespace() {
        let mut word = word;
        // Anything that cannot fit on a line of its own is cut to width and the
        // remainder carried on to the next.
        while word.chars().count() > columns {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            let cut: String = word.chars().take(columns).collect();
            out.push(cut);
            word = &word[word
                .char_indices()
                .nth(columns)
                .map(|(i, _)| i)
                .unwrap_or(word.len())..];
        }
        if word.is_empty() {
            continue;
        }

        let would_be = match current.is_empty() {
            true => word.chars().count(),
            false => current.chars().count() + 1 + word.chars().count(),
        };
        if would_be > columns {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }

    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

// ---- ZIP ------------------------------------------------------------------

/// A ZIP writer with stored entries and nothing else.
struct Zip {
    out: Vec<u8>,
    entries: Vec<Entry>,
}

struct Entry {
    name: String,
    crc: u32,
    length: usize,
    offset: usize,
}

impl Zip {
    fn new() -> Self {
        Self { out: Vec::new(), entries: Vec::new() }
    }

    fn add(&mut self, name: &str, data: &[u8]) {
        let offset = self.out.len();
        let crc = crc32(data);

        self.out.extend_from_slice(b"PK\x03\x04");
        self.out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.out.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.out.extend_from_slice(&0u16.to_le_bytes()); // stored
        self.out.extend_from_slice(&0u16.to_le_bytes()); // time
        // 1980-01-01. Zero is not a valid MS-DOS date, and some readers baulk.
        self.out.extend_from_slice(&0x0021u16.to_le_bytes());
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        self.out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // extra
        self.out.extend_from_slice(name.as_bytes());
        self.out.extend_from_slice(data);

        self.entries.push(Entry {
            name: name.to_string(),
            crc,
            length: data.len(),
            offset,
        });
    }

    fn finish(mut self) -> Vec<u8> {
        let directory_at = self.out.len();

        for entry in &self.entries {
            self.out.extend_from_slice(b"PK\x01\x02");
            self.out.extend_from_slice(&20u16.to_le_bytes()); // made by
            self.out.extend_from_slice(&20u16.to_le_bytes()); // needed
            self.out.extend_from_slice(&0u16.to_le_bytes()); // flags
            self.out.extend_from_slice(&0u16.to_le_bytes()); // stored
            self.out.extend_from_slice(&0u16.to_le_bytes()); // time
            self.out.extend_from_slice(&0x0021u16.to_le_bytes()); // date
            self.out.extend_from_slice(&entry.crc.to_le_bytes());
            self.out.extend_from_slice(&(entry.length as u32).to_le_bytes());
            self.out.extend_from_slice(&(entry.length as u32).to_le_bytes());
            self.out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
            self.out.extend_from_slice(&0u16.to_le_bytes()); // extra
            self.out.extend_from_slice(&0u16.to_le_bytes()); // comment
            self.out.extend_from_slice(&0u16.to_le_bytes()); // disk
            self.out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            self.out.extend_from_slice(&(entry.offset as u32).to_le_bytes());
            self.out.extend_from_slice(entry.name.as_bytes());
        }

        let directory_size = self.out.len() - directory_at;
        let count = self.entries.len() as u16;

        self.out.extend_from_slice(b"PK\x05\x06");
        self.out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        self.out.extend_from_slice(&0u16.to_le_bytes()); // disk with directory
        self.out.extend_from_slice(&count.to_le_bytes());
        self.out.extend_from_slice(&count.to_le_bytes());
        self.out.extend_from_slice(&(directory_size as u32).to_le_bytes());
        self.out.extend_from_slice(&(directory_at as u32).to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // comment
        self.out
    }
}

/// The usual CRC-32, computed a nibble at a time so no 256-entry table has to
/// be carried around for the five small files this writes.
fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 16] = [
        0x00000000, 0x1db71064, 0x3b6e20c8, 0x26d930ac, 0x76dc4190, 0x6b6b51f4, 0x4db26158,
        0x5005713c, 0xedb88320, 0xf00f9344, 0xd6d6a3e8, 0xcb61b38c, 0x9b64c2b0, 0x86d3d2d4,
        0xa00ae278, 0xbdbdf21c,
    ];
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        crc = (crc >> 4) ^ TABLE[(crc & 0x0F) as usize];
        crc = (crc >> 4) ^ TABLE[(crc & 0x0F) as usize];
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked against the known CRC-32 of "123456789".
    #[test]
    fn the_crc_is_the_real_one() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn every_format_has_a_distinct_extension_and_says_what_it_is() {
        let mut seen = Vec::new();
        for format in Format::all() {
            assert!(!format.label().is_empty());
            assert!(!format.note().is_empty());
            assert!(!seen.contains(&format.extension()), "{format:?} repeats an extension");
            seen.push(format.extension());
        }
        assert_eq!(seen.len(), 3);
    }

    /// The defaults are the whole point: 16pt and bold, without being asked.
    #[test]
    fn the_default_style_is_large_and_bold() {
        let style = Style::default();
        assert_eq!(style.size, 16);
        assert!(style.bold);
        assert_eq!(Format::default(), Format::Docx);
    }

    #[test]
    fn the_size_and_weight_reach_the_word_styles() {
        let xml = styles_xml(Style { size: 16, bold: true });
        // Word counts in half-points.
        assert!(xml.contains(r#"<w:sz w:val="32"/>"#), "{xml}");
        assert!(xml.contains("<w:b/>"), "{xml}");
        assert!(xml.contains(r#"w:ascii="Verdana""#), "{xml}");

        let plain = styles_xml(Style { size: 20, bold: false });
        assert!(plain.contains(r#"<w:sz w:val="40"/>"#), "{plain}");
        assert!(!plain.contains("<w:b/>"), "{plain}");
    }

    /// A stray `&` in an episode title must not make the document unopenable.
    #[test]
    fn xml_is_escaped() {
        let xml = document_xml("Tom & Jerry <hello>");
        assert!(xml.contains("Tom &amp; Jerry &lt;hello&gt;"), "{xml}");
    }

    #[test]
    fn a_docx_is_a_zip_with_the_parts_word_looks_for() {
        let bytes = docx("Speaker 1: Hello.", Style::default());
        assert_eq!(&bytes[0..4], b"PK\x03\x04", "not a zip");
        assert!(bytes.windows(4).any(|w| w == b"PK\x05\x06"), "no end of directory");

        let text = String::from_utf8_lossy(&bytes);
        for part in [
            "[Content_Types].xml",
            "_rels/.rels",
            "word/document.xml",
            "word/styles.xml",
            "word/_rels/document.xml.rels",
        ] {
            assert!(text.contains(part), "{part} missing from the package");
        }
        assert!(text.contains("Speaker 1: Hello."), "the text never made it in");
    }

    #[test]
    fn a_pdf_has_a_header_a_trailer_and_one_page_per_chunk() {
        let bytes = pdf("Speaker 1: Hello.", Style::default());
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("%PDF-1.4"), "no header");
        assert!(text.trim_end().ends_with("%%EOF"), "no trailer");
        assert!(text.contains("/BaseFont /Courier-Bold"), "wrong font: {text:.300}");
        assert!(text.contains("/F1 16 Tf"), "wrong size");
        assert!(text.contains("(Speaker 1: Hello.) Tj"), "the text never made it in");
        assert!(text.contains("startxref"), "no cross-reference table");
    }

    /// The offsets in the table have to be where the objects actually are, or
    /// the file opens as blank in a strict reader.
    /// Worked on the raw bytes, never on a `from_utf8_lossy` of them: the file
    /// opens with a comment of deliberately invalid UTF-8, so converting it to
    /// a string replaces those four bytes with three-byte replacement
    /// characters and every offset in the table stops matching. A reader
    /// counts bytes, so the test has to as well.
    #[test]
    fn the_pdf_cross_reference_offsets_point_at_their_objects() {
        let bytes = pdf("A line.", Style::default());

        let marker = b"startxref\n";
        let at = bytes
            .windows(marker.len())
            .rposition(|w| w == marker)
            .expect("a startxref");
        let tail = &bytes[at + marker.len()..];
        let digits: Vec<u8> = tail.iter().copied().take_while(|b| b.is_ascii_digit()).collect();
        let xref_at: usize = String::from_utf8(digits)
            .expect("ascii")
            .parse()
            .expect("an offset");

        assert!(
            bytes[xref_at..].starts_with(b"xref"),
            "startxref points at {xref_at}, which is not the table"
        );

        // Every offset in the table must land on "<n> 0 obj".
        let table = String::from_utf8_lossy(&bytes[xref_at..]).into_owned();
        let entries: Vec<usize> = table
            .lines()
            .filter(|l| l.ends_with(" n "))
            .filter_map(|l| l.split_whitespace().next()?.parse().ok())
            .collect();
        assert!(!entries.is_empty(), "no object offsets");
        for (i, offset) in entries.iter().enumerate() {
            let expected = format!("{} 0 obj", i + 1);
            assert!(
                bytes[*offset..].starts_with(expected.as_bytes()),
                "object {} is not at {offset}",
                i + 1
            );
        }
    }

    #[test]
    fn pdf_literals_are_escaped() {
        let mut out = String::new();
        escape_pdf_into(r"a (b) c \ d", &mut out);
        assert_eq!(out, r"a \(b\) c \\ d");
    }

    /// Whisper produces curly quotes and dashes; the standard fonts are
    /// single-byte, so they have to be mapped rather than passed through.
    #[test]
    fn pdf_maps_the_punctuation_a_transcript_actually_contains() {
        let mut out = String::new();
        escape_pdf_into("it\u{2019}s \u{2014} \u{201C}quoted\u{201D}\u{2026}", &mut out);
        assert_eq!(out, r"it\222s \227 \223quoted\224\205");

        // Something with no WinAnsi place is visibly substituted, not dropped.
        let mut out = String::new();
        escape_pdf_into("\u{4E2D}", &mut out);
        assert_eq!(out, "?");
    }

    #[test]
    fn wrapping_breaks_between_words_and_never_exceeds_the_width() {
        let lines = wrap("the quick brown fox jumps over the lazy dog", 12);
        for line in &lines {
            assert!(line.chars().count() <= 12, "{line:?} is too long");
        }
        assert_eq!(lines.concat().replace(' ', "").len(), 35);
    }

    #[test]
    fn a_word_too_long_for_the_line_is_cut_rather_than_overflowing() {
        let lines = wrap("see https://example.com/a/very/long/path/indeed here", 10);
        for line in &lines {
            assert!(line.chars().count() <= 10, "{line:?} is too long");
        }
        assert!(lines.len() > 2);
    }

    #[test]
    fn a_blank_line_survives_wrapping() {
        assert_eq!(wrap("", 10), vec![String::new()]);
    }

    /// Round the whole thing: each format writes a file that starts the way its
    /// format requires.
    #[test]
    fn each_format_writes_a_file_of_that_format() {
        let dir = std::env::temp_dir().join(format!("podbatch-doc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");

        for format in Format::all() {
            let path = dir.join(format!("t.{}", format.extension()));
            write(&path, format, Style::default(), "Speaker 1: Hello.\n\nSpeaker 2: Hi.")
                .expect("write");
            let bytes = std::fs::read(&path).expect("read");
            match format {
                Format::Text => assert!(bytes.starts_with(b"Speaker 1")),
                Format::Docx => assert!(bytes.starts_with(b"PK\x03\x04")),
                Format::Pdf => assert!(bytes.starts_with(b"%PDF")),
            }
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
