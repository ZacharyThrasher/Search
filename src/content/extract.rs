//! Text extraction. Runs only inside the extractor child processes
//! (`searchi --worker`), never in the indexer itself and never in the
//! elevated helper: these parsers read untrusted files.
//!
//! Formats: plain text and code (UTF-8, UTF-16, Windows-1252), HTML/XML,
//! Markdown (as text), PDF text, Office Open XML (docx, xlsx, pptx) and
//! OpenDocument (odt, ods, odp). Binary files are recognised by sniffing and
//! skipped.

use std::io::{Cursor, Read};
use std::path::Path;

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::chunk::Segment;

/// How a file ended up in the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Indexed = 0,
    Binary = 1,
    TooBig = 2,
    Empty = 3,
    Unsupported = 4,
    /// The parser gave up or failed; not retried until the file changes.
    Failed = 5,
    /// The extractor ran past its time limit; not retried until the file changes.
    Timeout = 6,
    /// Could not be read right now (locked, vanished); retried on the next pass.
    Unreadable = 7,
    /// Indexed, but only the first part of a very long text.
    Partial = 8,
}

impl Status {
    pub fn from_u64(v: u64) -> Status {
        match v {
            0 => Status::Indexed,
            1 => Status::Binary,
            2 => Status::TooBig,
            3 => Status::Empty,
            4 => Status::Unsupported,
            5 => Status::Failed,
            6 => Status::Timeout,
            7 => Status::Unreadable,
            8 => Status::Partial,
            _ => Status::Failed,
        }
    }

    pub fn has_text(self) -> bool {
        matches!(self, Status::Indexed | Status::Partial)
    }
}

/// Why a file produced no text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    Binary,
    Empty,
    TooBig,
    Unsupported,
    Encrypted,
    Failed(String),
    Unreadable(String),
}

impl Skip {
    pub fn status(&self) -> Status {
        match self {
            Skip::Binary => Status::Binary,
            Skip::Empty => Status::Empty,
            Skip::TooBig => Status::TooBig,
            Skip::Unsupported => Status::Unsupported,
            Skip::Encrypted | Skip::Failed(_) => Status::Failed,
            Skip::Unreadable(_) => Status::Unreadable,
        }
    }

    pub fn note(&self) -> String {
        match self {
            Skip::Failed(m) | Skip::Unreadable(m) => m.clone(),
            Skip::Encrypted => "encrypted".into(),
            other => format!("{other:?}").to_lowercase(),
        }
    }
}

/// Cap on any single decompressed part of an Office/ODF file, against zip bombs.
const MAX_PART_BYTES: u64 = 96 << 20;
/// Cap on the text collected from one XML part.
const MAX_XML_TEXT: usize = 24 << 20;

pub fn extract(path: &Path, max_bytes: u64) -> Result<Vec<Segment>, Skip> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(|e| Skip::Unreadable(e.to_string()))?;
    if !meta.is_file() {
        return Err(Skip::Unsupported);
    }
    if meta.len() == 0 {
        return Err(Skip::Empty);
    }
    if meta.len() > max_bytes {
        return Err(Skip::TooBig);
    }
    let bytes = std::fs::read(path).map_err(|e| Skip::Unreadable(e.to_string()))?;
    extract_bytes(&bytes, &ext)
}

pub fn extract_bytes(bytes: &[u8], ext: &str) -> Result<Vec<Segment>, Skip> {
    let segs = match ext {
        "pdf" => pdf(bytes)?,
        "docx" => docx(bytes)?,
        "xlsx" => xlsx(bytes)?,
        "pptx" => pptx(bytes)?,
        "odt" | "ods" | "odp" => odf(bytes)?,
        "html" | "htm" | "xml" => {
            let text = decode_text(bytes)?;
            vec![Segment { page: 0, text: strip_markup(&text) }]
        }
        _ => vec![Segment { page: 0, text: decode_text(bytes)? }],
    };
    let segs: Vec<Segment> = segs.into_iter().filter(|s| s.text.chars().any(char::is_alphanumeric)).collect();
    if segs.is_empty() { Err(Skip::Empty) } else { Ok(segs) }
}

// ----------------------------------------------------------------- plain text

