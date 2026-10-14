//! Drawing: everything except the text box is painted here with GDI into a
//! memory bitmap, then copied to the window in one go.
//!
//! In the dark theme the window uses the system acrylic backdrop, which shows
//! through wherever the client area is pure black, so the background is black
//! and nothing else is drawn in near-black. GDI cannot draw dark text on that
//! glass (DWM treats dark pixels as transparent), so the light theme paints a
//! solid background instead.

use std::ptr::null_mut;

use search::popup::{highlight_ranges, split_path};
use windows_sys::Win32::Foundation::{COLORREF, RECT};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{DI_NORMAL, DrawIconEx};

use crate::{App, Focus, Layout, Mode};

pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Window background. Black means "glass": the acrylic backdrop shows
    /// through. Only the dark theme uses it (see `App::apply_theme`).
    pub bg: COLORREF,
    pub text: COLORREF,
    pub muted: COLORREF,
    pub select: COLORREF,
    pub accent: COLORREF,
    pub divider: COLORREF,
    pub border: COLORREF,
}

impl Palette {
    pub fn new(dark: bool) -> Palette {
        if dark {
            Palette {
                dark,
                bg: 0,
                text: rgb(255, 255, 255),
                muted: rgb(172, 172, 172),
                select: rgb(66, 66, 66),
                accent: rgb(96, 205, 255),
                divider: rgb(74, 74, 74),
                border: rgb(80, 80, 80),
            }
        } else {
            Palette {
                dark,
                bg: rgb(243, 243, 243),
                text: rgb(27, 27, 27),
                muted: rgb(96, 96, 96),
                select: rgb(218, 218, 218),
                accent: rgb(0, 95, 184),
                divider: rgb(206, 206, 206),
                border: rgb(190, 190, 190),
            }
        }
    }
}

pub struct Fonts {
    pub input: HFONT,
    pub name: HFONT,
    pub name_bold: HFONT,
    pub small: HFONT,
    pub small_bold: HFONT,
    pub glyph: HFONT,
}

fn make_font(face: &str, px: i32, weight: i32) -> HFONT {
    let w: Vec<u16> = face.encode_utf16().chain([0]).collect();
    // Grayscale anti-aliasing: ClearType fringes badly over the backdrop.
    // SAFETY: valid NUL-terminated face name.
    unsafe {
        CreateFontW(
            -px,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            ANTIALIASED_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            w.as_ptr(),
        )
    }
}

impl Fonts {
    pub fn new(dpi: u32) -> Fonts {
        let px = |v: i32| (v * dpi as i32 + 48) / 96;
        Fonts {
            input: make_font("Segoe UI", px(20), 400),
            name: make_font("Segoe UI", px(15), 400),
            name_bold: make_font("Segoe UI", px(15), 600),
            small: make_font("Segoe UI", px(12), 400),
            small_bold: make_font("Segoe UI", px(12), 600),
            glyph: make_font("Segoe Fluent Icons", px(18), 400),
        }
    }

