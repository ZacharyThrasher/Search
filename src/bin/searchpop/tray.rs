//! Notification-area icon: the popup has no taskbar button, so this is how you
//! see that it is running and how you quit it.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::WM_APP_TRAY;

const TRAY_ID: u32 = 1;

fn light_taskbar() -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0".encode_utf16().collect();
    let val: Vec<u16> = "SystemUsesLightTheme\0".encode_utf16().collect();
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: valid strings and an out buffer of the stated size.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut data as *mut u32 as *mut _,
            &mut size,
        )
    };
    r == 0 && data != 0
}

/// A magnifier glyph rendered once into a 32-bit icon, white or near-black to
/// suit the taskbar. Falls back to the stock application icon on any failure.
fn make_icon() -> HICON {
    // SAFETY: GDI objects are created, used and released within this function.
    unsafe {
        let size = 32;
        let dc = CreateCompatibleDC(null_mut());
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = size;
        bi.bmiHeader.biHeight = -size;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut core::ffi::c_void = null_mut();
        let color = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if color.is_null() || bits.is_null() {
            DeleteDC(dc);
            return LoadIconW(null_mut(), IDI_APPLICATION);
        }
        let old = SelectObject(dc, color);
        let face: Vec<u16> = "Segoe Fluent Icons\0".encode_utf16().collect();
        let font = CreateFontW(
            -26, 0, 0, 0, 400, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32, ANTIALIASED_QUALITY as u32, (DEFAULT_PITCH | FF_DONTCARE) as u32, face.as_ptr(),
        );
        let oldf = SelectObject(dc, font);
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, 0x00FF_FFFF);
        let g: Vec<u16> = "\u{E721}".encode_utf16().collect();
        let mut r = RECT { left: 0, top: 0, right: size, bottom: size };
        DrawTextW(dc, g.as_ptr(), g.len() as i32, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        SelectObject(dc, oldf);
        DeleteObject(font);

        // White on black -> coverage becomes alpha; colour is white or near-black.
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        let ink = if light_taskbar() { 0x20u32 } else { 0xFFu32 };
        for p in px.iter_mut() {
            let a = (*p & 0xFF).max((*p >> 8) & 0xFF).max((*p >> 16) & 0xFF);
            let c = ink * a / 255; // premultiplied
            *p = (a << 24) | (c << 16) | (c << 8) | c;
        }
        let mask = CreateBitmap(size, size, 1, 1, null());
        let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(mask);
        SelectObject(dc, old);
        DeleteObject(color);
        DeleteDC(dc);
        if icon.is_null() { LoadIconW(null_mut(), IDI_APPLICATION) } else { icon }
    }
}

fn data(hwnd: HWND, tip: &str) -> NOTIFYICONDATAW {
    // SAFETY: zeroed NOTIFYICONDATAW is a valid starting point; fields are set below.
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ID;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_APP_TRAY;
    for (i, c) in tip.encode_utf16().take(nid.szTip.len() - 1).enumerate() {
        nid.szTip[i] = c;
    }
    nid
}

pub fn add(hwnd: HWND, tip: &str, _dark: bool) {
    let mut nid = data(hwnd, tip);
    nid.hIcon = make_icon();
    // SAFETY: valid NOTIFYICONDATAW; the shell copies the icon.
    unsafe {
        Shell_NotifyIconW(NIM_ADD, &nid);
        DestroyIcon(nid.hIcon);
    }
}

pub fn remove(hwnd: HWND) {
    let nid = data(hwnd, "");
    // SAFETY: valid NOTIFYICONDATAW.
    unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
}