/// Decode a text file: BOM, UTF-16 without BOM, UTF-8, else Windows-1252.
/// Binary data is refused.
pub fn decode_text(bytes: &[u8]) -> Result<String, Skip> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return utf8_or_legacy(rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Ok(encoding_rs::UTF_16LE.decode_without_bom_handling(rest).0.into_owned());
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Ok(encoding_rs::UTF_16BE.decode_without_bom_handling(rest).0.into_owned());
    }
    let sample = &bytes[..bytes.len().min(8192)];
    if sample.contains(&0) {
        // Mostly-ASCII UTF-16 has a NUL in every other byte.
        let (mut even, mut odd) = (0usize, 0usize);
        for (i, &b) in sample.iter().enumerate() {
            if b == 0 {
                if i % 2 == 0 { even += 1 } else { odd += 1 }
            }
        }
        let half = sample.len() / 2;
        if odd * 10 >= half * 3 && even * 20 <= half {
            return Ok(encoding_rs::UTF_16LE.decode_without_bom_handling(bytes).0.into_owned());
        }
        if even * 10 >= half * 3 && odd * 20 <= half {
            return Ok(encoding_rs::UTF_16BE.decode_without_bom_handling(bytes).0.into_owned());
        }
        return Err(Skip::Binary);
    }
    let controls = sample
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0C | 0x1B))
        .count();
    if controls * 100 > sample.len().max(1) {
        return Err(Skip::Binary);
    }
    utf8_or_legacy(bytes)
}

fn utf8_or_legacy(bytes: &[u8]) -> Result<String, Skip> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(_) => Ok(encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes).0.into_owned()),
    }
}

