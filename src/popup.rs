//! The popup's logic that does not need a window: the config file, hotkey
//! parsing, the result list's selection/scroll model and match highlighting.
//! The Win32 glue in `searchpop` only calls into this, so it can be tested.

use std::ops::Range;

use crate::query::Query;

/// Modifier bits, the same values Windows uses for `RegisterHotKey`.
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    /// Windows virtual-key code.
    pub vk: u32,
}

/// Parse `"Alt+Space"`, `"ctrl+shift+K"`, `"Win+F2"` and similar. A hotkey
/// needs at least one modifier (a bare key would steal typing everywhere).
pub fn parse_hotkey(s: &str) -> Result<Hotkey, String> {
    let mut modifiers = 0;
    let mut vk = None;
    for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let lower = part.to_ascii_lowercase();
        let m = match lower.as_str() {
            "alt" => MOD_ALT,
            "ctrl" | "control" => MOD_CONTROL,
            "shift" => MOD_SHIFT,
            "win" | "windows" | "super" | "meta" => MOD_WIN,
            _ => 0,
        };
        if m != 0 {
            modifiers |= m;
            continue;
        }
        if vk.is_some() {
            return Err(format!("more than one key in hotkey {s:?}"));
        }
        vk = Some(key_code(&lower).ok_or_else(|| format!("unknown key {part:?} in hotkey {s:?}"))?);
    }
    let vk = vk.ok_or_else(|| format!("hotkey {s:?} has no key"))?;
    if modifiers == 0 {
        return Err(format!("hotkey {s:?} needs a modifier (Alt, Ctrl, Shift or Win)"));
    }
    Ok(Hotkey { modifiers, vk })
}

fn key_code(name: &str) -> Option<u32> {
    let b = name.as_bytes();
    if b.len() == 1 {
        match b[0] {
            c @ b'a'..=b'z' => return Some((c - b'a' + b'A') as u32),
            c @ b'0'..=b'9' => return Some(c as u32),
            b'`' => return Some(0xC0),
            b';' => return Some(0xBA),
            b'=' => return Some(0xBB),
            b',' => return Some(0xBC),
            b'-' => return Some(0xBD),
            b'.' => return Some(0xBE),
            b'/' => return Some(0xBF),
            b'[' => return Some(0xDB),
            b'\\' => return Some(0xDC),
            b']' => return Some(0xDD),
            b'\'' => return Some(0xDE),
            _ => return None,
        }
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
        && (1..=24).contains(&n)
    {
        return Some(0x70 + n - 1);
    }
    Some(match name {
        "space" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "insert" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "up" => 0x26,
        "down" => 0x28,
        "left" => 0x25,
        "right" => 0x27,
        "backtick" | "grave" => 0xC0,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    System,
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub hotkey: String,
    /// Results requested from the helper.
    pub max_results: usize,
    /// Rows shown at once (the rest scroll).
    pub visible_rows: usize,
    /// Window width at 100% scale, in pixels.
    pub width: i32,
    pub theme: Theme,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hotkey: "Alt+Space".into(),
            max_results: 50,
            visible_rows: 8,
            width: 680,
            theme: Theme::System,
        }
    }
}

pub const DEFAULT_CONFIG_FILE: &str = "\
# Search popup settings. Changes apply the next time the popup starts
# (right-click the tray icon > Quit, then start searchpop again).

# Global hotkey: modifiers Alt, Ctrl, Shift, Win plus one key
# (Space, Enter, A-Z, 0-9, F1-F24, ...). Examples: \"Ctrl+Alt+Space\", \"Win+Shift+F\".
hotkey = \"Alt+Space\"

# How many results to ask the helper for, and how many rows to show at once.
max_results = 50
visible_rows = 8

# Window width in pixels at 100% scaling.
width = 680

# \"system\" follows Windows, or force \"dark\" / \"light\".
theme = \"system\"
";

impl Config {
    /// Parse the TOML-like `key = value` file. Unknown keys and bad values are
    /// reported in the second return value and otherwise ignored, so a typo
    /// never stops the popup from starting.
    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut c = Config::default();
        let mut problems = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                problems.push(format!("line {}: expected key = value", n + 1));
                continue;
            };
            let (k, v) = (k.trim(), unquote(v.trim()));
            let num = |lo: i64, hi: i64| v.parse::<i64>().ok().filter(|x| (lo..=hi).contains(x));
            match k {
                "hotkey" => match parse_hotkey(v) {
                    Ok(_) => c.hotkey = v.to_string(),
                    Err(e) => problems.push(format!("line {}: {e}", n + 1)),
                },
                "max_results" => match num(1, 500) {
                    Some(x) => c.max_results = x as usize,
                    None => problems.push(format!("line {}: max_results must be 1-500", n + 1)),
                },
                "visible_rows" => match num(1, 20) {
                    Some(x) => c.visible_rows = x as usize,
                    None => problems.push(format!("line {}: visible_rows must be 1-20", n + 1)),
                },
                "width" => match num(320, 2000) {
                    Some(x) => c.width = x as i32,
                    None => problems.push(format!("line {}: width must be 320-2000", n + 1)),
                },
                "theme" => match v.to_ascii_lowercase().as_str() {
                    "system" => c.theme = Theme::System,
                    "dark" => c.theme = Theme::Dark,
                    "light" => c.theme = Theme::Light,
                    _ => problems.push(format!("line {}: theme must be system, dark or light", n + 1)),
                },
                _ => problems.push(format!("line {}: unknown setting {k:?}", n + 1)),
            }
        }
        (c, problems)
    }
}

