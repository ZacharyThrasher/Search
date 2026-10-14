//! `searchpop --preview out.bmp`: paint a sample result list into a bitmap
//! file and exit. For looking at layout changes without typing into the real
//! window; also handy for screenshots in a review.

use std::ops::Range;
use std::ptr::null_mut;

use windows_sys::Win32::Graphics::Gdi::*;

use crate::content::ContentRow;
use crate::{App, Mode, Row};

fn hl(text: &str, words: &[&str]) -> Vec<Range<usize>> {
    words.iter().filter_map(|w| text.find(w).map(|i| i..i + w.len())).collect()
}

fn hit(path: &str, snippet: &str, words: &[&str], page: u32) -> ContentRow {
    ContentRow { path: path.into(), snippet: snippet.into(), highlights: hl(snippet, words), page }
}

impl App {
    pub fn preview(&mut self, path: &str) {
        self.query = "quarterly rep".into();
        self.rows = [
            (r"C:\Users\Zach\Documents\Quarterly Report Q3.docx", false),
            (r"C:\Users\Zach\Documents\reports", true),
            (r"C:\Work\notes\quarterly-planning.md", false),
        ]
        .into_iter()
        .map(|(path, is_dir)| Row { path: path.into(), is_dir })
        .collect();
        self.total = self.rows.len() as u64;
        self.mode = Mode::Results;
        self.list.reset(self.rows.len());
        self.content = vec![
            hit(
                r"C:\Users\Zach\Documents\Board deck.pptx",
                "\u{2026}the quarterly report shows revenue growth across all regions, driven by the new subscription tier\u{2026}",
                &["quarterly", "report"],
                4,
            ),
            hit(
                r"C:\Work\finance\FY26 review.pdf",
                "Appendix B repeats the quarterly reporting calendar for every business unit and lists the owners",
                &["quarterly", "reporting"],
                12,
            ),
            hit(
                r"C:\Users\Zach\src\finance\report.rs",
                "fn quarterly_report(rows: &[Row]) -> Report { let mut total = 0; for r in rows { total += r.amount }",
                &["quarterly", "report"],
                0,
            ),
            hit(r"C:\Work\misc\old.txt", "A fourth hit, below the fold", &[], 0),
        ];
        self.content_list.reset(self.content.len());
        self.content_indexing = true;
        self.layout_and_resize(false);
        let lay = self.layout();
        // SAFETY: GDI objects are created and released here; the bitmap is read through GetDIBits.
        unsafe {
            let screen = GetDC(null_mut());
            let dc = CreateCompatibleDC(screen);
            let bmp = CreateCompatibleBitmap(screen, lay.w, lay.h);
            let old = SelectObject(dc, bmp);
            self.paint(dc, &lay);
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = lay.w;
            bi.bmiHeader.biHeight = lay.h; // bottom-up, as .bmp wants
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 24;
            let row = ((lay.w * 3 + 3) / 4 * 4) as usize;
            let mut pixels = vec![0u8; row * lay.h as usize];
            GetDIBits(dc, bmp, 0, lay.h as u32, pixels.as_mut_ptr().cast(), &mut bi, DIB_RGB_COLORS);
            SelectObject(dc, old);
            DeleteObject(bmp);
            DeleteDC(dc);
            ReleaseDC(null_mut(), screen);
            let mut file = Vec::new();
            file.extend_from_slice(b"BM");
            file.extend_from_slice(&((54 + pixels.len()) as u32).to_le_bytes());
            file.extend_from_slice(&[0, 0, 0, 0]);
            file.extend_from_slice(&54u32.to_le_bytes());
            file.extend_from_slice(std::slice::from_raw_parts(&bi.bmiHeader as *const _ as *const u8, 40));
            file.extend_from_slice(&pixels);
            let _ = std::fs::write(path, file);
        }
    }
}