/// Position of `needle` (lower case) in `hay`, ignoring ASCII case.
fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let first = needle[0];
    let mut from = 0;
    while let Some(p) = memchr::memchr2(first, first.to_ascii_uppercase(), &hay[from..]) {
        let at = from + p;
        if hay.len() - at >= needle.len() && hay[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// Drop tags (and the contents of script/style) from HTML or XML and decode
/// the common entities. Not a parser: good enough to make words searchable.
pub fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' => {
                let rest = &s[i..];
                let lower: String = rest.chars().take(9).collect::<String>().to_ascii_lowercase();
                let skip_to = if lower.starts_with("<script") {
                    Some("</script")
                } else if lower.starts_with("<style") {
                    Some("</style")
                } else if rest.starts_with("<!--") {
                    Some("-->")
                } else {
                    None
                };
                if let Some(end) = skip_to {
                    i += find_ci(rest.as_bytes(), end.as_bytes()).map_or(rest.len(), |p| p + end.len());
                    // Finish the closing tag.
                    while i < b.len() && b[i] != b'>' {
                        i += 1;
                    }
                    i += 1;
                } else {
                    // A tag: separate neighbouring words.
                    while i < b.len() && b[i] != b'>' {
                        i += 1;
                    }
                    i += 1;
                }
                out.push(' ');
            }
            b'&' => {
                let rest = &s[i..];
                // Look for the `;` among the next few bytes (never slice mid-character).
                if let Some(semi) = rest.as_bytes()[..rest.len().min(12)].iter().position(|&b| b == b';') {
                    let name = &rest[1..semi];
                    let ch = match name {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        "nbsp" => Some(' '),
                        _ => name
                            .strip_prefix('#')
                            .and_then(|n| match n.strip_prefix(['x', 'X']) {
                                Some(h) => u32::from_str_radix(h, 16).ok(),
                                None => n.parse().ok(),
                            })
                            .and_then(char::from_u32),
                    };
                    if let Some(c) = ch {
                        out.push(c);
                        i += semi + 1;
                        continue;
                    }
                }
                out.push('&');
                i += 1;
            }
            _ => {
                let ch = s[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

// ------------------------------------------------------------------------ PDF

fn pdf(bytes: &[u8]) -> Result<Vec<Segment>, Skip> {
    if !bytes.starts_with(b"%PDF") && !bytes[..bytes.len().min(1024)].windows(4).any(|w| w == b"%PDF") {
        return Err(Skip::Unsupported);
    }
    let pages = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem_by_pages(bytes))
        .map_err(|_| Skip::Failed("pdf parser panicked".into()))?
        .map_err(|e| {
            let msg = e.to_string();
            if msg.to_lowercase().contains("encrypt") || msg.to_lowercase().contains("password") {
                Skip::Encrypted
            } else {
                Skip::Failed(msg)
            }
        })?;
    Ok(pages
        .into_iter()
        .enumerate()
        .map(|(i, text)| Segment { page: i as u32 + 1, text })
        .collect())
}

// ------------------------------------------------------------------ zip + XML

fn open_zip(bytes: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>, Skip> {
    zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| Skip::Failed(format!("not a valid zip: {e}")))
}

fn read_part(z: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<Vec<u8>> {
    let f = z.by_name(name).ok()?;
    if f.size() > MAX_PART_BYTES {
        return None;
    }
    let mut out = Vec::new();
    f.take(MAX_PART_BYTES).read_to_end(&mut out).ok()?;
    Some(out)
}

/// Names of the parts called `<prefix>N<suffix>`, in numeric order of N.
fn numbered_parts(z: &zip::ZipArchive<Cursor<&[u8]>>, prefix: &str, suffix: &str) -> Vec<(u32, String)> {
    let mut v: Vec<(u32, String)> = z
        .file_names()
        .filter_map(|n| n.ok())
        .filter_map(|n| {
            let mid = n.strip_prefix(prefix)?.strip_suffix(suffix)?;
            Some((mid.parse().ok()?, n.to_string()))
        })
        .collect();
    v.sort();
    v
}

/// Collect text from an XML part. `text_tags`: local names whose contents are
/// text (empty: every text node counts). `break_tags`: closing one ends a
/// paragraph. `space_tags`: empty elements that stand for a space.
fn xml_text(xml: &[u8], text_tags: &[&str], break_tags: &[&str], space_tags: &[&str]) -> String {
    let xml = String::from_utf8_lossy(xml);
    let mut r = Reader::from_str(&xml);
    r.config_mut().check_end_names = false;
    let mut out = String::new();
    let mut in_text = 0u32;
    let all = text_tags.is_empty();
    loop {
        if out.len() > MAX_XML_TEXT {
            break;
        }
        match r.read_event() {
            Ok(Event::Start(e)) => {
                if text_tags.contains(&e.local_name().into_inner()) {
                    in_text += 1;
                }
            }
            Ok(Event::End(e)) => {
                let n = e.local_name().into_inner();
                if text_tags.contains(&n) {
                    in_text = in_text.saturating_sub(1);
                }
                if break_tags.contains(&n) {
                    out.push('\n');
                }
            }
            Ok(Event::Empty(e)) => {
                let n = e.local_name().into_inner();
                if space_tags.contains(&n) {
                    out.push(' ');
                }
                if break_tags.contains(&n) {
                    out.push('\n');
                }
            }
            Ok(Event::Text(t)) => {
                if all || in_text > 0 {
                    out.push_str(&t.xml10_content());
                }
            }
            Ok(Event::CData(t)) => {
                if all || in_text > 0 {
                    out.push_str(&t);
                }
            }
            Ok(Event::GeneralRef(e)) => {
                if all || in_text > 0 {
                    match e.resolve_char_ref() {
                        Ok(Some(c)) => out.push(c),
                        _ => match &*e {
                            "amp" => out.push('&'),
                            "lt" => out.push('<'),
                            "gt" => out.push('>'),
                            "quot" => out.push('"'),
                            "apos" => out.push('\''),
                            _ => {}
                        },
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            // Damaged XML: keep what was read so far.
            Err(_) => break,
        }
    }
    out
}

fn docx(bytes: &[u8]) -> Result<Vec<Segment>, Skip> {
    let mut z = open_zip(bytes)?;
    if z.by_name("EncryptedPackage").is_ok() {
        return Err(Skip::Encrypted);
    }
    let main = read_part(&mut z, "word/document.xml").ok_or(Skip::Unsupported)?;
    let mut text = xml_text(&main, &["t"], &["p", "br"], &["tab"]);
    for (_, name) in numbered_parts(&z, "word/header", ".xml")
        .into_iter()
        .chain(numbered_parts(&z, "word/footer", ".xml"))
        .chain(numbered_parts(&z, "word/footnotes", ".xml"))
    {
        if let Some(part) = read_part(&mut z, &name) {
            text.push('\n');
            text.push_str(&xml_text(&part, &["t"], &["p"], &["tab"]));
        }
    }
    Ok(vec![Segment { page: 0, text }])
}

fn xlsx(bytes: &[u8]) -> Result<Vec<Segment>, Skip> {
    let mut z = open_zip(bytes)?;
    if z.by_name("EncryptedPackage").is_ok() {
        return Err(Skip::Encrypted);
    }
    let mut text = String::new();
    if let Some(p) = read_part(&mut z, "xl/sharedStrings.xml") {
        text.push_str(&xml_text(&p, &["t"], &["si"], &[]));
    }
    for (_, name) in numbered_parts(&z, "xl/worksheets/sheet", ".xml") {
        if let Some(p) = read_part(&mut z, &name) {
            // Cells that hold their text inline (<is><t>).
            text.push('\n');
            text.push_str(&xml_text(&p, &["t"], &["is"], &[]));
        }
    }
    Ok(vec![Segment { page: 0, text }])
}

fn pptx(bytes: &[u8]) -> Result<Vec<Segment>, Skip> {
    let mut z = open_zip(bytes)?;
    if z.by_name("EncryptedPackage").is_ok() {
        return Err(Skip::Encrypted);
    }
    let slides = numbered_parts(&z, "ppt/slides/slide", ".xml");
    if slides.is_empty() {
        return Err(Skip::Unsupported);
    }
    let mut out = Vec::new();
    for (n, name) in slides {
        let mut text = read_part(&mut z, &name)
            .map(|p| xml_text(&p, &["t"], &["p"], &[]))
            .unwrap_or_default();
        if let Some(notes) = read_part(&mut z, &format!("ppt/notesSlides/notesSlide{n}.xml")) {
            text.push('\n');
            text.push_str(&xml_text(&notes, &["t"], &["p"], &[]));
        }
        out.push(Segment { page: n, text });
    }
    Ok(out)
}

fn odf(bytes: &[u8]) -> Result<Vec<Segment>, Skip> {
    let mut z = open_zip(bytes)?;
    let part = read_part(&mut z, "content.xml").ok_or(Skip::Unsupported)?;
    Ok(vec![Segment { page: 0, text: xml_text(&part, &[], &["p", "h"], &["s", "tab"]) }])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    pub fn zip_of(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in parts {
                w.start_file(*name, opts).unwrap();
                w.write_all(body.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    fn text_of(segs: &[Segment]) -> String {
        segs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("|")
    }

    #[test]
    fn utf8_utf16_and_legacy_text_decode() {
        assert_eq!(decode_text("héllo wörld".as_bytes()).unwrap(), "héllo wörld");
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice("bom text".as_bytes());
        assert_eq!(decode_text(&with_bom).unwrap(), "bom text");
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("wide text".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(decode_text(&utf16).unwrap(), "wide text");
        let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain("big end".encode_utf16().flat_map(|u| u.to_be_bytes())).collect();
        assert_eq!(decode_text(&be).unwrap(), "big end");
        // No BOM (PowerShell's older default).
        let bare: Vec<u8> = "plain ascii words in utf16".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(decode_text(&bare).unwrap(), "plain ascii words in utf16");
        // Windows-1252: 0xE9 is é, not valid UTF-8.
        assert_eq!(decode_text(b"caf\xe9 au lait").unwrap(), "café au lait");
    }

    #[test]
    fn binary_files_are_recognised() {
        assert_eq!(decode_text(&[0x4D, 0x5A, 0x90, 0x00, 0x03, 0x00, 0x00, 0x00, 0x04]), Err(Skip::Binary));
        let noise: Vec<u8> = (0..4000u32).map(|i| (i * 7 % 31) as u8).collect();
        assert_eq!(decode_text(&noise), Err(Skip::Binary));
        assert_eq!(extract_bytes(b"", "txt"), Err(Skip::Empty));
        assert_eq!(extract_bytes(b"   \n\n  ", "txt"), Err(Skip::Empty));
    }

    #[test]
    fn html_is_reduced_to_its_words() {
        let t = strip_markup(
            "<html><head><style>p{color:red}</style><script>var x = 'hidden';</script></head>\
             <body><p>Fish &amp; chips</p><!-- note --><b>caf&#233;</b>&nbsp;&lt;ok&gt;</body></html>",
        );
        let words: Vec<&str> = t.split_whitespace().collect();
        assert_eq!(words, ["Fish", "&", "chips", "café", "<ok>"]);
        assert!(!t.contains("hidden") && !t.contains("color"));
    }

    #[test]
    fn markup_with_multibyte_text_after_an_ampersand_does_not_panic() {
        // The byte 12 positions after '&' falls inside a multi-byte character.
        for s in ["&ééééééé; x", "a & b — c é &#x1F600; &#xZZ; &;", "<a>&</a>&é", "<scr\u{ef}pt>x</script>", "<!-- é"] {
            let _ = strip_markup(s);
        }
        assert_eq!(strip_markup("fish &amp; chips").trim(), "fish & chips");
        assert!(strip_markup("&ééééééé; x").contains('x'));
    }

    #[test]
    fn docx_text_comes_out_in_paragraphs() {
        let doc = r#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body>
            <w:p><w:r><w:t>Quarterly</w:t></w:r><w:r><w:t xml:space="preserve"> report</w:t></w:r></w:p>
            <w:p><w:r><w:t>Fish &amp; chips</w:t></w:r><w:r><w:tab/></w:r><w:r><w:t>end</w:t></w:r></w:p>
            <w:p><w:r><w:instrText>HYPERLINK "http://nope"</w:instrText></w:r></w:p></w:body></w:document>"#;
        let bytes = zip_of(&[("word/document.xml", doc), ("word/header1.xml", "<w:hdr xmlns:w=\"x\"><w:p><w:t>Header words</w:t></w:p></w:hdr>")]);
        let segs = extract_bytes(&bytes, "docx").unwrap();
        let t = text_of(&segs);
        assert!(t.contains("Quarterly report\n"), "{t:?}");
        assert!(t.contains("Fish & chips end"), "{t:?}");
        assert!(t.contains("Header words"));
        assert!(!t.contains("HYPERLINK"));
    }

    #[test]
    fn xlsx_collects_shared_and_inline_strings() {
        let shared = r#"<sst xmlns="x"><si><t>Revenue</t></si><si><r><t>Net </t></r><r><t>profit</t></r></si></sst>"#;
        let sheet = r#"<worksheet xmlns="x"><sheetData><row><c t="s"><v>0</v></c><c t="inlineStr"><is><t>Inline note</t></is></c><c><v>42</v></c></row></sheetData></worksheet>"#;
        let bytes = zip_of(&[("xl/sharedStrings.xml", shared), ("xl/worksheets/sheet1.xml", sheet)]);
        let t = text_of(&extract_bytes(&bytes, "xlsx").unwrap());
        assert!(t.contains("Revenue") && t.contains("Net profit") && t.contains("Inline note"), "{t:?}");
        assert!(!t.contains("42"));
    }

    #[test]
    fn pptx_slides_keep_their_numbers() {
        let slide = |s: &str| format!(r#"<p:sld xmlns:p="x" xmlns:a="y"><a:p><a:r><a:t>{s}</a:t></a:r></a:p></p:sld>"#);
        let (s1, s2, s10, n2) = (slide("Opening"), slide("Middle"), slide("Closing"), slide("speaker note"));
        let bytes = zip_of(&[
            ("ppt/slides/slide10.xml", &s10),
            ("ppt/slides/slide2.xml", &s2),
            ("ppt/slides/slide1.xml", &s1),
            ("ppt/notesSlides/notesSlide2.xml", &n2),
        ]);
        let segs = extract_bytes(&bytes, "pptx").unwrap();
        assert_eq!(segs.iter().map(|s| s.page).collect::<Vec<_>>(), [1, 2, 10]);
        assert!(segs[1].text.contains("Middle") && segs[1].text.contains("speaker note"));
    }

    #[test]
    fn odf_text_comes_out() {
        let content = r#"<office:document-content xmlns:office="o" xmlns:text="t"><office:body><text:p>First<text:s/>para</text:p><text:h>Heading</text:h></office:body></office:document-content>"#;
        let bytes = zip_of(&[("content.xml", content)]);
        let t = text_of(&extract_bytes(&bytes, "odt").unwrap());
        assert!(t.contains("First para") && t.contains("Heading"), "{t:?}");
    }

    #[test]
    fn broken_containers_fail_cleanly() {
        assert!(matches!(extract_bytes(b"this is not a zip file at all", "docx"), Err(Skip::Failed(_))));
        assert!(matches!(extract_bytes(&zip_of(&[("other.txt", "x")]), "docx"), Err(Skip::Unsupported)));
        assert!(matches!(extract_bytes(b"definitely not a pdf", "pdf"), Err(Skip::Unsupported)));
        // Truncated pdf header only.
        assert!(extract_bytes(b"%PDF-1.4\n1 0 obj\n<<", "pdf").is_err());
        // Damaged XML keeps the text before the damage.
        let bytes = zip_of(&[("word/document.xml", "<w:document xmlns:w=\"x\"><w:p><w:t>kept words</w:t></w:p><w:p><w:t>cut")]);
        assert!(text_of(&extract_bytes(&bytes, "docx").unwrap()).contains("kept words"));
    }

    #[test]
    fn extract_reads_files_and_applies_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("notes.txt");
        std::fs::write(&p, "alpha beta gamma").unwrap();
        assert_eq!(text_of(&extract(&p, 1 << 20).unwrap()), "alpha beta gamma");
        assert_eq!(extract(&p, 5), Err(Skip::TooBig));
        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(extract(&empty, 100), Err(Skip::Empty));
        assert!(matches!(extract(&dir.path().join("missing.txt"), 100), Err(Skip::Unreadable(_))));
        assert_eq!(extract(dir.path(), 100), Err(Skip::Unsupported));
    }
}