/// Drop a trailing `# comment` that is not inside quotes.
fn strip_comment(s: &str) -> &str {
    let mut in_q = false;
    for (i, ch) in s.char_indices() {
        match ch {
            '"' => in_q = !in_q,
            '#' if !in_q => return &s[..i],
            _ => {}
        }
    }
    s
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}

/// Split `C:\Users\Zach\notes.txt` into (`notes.txt`, `C:\Users\Zach`).
/// A drive root (`C:\`) is its own name with an empty folder.
pub fn split_path(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('\\');
    match trimmed.rfind('\\') {
        Some(i) => (&trimmed[i + 1..], &trimmed[..i]),
        None if trimmed.is_empty() => (path, ""),
        None => (trimmed, ""),
    }
}

/// Byte ranges of `name` that the positive terms of `query` matched (plain
/// text, the literal parts of wildcards, `ext:`, `regex:`), sorted and merged.
/// Empty if case folding changes the byte length of the name (positions would
/// not line up).
pub fn highlight_ranges(name: &str, query: &str) -> Vec<Range<usize>> {
    thread_local! {
        // Rows are painted one after another with the same query: parse it once.
        static PARSED: std::cell::RefCell<Option<(String, Query)>> = const { std::cell::RefCell::new(None) };
    }
    PARSED.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().is_none_or(|(text, _)| text != query) {
            *slot = Some((query.to_string(), Query::parse(query)));
        }
        slot.as_ref()
            .map(|(_, q)| q.highlight(name))
            .unwrap_or_default()
    })
}

/// Selection and scrolling over a list of `len` rows of which `visible` fit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListView {
    pub len: usize,
    pub visible: usize,
    pub selected: usize,
    pub top: usize,
}

impl ListView {
    pub fn reset(&mut self, len: usize) {
        self.len = len;
        self.selected = 0;
        self.top = 0;
    }

    /// Move the selection by `delta` rows, wrapping around the ends.
    pub fn step(&mut self, delta: isize) {
        if self.len == 0 {
            return;
        }
        let n = self.len as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
        self.scroll_to_selection();
    }

    /// Move by a page, stopping at the ends (no wrapping).
    pub fn page(&mut self, dir: isize) {
        if self.len == 0 {
            return;
        }
        let jump = self.visible.max(1) as isize * dir;
        self.selected = (self.selected as isize + jump).clamp(0, self.len as isize - 1) as usize;
        self.scroll_to_selection();
    }

    pub fn select(&mut self, i: usize) {
        if i < self.len {
            self.selected = i;
            self.scroll_to_selection();
        }
    }

    /// Scroll the window of rows without moving the selection (mouse wheel).
    pub fn scroll(&mut self, delta: isize) {
        let max_top = self.len.saturating_sub(self.visible) as isize;
        self.top = (self.top as isize + delta).clamp(0, max_top) as usize;
    }

    pub fn scroll_to_selection(&mut self) {
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.visible > 0 && self.selected >= self.top + self.visible {
            self.top = self.selected + 1 - self.visible;
        }
    }