    pub fn destroy(&self) {
        for f in [self.input, self.name, self.name_bold, self.small, self.small_bold, self.glyph] {
            // SAFETY: fonts created by make_font and not selected into a live DC.
            unsafe { DeleteObject(f) };
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn text_w(dc: HDC, s: &[u16]) -> i32 {
    if s.is_empty() {
        return 0;
    }
    let mut sz = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
    // SAFETY: valid DC and string slice.
    unsafe { GetTextExtentPoint32W(dc, s.as_ptr(), s.len() as i32, &mut sz) };
    sz.cx
}

/// Draw `s` in `r`, one line, ellipsis when it does not fit.
fn draw(dc: HDC, s: &str, r: &mut RECT, flags: u32) {
    if s.is_empty() {
        return; // DrawTextW faults on the dangling pointer of an empty buffer
    }
    let w = wide(s);
    // SAFETY: valid DC, buffer and rect.
    unsafe { DrawTextW(dc, w.as_ptr(), w.len() as i32, r, flags | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER) };
}

impl App {
    /// Paint the whole client area into `target`.
    pub fn paint(&self, target: HDC, lay: &Layout) {
        // SAFETY: GDI calls on DCs/bitmaps created and destroyed in this function.
        unsafe {
            let dc = CreateCompatibleDC(target);
            let bmp = CreateCompatibleBitmap(target, lay.w, lay.h);
            let old_bmp = SelectObject(dc, bmp);
            SetBkMode(dc, TRANSPARENT as i32);

            // In the dark theme the backdrop shows through black.
            let black = CreateSolidBrush(self.pal.bg);
            let all = RECT { left: 0, top: 0, right: lay.w, bottom: lay.h };
            FillRect(dc, &all, black);
            DeleteObject(black);

            self.paint_input_bar(dc, lay);
            if lay.list_h > 0 {
                self.paint_divider(dc, lay);
                match &self.mode {
                    Mode::Results => self.paint_rows(dc, lay),
                    Mode::Message(m) if lay.names_h > 0 => self.paint_message(dc, lay, m),
                    _ => {}
                }
                if lay.content_h > 0 {
                    self.paint_content(dc, lay);
                }
                self.paint_footer(dc, lay);
            }

            BitBlt(target, 0, 0, lay.w, lay.h, dc, 0, 0, SRCCOPY);
            SelectObject(dc, old_bmp);
            DeleteObject(bmp);
            DeleteDC(dc);
        }
    }

    unsafe fn paint_input_bar(&self, dc: HDC, lay: &Layout) {
        unsafe {
            let old = SelectObject(dc, self.fonts.glyph);
            SetTextColor(dc, self.pal.muted);
            let mut r = RECT {
                left: lay.pad,
                top: 0,
                right: lay.pad + lay.s(24),
                bottom: lay.input_h,
            };
            draw(dc, "\u{E721}", &mut r, DT_LEFT);
            SelectObject(dc, old);
        }
    }

    unsafe fn paint_divider(&self, dc: HDC, lay: &Layout) {
        unsafe {
            let b = CreateSolidBrush(self.pal.divider);
            let r = RECT {
                left: lay.s(8),
                top: lay.input_h,
                right: lay.w - lay.s(8),
                bottom: lay.input_h + 1,
            };
            FillRect(dc, &r, b);
            DeleteObject(b);
        }
    }

    unsafe fn paint_message(&self, dc: HDC, lay: &Layout, msg: &str) {
        unsafe {
            let old = SelectObject(dc, self.fonts.name);
            SetTextColor(dc, self.pal.muted);
            let mut r = RECT {
                left: lay.pad + lay.s(24),
                top: lay.list_y,
                right: lay.w - lay.pad,
                bottom: lay.list_y + lay.row_h,
            };
            draw(dc, msg, &mut r, DT_LEFT | DT_END_ELLIPSIS);
            SelectObject(dc, old);
        }
    }

    unsafe fn paint_rows(&self, dc: HDC, lay: &Layout) {
        unsafe {
            let icon = lay.s(32);
            for (slot, i) in self.list.shown().enumerate() {
                let row = &self.rows[i];
                let top = lay.list_y + slot as i32 * lay.row_h;
                let selected = self.focus == Focus::Names && i == self.list.selected;
                if selected {
                    let b = CreateSolidBrush(self.pal.select);
                    let rg = CreateRoundRectRgn(
                        lay.s(8),
                        top + lay.s(2),
                        lay.w - lay.s(8) + 1,
                        top + lay.row_h - lay.s(2) + 1,
                        lay.s(8),
                        lay.s(8),
                    );
                    FillRgn(dc, rg, b);
                    DeleteObject(rg);
                    DeleteObject(b);
                    // Accent bar.
                    let ab = CreateSolidBrush(self.pal.accent);
                    let bar = CreateRoundRectRgn(
                        lay.s(8),
                        top + lay.row_h / 2 - lay.s(9),
                        lay.s(8) + lay.s(3) + 1,
                        top + lay.row_h / 2 + lay.s(9) + 1,
                        lay.s(3),
                        lay.s(3),
                    );
                    FillRgn(dc, bar, ab);
                    DeleteObject(bar);
                    DeleteObject(ab);
                }

                let ic = self.icons.get(&row.path, row.is_dir);
                if !ic.is_null() {
                    DrawIconEx(dc, lay.pad + lay.s(4), top + (lay.row_h - icon) / 2, ic, icon, icon, 0, null_mut(), DI_NORMAL);
                }

                let tx = lay.pad + lay.s(4) + icon + lay.s(12);
                let max_w = lay.w - lay.pad - tx;
                self.paint_name(dc, row, tx, top + lay.s(7), max_w, lay);

                let old = SelectObject(dc, self.fonts.small);
                SetTextColor(dc, self.pal.muted);
                let mut r = RECT {
                    left: tx,
                    top: top + lay.s(28),
                    right: tx + max_w,
                    bottom: top + lay.s(28) + lay.s(18),
                };
                let (_, folder) = split_path(&row.path);
                let shown = if folder.is_empty() { row.path.as_str() } else { folder };
                draw(dc, shown, &mut r, DT_LEFT | DT_PATH_ELLIPSIS);
                SelectObject(dc, old);
            }

            // Scroll indicator.
            if self.list.len > self.list.visible {
                let track_h = lay.row_h * self.list.visible as i32 - lay.s(8);
                let thumb_h = (track_h * self.list.visible as i32 / self.list.len as i32).max(lay.s(16));
                let span = track_h - thumb_h;
                let denom = (self.list.len - self.list.visible).max(1) as i32;
                let y = lay.list_y + lay.s(4) + span * self.list.top as i32 / denom;
                let b = CreateSolidBrush(self.pal.divider);
                let rg = CreateRoundRectRgn(lay.w - lay.s(7), y, lay.w - lay.s(4), y + thumb_h + 1, lay.s(3), lay.s(3));
                FillRgn(dc, rg, b);
                DeleteObject(rg);
                DeleteObject(b);
            }
        }
    }

    /// The "In files" section: a caption, then rows with a snippet each.
    unsafe fn paint_content(&self, dc: HDC, lay: &Layout) {
        unsafe {
            // Caption.
            let old = SelectObject(dc, self.fonts.small_bold);
            SetTextColor(dc, self.pal.muted);
            let mut r = RECT {
                left: lay.pad,
                top: lay.content_y,
                right: lay.w - lay.pad,
                bottom: lay.content_y + lay.s(24),
            };
            draw(dc, "In files", &mut r, DT_LEFT);
            SelectObject(dc, old);

            let icon = lay.s(32);
            for (slot, i) in self.content_list.shown().enumerate() {
                let row = &self.content[i];
                let top = lay.content_rows_y + slot as i32 * lay.row_h;
                if self.focus == Focus::Content && i == self.content_list.selected {
                    let b = CreateSolidBrush(self.pal.select);
                    let rg = CreateRoundRectRgn(
                        lay.s(8),
                        top + lay.s(2),
                        lay.w - lay.s(8) + 1,
                        top + lay.row_h - lay.s(2) + 1,
                        lay.s(8),
                        lay.s(8),
                    );
                    FillRgn(dc, rg, b);
                    DeleteObject(rg);
                    DeleteObject(b);
                    let ab = CreateSolidBrush(self.pal.accent);
                    let bar = CreateRoundRectRgn(
                        lay.s(8),
                        top + lay.row_h / 2 - lay.s(9),
                        lay.s(8) + lay.s(3) + 1,
                        top + lay.row_h / 2 + lay.s(9) + 1,
                        lay.s(3),
                        lay.s(3),
                    );
                    FillRgn(dc, bar, ab);
                    DeleteObject(bar);
                    DeleteObject(ab);
                }
                let ic = self.icons.get(&row.path, false);
                if !ic.is_null() {
                    DrawIconEx(dc, lay.pad + lay.s(4), top + (lay.row_h - icon) / 2, ic, icon, icon, 0, null_mut(), DI_NORMAL);
                }
                let tx = lay.pad + lay.s(4) + icon + lay.s(12);
                let right = lay.w - lay.pad;
                let (name, folder) = split_path(&row.path);

                // Line 1: the file name, then its folder (and page) in small grey.
                let of = SelectObject(dc, self.fonts.name);
                SetTextColor(dc, self.pal.text);
                let nw = text_w(dc, &wide(name)).min(right - tx);
                let mut r = RECT { left: tx, top: top + lay.s(7), right, bottom: top + lay.s(7) + lay.s(22) };
                draw(dc, name, &mut r, DT_LEFT | DT_END_ELLIPSIS);
                SelectObject(dc, self.fonts.small);
                SetTextColor(dc, self.pal.muted);
                let place = if row.page > 0 { format!("{folder}  \u{B7}  page {}", row.page) } else { folder.to_string() };
                let fx = tx + nw + lay.s(10);
                if right - fx > lay.s(60) {
                    let mut r = RECT { left: fx, top: top + lay.s(7), right, bottom: top + lay.s(7) + lay.s(22) };
                    draw(dc, &place, &mut r, DT_LEFT | DT_PATH_ELLIPSIS);
                }
                SelectObject(dc, of);

                // Line 2: the snippet, matches in bold accent colour, cut at the edge.
                self.paint_snippet(dc, &row.snippet, &row.highlights, tx, top + lay.s(29), right, lay);
            }

            if self.content_list.len > self.content_list.visible {
                let track_h = lay.row_h * self.content_list.visible as i32 - lay.s(8);
                let thumb_h = (track_h * self.content_list.visible as i32 / self.content_list.len as i32).max(lay.s(16));
                let span = track_h - thumb_h;
                let denom = (self.content_list.len - self.content_list.visible).max(1) as i32;
                let y = lay.content_rows_y + lay.s(4) + span * self.content_list.top as i32 / denom;
                let b = CreateSolidBrush(self.pal.divider);
                let rg = CreateRoundRectRgn(lay.w - lay.s(7), y, lay.w - lay.s(4), y + thumb_h + 1, lay.s(3), lay.s(3));
                FillRgn(dc, rg, b);
                DeleteObject(rg);
                DeleteObject(b);
            }
        }
    }

    /// One line of text with some byte ranges emphasised, clipped with an ellipsis.
    #[allow(clippy::too_many_arguments)]
    unsafe fn paint_snippet(
        &self,
        dc: HDC,
        text: &str,
        marks: &[std::ops::Range<usize>],
        x: i32,
        y: i32,
        right: i32,
        lay: &Layout,
    ) {
        unsafe {
            let mut pieces: Vec<(&str, bool)> = Vec::new();
            let mut at = 0;
            for m in marks {
                if m.start < at || m.end > text.len() || !text.is_char_boundary(m.start) || !text.is_char_boundary(m.end) {
                    continue;
                }
                if m.start > at {
                    pieces.push((&text[at..m.start], false));
                }
                pieces.push((&text[m.clone()], true));
                at = m.end;
            }
            if at < text.len() {
                pieces.push((&text[at..], false));
            }
            let old = SelectObject(dc, self.fonts.small);
            let mut cx = x;
            for (t, bold) in pieces {
                if t.is_empty() || cx >= right {
                    continue;
                }
                SelectObject(dc, if bold { self.fonts.small_bold } else { self.fonts.small });
                SetTextColor(dc, if bold { self.pal.accent } else { self.pal.muted });
                let w = wide(t);
                let tw = text_w(dc, &w);
                let mut r = RECT { left: cx, top: y, right, bottom: y + lay.s(18) };
                if cx + tw > right {
                    // Does not fit: draw what does, with an ellipsis, and stop.
                    DrawTextW(dc, w.as_ptr(), w.len() as i32, &mut r, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_END_ELLIPSIS);
                    break;
                }
                DrawTextW(dc, w.as_ptr(), w.len() as i32, &mut r, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_NOCLIP);
                cx += tw;
            }
            SelectObject(dc, old);
        }
    }

    /// The file name, with the parts that matched the query in bold.
    unsafe fn paint_name(&self, dc: HDC, row: &crate::Row, x: i32, y: i32, max_w: i32, lay: &Layout) {
        unsafe {
            let name = split_path(&row.path).0;
            let spans = highlight_ranges(name, &self.query);
            let h = lay.s(22);
            // Pieces: (text, bold)
            let mut pieces: Vec<(&str, bool)> = Vec::new();
            let mut at = 0;
            for s in &spans {
                if s.start > at {
                    pieces.push((&name[at..s.start], false));
                }
                pieces.push((&name[s.clone()], true));
                at = s.end;
            }
            if at < name.len() || pieces.is_empty() {
                pieces.push((&name[at..], false));
            }

            // Measure; if it will not fit, draw plain with an ellipsis instead.
            let mut total = 0;
            for (t, bold) in &pieces {
                let old = SelectObject(dc, if *bold { self.fonts.name_bold } else { self.fonts.name });
                total += text_w(dc, &wide(t));
                SelectObject(dc, old);
            }
            SetTextColor(dc, self.pal.text);
            if total > max_w {
                let old = SelectObject(dc, self.fonts.name);
                let mut r = RECT { left: x, top: y, right: x + max_w, bottom: y + h };
                draw(dc, name, &mut r, DT_LEFT | DT_END_ELLIPSIS);
                SelectObject(dc, old);
                return;
            }
            let mut cx = x;
            for (t, bold) in &pieces {
                let old = SelectObject(dc, if *bold { self.fonts.name_bold } else { self.fonts.name });
                if *bold {
                    SetTextColor(dc, self.pal.accent);
                } else {
                    SetTextColor(dc, self.pal.text);
                }
                if t.is_empty() {
                    continue;
                }
                let w = wide(t);
                let mut r = RECT { left: cx, top: y, right: x + max_w, bottom: y + h };
                DrawTextW(dc, w.as_ptr(), w.len() as i32, &mut r, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_VCENTER | DT_NOCLIP);
                cx += text_w(dc, &w);
                SelectObject(dc, old);
            }
        }
    }

    unsafe fn paint_footer(&self, dc: HDC, lay: &Layout) {
        unsafe {
            let old = SelectObject(dc, self.fonts.small);
            SetTextColor(dc, self.pal.muted);
            let mut counts = match (&self.mode, self.rows.len()) {
                (Mode::Results, 0) => String::new(),
                (Mode::Results, n) if self.total as usize > n => format!("{n} of {} results", self.total),
                (Mode::Results, 1) => "1 result".to_string(),
                (Mode::Results, n) => format!("{n} results"),
                _ => String::new(),
            };
            if !self.content.is_empty() {
                let sep = if counts.is_empty() { "" } else { " \u{B7} " };
                counts = format!("{counts}{sep}{} in files", self.content.len());
                if self.content_indexing {
                    counts.push_str(" (still indexing)");
                }
            }
            let mut r = RECT {
                left: lay.pad,
                top: lay.footer_y,
                right: lay.w / 2,
                bottom: lay.footer_y + lay.footer_h,
            };
            draw(dc, &counts, &mut r, DT_LEFT);
            let mut r = RECT {
                left: lay.w / 2,
                top: lay.footer_y,
                right: lay.w - lay.pad,
                bottom: lay.footer_y + lay.footer_h,
            };
            draw(dc, "\u{21B5} Open     Ctrl+\u{21B5} Show in folder     Esc Close", &mut r, DT_RIGHT);
            SelectObject(dc, old);
        }
    }
}
