//! File icons for result rows.
//!
//! Most icons depend only on the extension and come back instantly from the
//! shell without touching the disk. Files whose icon lives in the file itself
//! (programs, shortcuts, icons) are fetched on a background thread; until one
//! arrives the row shows the generic icon for its type.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows_sys::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW};
use windows_sys::Win32::UI::WindowsAndMessaging::{HICON, PostMessageW};

use crate::WM_APP_ICON;

/// Extensions whose icon is stored per file.
const PER_FILE: &[&str] = &["exe", "lnk", "ico", "url", "scr", "cpl", "appref-ms", "msc"];

fn ext_of(path: &str) -> String {
    let name = path.rsplit('\\').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

struct Shared {
    /// Cache key -> HICON (as usize so the map is Send).
    map: Mutex<HashMap<String, usize>>,
    pending: Mutex<HashSet<String>>,
}

pub struct Icons {
    shared: Arc<Shared>,
    tx: Sender<String>,
}

fn load(path: &str, attrs: u32, by_attrs_only: bool) -> usize {
    let w: Vec<u16> = path.encode_utf16().chain([0]).collect();
    // SAFETY: valid NUL-terminated path and an out-struct of the right size.
    unsafe {
        let mut sfi: SHFILEINFOW = std::mem::zeroed();
        let mut flags = SHGFI_ICON | SHGFI_LARGEICON;
        if by_attrs_only {
            flags |= SHGFI_USEFILEATTRIBUTES;
        }
        let r = SHGetFileInfoW(w.as_ptr(), attrs, &mut sfi, std::mem::size_of::<SHFILEINFOW>() as u32, flags);
        if r == 0 { 0 } else { sfi.hIcon as usize }
    }
}

impl Icons {
    pub fn start(hwnd: HWND) -> Icons {
        let shared = Arc::new(Shared {
            map: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashSet::new()),
        });
        let (tx, rx) = channel::<String>();
        let (s, hwnd) = (shared.clone(), hwnd as usize);
        std::thread::Builder::new()
            .name("icons".into())
            .spawn(move || {
                // SAFETY: standard COM init for a shell-calling thread.
                unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
                while let Ok(path) = rx.recv() {
                    let h = load(&path, FILE_ATTRIBUTE_NORMAL, false);
                    {
                        let mut map = s.map.lock().unwrap_or_else(|e| e.into_inner());
                        // Per-file icons are cached by path; start over when that gets big.
                        if map.len() > 600 {
                            map.retain(|k, &mut v| {
                                let generic = k.starts_with('.') || k.starts_with('<');
                                if !generic && v != 0 {
                                    // SAFETY: handle came from SHGetFileInfoW; rows that still hold it just redraw generic next paint.
                                    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyIcon(v as HICON) };
                                }
                                generic
                            });
                        }
                        map.insert(path.clone(), h);
                    }
                    s.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&path);
                    // SAFETY: posting to a window handle.
                    unsafe { PostMessageW(hwnd as HWND, WM_APP_ICON, 0, 0) };
                }
            })
            .expect("spawn icon thread");
        Icons { shared, tx }
    }

    /// The icon for `path`, or null if the shell has none.
    pub fn get(&self, path: &str, is_dir: bool) -> HICON {
        let ext = ext_of(path);
        let generic_key = if is_dir { "<dir>".to_string() } else { format!(".{ext}") };
        let generic = {
            let mut map = self.shared.map.lock().unwrap_or_else(|e| e.into_inner());
            *map.entry(generic_key.clone()).or_insert_with(|| {
                if is_dir {
                    load("folder", FILE_ATTRIBUTE_DIRECTORY, true)
                } else {
                    load(&format!("file.{ext}"), FILE_ATTRIBUTE_NORMAL, true)
                }
            })
        };
        if !is_dir && PER_FILE.contains(&ext.as_str()) {
            if let Some(&h) = self.shared.map.lock().unwrap_or_else(|e| e.into_inner()).get(path) {
                if h != 0 {
                    return h as HICON;
                }
            } else if self.shared.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(path.to_string()) {
                let _ = self.tx.send(path.to_string());
            }
        }
        generic as HICON
    }
}