    /// Rows currently on screen.
    pub fn shown(&self) -> Range<usize> {
        self.top..(self.top + self.visible).min(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys_parse() {
        assert_eq!(parse_hotkey("Alt+Space").unwrap(), Hotkey { modifiers: MOD_ALT, vk: 0x20 });
        assert_eq!(
            parse_hotkey(" ctrl + SHIFT + k ").unwrap(),
            Hotkey { modifiers: MOD_CONTROL | MOD_SHIFT, vk: b'K' as u32 }
        );
        assert_eq!(parse_hotkey("Win+F12").unwrap(), Hotkey { modifiers: MOD_WIN, vk: 0x7B });
        assert_eq!(parse_hotkey("Alt+;").unwrap().vk, 0xBA);
        assert!(parse_hotkey("Space").is_err()); // no modifier
        assert!(parse_hotkey("Alt").is_err()); // no key
        assert!(parse_hotkey("Alt+A+B").is_err());
        assert!(parse_hotkey("Alt+Banana").is_err());
        assert!(parse_hotkey("Alt+F25").is_err());
        assert!(parse_hotkey("").is_err());
    }

    #[test]
    fn config_defaults_and_overrides() {
        let (c, p) = Config::parse("");
        assert_eq!(c, Config::default());
        assert!(p.is_empty());

        let (c, p) = Config::parse(
            "# comment\nhotkey = \"Ctrl+Alt+Space\"  # trailing\nmax_results=20\nvisible_rows = 6\nwidth=800\ntheme = \"Dark\"\n",
        );
        assert!(p.is_empty(), "{p:?}");
        assert_eq!(c.hotkey, "Ctrl+Alt+Space");
        assert_eq!((c.max_results, c.visible_rows, c.width, c.theme), (20, 6, 800, Theme::Dark));
    }

    #[test]
    fn bad_config_lines_are_reported_and_ignored() {
        let (c, p) = Config::parse("hotkey = \"Space\"\nmax_results = 0\nwidth = x\ntheme = pink\nnonsense\nwho = 1\n");
        assert_eq!(c, Config::default());
        assert_eq!(p.len(), 6, "{p:?}");
        // A '#' inside quotes is not a comment.
        let (c, _) = Config::parse("hotkey = \"Alt+#\"\n");
        assert_eq!(c.hotkey, Config::default().hotkey); // '#' is not a key, so rejected
    }

    #[test]
    fn default_file_parses_to_defaults() {
        let (c, p) = Config::parse(DEFAULT_CONFIG_FILE);
        assert_eq!(c, Config::default());
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn paths_split_into_name_and_folder() {
        assert_eq!(split_path(r"C:\Users\Zach\notes.txt"), ("notes.txt", r"C:\Users\Zach"));
        assert_eq!(split_path(r"C:\Users"), ("Users", "C:"));
        assert_eq!(split_path(r"C:\"), ("C:", ""));
        assert_eq!(split_path(r"C:\Users\Zach\"), ("Zach", r"C:\Users"));
        assert_eq!(split_path("loose"), ("loose", ""));
    }

    #[test]
    fn highlights_merge_and_ignore_case() {
        assert_eq!(highlight_ranges("Notepad++.exe", "note"), vec![0..4]);
        assert_eq!(highlight_ranges("Notepad++.exe", "pad NOTE"), vec![0..7]);
        assert_eq!(highlight_ranges("a-a-a", "a"), vec![0..1, 2..3, 4..5]);
        assert_eq!(highlight_ranges("abc", "xyz"), Vec::<Range<usize>>::new());
        assert_eq!(highlight_ranges("abc", "  "), Vec::<Range<usize>>::new());
        // Accents line up when folding keeps byte lengths.
        assert_eq!(highlight_ranges("Résumé.pdf", "RÉSUMÉ"), vec![0..8]);
        // 'İ' lowercases to two chars; positions would drift, so no highlight.
        assert!(highlight_ranges("İstanbul", "stan").is_empty());
        // Same total length but shifted boundaries ('Ⱥ' 2->3 bytes, 'Ɱ' 3->2): must not panic.
        assert!(highlight_ranges("\u{23A}\u{2C6E}", "\u{271}").is_empty());
    }

    #[test]
    fn list_selection_wraps_and_scrolls() {
        let mut l = ListView { visible: 3, ..Default::default() };
        l.reset(10);
        l.step(-1); // wraps to the last row
        assert_eq!((l.selected, l.top), (9, 7));
        l.step(1); // wraps to the first
        assert_eq!((l.selected, l.top), (0, 0));
        for _ in 0..4 {
            l.step(1);
        }
        assert_eq!((l.selected, l.top), (4, 2));
        l.step(-3);
        assert_eq!((l.selected, l.top), (1, 1));
        l.page(1);
        assert_eq!(l.selected, 4);
        l.page(1);
        l.page(1);
        l.page(1);
        assert_eq!(l.selected, 9); // stops at the end
        l.scroll(-100);
        assert_eq!(l.top, 0);
        assert_eq!(l.shown(), 0..3);
        l.scroll(100);
        assert_eq!(l.shown(), 7..10);
        // Empty and short lists.
        l.reset(0);
        l.step(1);
        assert_eq!(l.selected, 0);
        l.reset(2);
        assert_eq!(l.shown(), 0..2);
        l.step(1);
        assert_eq!((l.selected, l.top), (1, 0));
    }
}
