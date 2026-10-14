# Search

A local, universal search and launcher for Windows 11, built in Rust. Personal-PC tool: no installer, no updater, no encryption layer.

## Status: step 3 of the roadmap (hotkey popup)

Press **Alt+Space**, type, Enter: see [The popup](#the-popup-step-3). It sits on the index from steps 1 and 2, described next.

### Step 2: live updates + snapshot

An in-RAM filename index that stays current while you work and restarts without rescanning (`s` CLI).

- **Index**: one flat, case-folded name buffer plus parent links. A query is a single parallel SIMD substring scan; paths are rebuilt on demand. It can be edited in place: new names are appended, removed names are tombstoned (and squeezed out later), and renaming or moving a folder never touches its children.
- **Live updates**: after loading, a tail thread per volume polls the NTFS USN journal every 100 ms and applies creates, renames and deletes. A change shows up in the next query.
- **Snapshot**: each volume's index is saved to `<data folder>\<drive>.snap` (`%LOCALAPPDATA%\Search`, or `C:\Program Files\Search\data` when elevated) together with the journal ID and position. On start the snapshot is loaded and only the journal records since it was written are replayed, so a restart takes about a second instead of a full MFT scan.
- **Source `mft`** (default): enumerates NTFS volumes with `FSCTL_ENUM_USN_DATA`, no directory walking. Needs an **elevated** shell (raw volume handle).
- **Source `walk`**: parallel directory walk. No elevation, any file system. Slower, static (no live updates, no snapshot); also the ground truth for the MFT path.

### Run

```powershell
cargo build --release

# Elevated shell (Administrator): every fixed NTFS drive.
# First run scans and saves a snapshot; later runs load it.
.\target\release\s.exe notepad

# Interactive: keeps tailing the journal, so new files appear in the next query
.\target\release\s.exe -v C -v E

# Prove it on this machine (self-elevates): live latency, instant restart, offline catch-up
.\scripts\live-test.ps1

# Step-1 style timing runs (always scan, never touch snapshots)
.\scripts\bench-mft.ps1
.\target\release\s.exe --no-snapshot -v C -v E --bench 200 --queries-file benches\queries.txt

# No elevation: walk folders instead
.\target\release\s.exe --source walk --root C:\Users notepad
```

Flags worth knowing: `--rescan` (ignore the snapshot, scan, save), `--no-snapshot` (step-1 behavior), `--save` (write the snapshot on exit even if little changed), `--no-live` (interactive without tailing), `--data-dir DIR`, `--live-test DIR`. In interactive mode `:stats` shows per-volume state.

Query rules: case-insensitive on the **file name**; several words must all appear (any order); wildcards, `|`, `!`, `ext:`, paths and `regex:` work too (see [Query syntax](#query-syntax)); `-d` dirs only, `-f` files only, `--no-count` stops after the first page.

### How a start works (per volume)

1. Load the snapshot if there is one and ask the volume for its journal (read-only; a journal is never created).
2. Journal ID matches and the saved position is still covered: replay the records since, done.
3. Otherwise (no snapshot, unreadable snapshot, journal reset, journal wrapped past the snapshot, snapshot ahead of the journal): full scan. The journal position is read **before** the scan and replayed after it, and replay is idempotent, so changes that race the scan are not lost.
4. While running, if the journal is found reset or wrapped, the volume rescans in the background and keeps answering from the old index until the new one is ready.

Not elevated but a snapshot exists: the snapshot is served read-only, marked stale, with no live updates.

Snapshots are rewritten on exit or every 6 h only when worth it: more than 20,000 changes since the last one, or the snapshot is over 12 h old (or `--save`). Replay makes an older snapshot cheap, and this avoids rewriting ~700 MB for a handful of changes. A snapshot is a temp file renamed into place and checked on load (size, version, checksum, column invariants); anything wrong means a rescan, never a guess.

### Known limits

- Name matching with wildcards, paths and regex (see [Query syntax](#query-syntax)); the CLI lists in index order, the popup ranks.
- Live updates and the journal both need an elevated process; the popup gets them from the elevated helper `searchd` (step 3).
- NTFS only for `mft`; volumes without an active USN journal are skipped (not created).
- One name per file (hard links show a single name).
- The index uses about 8 bytes/entry more than step 1 for the record table that makes live edits possible (about 51 B/entry).
- A file created and kept open may only show up when the journal records it; `s --live-test` reports that case separately.
- Snapshot writes briefly pause applying new changes (queries keep working).

### Tests

```powershell
cargo test --release
cargo test --release --lib scale -- --ignored --nocapture   # 15.7M-entry save/load timing
```

Covers the USN buffer parser (including hostile input), the index against a naive scan and a random-edit model, snapshot round-trips and damage detection, and the whole engine (resume, replay, wrapped/reset journal, scan races, live tail, not-elevated) against a fake volume. The real ioctl path needs an elevated shell and is exercised by `scripts\live-test.ps1` and `scripts\bench-mft.ps1`.

## The popup (step 3)

Names only (no content search yet). Three programs:

| | runs as | what it does |
|---|---|---|
| `searchd.exe` | **elevated** (logon task) | owns the index (MFT scan or snapshot, then the USN tail) and answers queries over a per-user named pipe |
| `searchpop.exe` | normal | the hidden window: hotkey, text box, results, launching. Whatever it opens runs **unelevated** |
| `searchq.exe` | any | command-line client for the helper (`searchq notepad`, `--status`, `--bench 200 notepad`) |

### Install (once, and again after you rebuild)

```powershell
.\scripts\install.ps1      # builds, one UAC prompt, starts both
.\scripts\uninstall.ps1    # removes the task, the files and the autostart (-RemoveData also wipes snapshots)
```

- The helper is copied to `C:\Program Files\Search` (admin-write only, so an unprivileged process cannot swap the exe that runs elevated) and run by the scheduled task `SearchHelper` at your logon with highest privileges. The popup starts via `HKCU\...\Run`. Updating means running `install.ps1` again.
- If the helper is not running when you search, the popup starts the task itself and says "Starting the search helper…". While the index loads it says "Still indexing…".
- The helper's snapshots and log (`searchd.log`) live in `C:\Program Files\Search\data`: the helper is elevated, so it must not write to a folder a normal process could redirect with a junction or link. The popup's log (`searchpop.log`) and config stay in `%LOCALAPPDATA%\Search`. Any elevated run (the helper, or `s` in an admin terminal) uses the Program Files folder unless you pass `--data-dir`.

### Keys

| | |
|---|---|
| type | filter as you type (results update per keystroke) |
| Up / Down, PgUp / PgDn | move (wraps at the ends); mouse wheel scrolls, click selects, double-click opens |
| Enter | open (ShellExecute, unelevated) |
| Ctrl+Enter | open the containing folder with the item selected |
| Esc, Alt+Space again, click elsewhere | hide |
| Ctrl+A, Ctrl+Backspace | select all, delete word |

Tray icon: click toggles the popup; right-click for Open settings file / Open log folder / Quit.

### Query syntax

Everything-style, case-insensitive everywhere. Plain words behave as before.

| You type | It means |
|---|---|
| `report` | the name contains `report` |
| `*.png` | the name **ends in** `.png` (a term with `*` or `?` must match the whole name) |
| `*_reaction.gif` | works as typed |
| `report*` / `*report*` | starts with / contains `report` |
| `c?t` | `cat`, `cut`, ... (`?` is exactly one character) |
| `report *.pdf` | space = AND |
| `*.png \| *.gif` | `\|` = OR (AND binds tighter: `a b \| c` is `(a b) or c`) |
| `report !draft` | leading `!` = NOT |
| `ext:png;jpg` | the name ends in `.png` or `.jpg` |
| `projects\*.md`, `users/zach/` | a `\` or `/` in a term matches the **full path** instead of the name. Without wildcards it is a substring of the path; with wildcards it must end at the end of the path, and only starts at the beginning when it begins with a drive (`c:\users\*\desktop\*.png`). `*` also crosses folders |
| `regex:^img_\d+\.jpe?g$` | a regular expression on the name (the rest of the word; `\|` inside it is the regex's own) |
| `"my file"` | quotes keep spaces (and `\|`) literal inside one term |

Half-typed input never errors: a broken regex (say a trailing `regex:(`) simply shows no results.

How it stays fast: every term with a literal part (`.png`, `_reaction.gif`, `report`, an `ext:` list, a regex's own prefix or suffix) is scanned for with SIMD `memmem` over the flat name buffer, with the name's terminating NUL included for end-anchored patterns so `*.png` only finds names that end in `.png`; only those candidates are checked in full. `*.png`-style globs and `ext:` use a small per-entry extension hash (2 bytes) instead of scanning names. Path terms are matched per **folder**, not per file: each folder keeps a DFA state for its own path (4 bytes per entry for the folder link, plus a row per folder), so most files are accepted or rejected from their folder's state alone. Terms with no literal (`???`, `!x`) still look at every entry. About 8 extra bytes per entry in RAM; nothing is added to the snapshot, the hints are rebuilt on load.

### Ranking

Order for plain words (optionally with `!` and `ext:`): **exact name** (the name, or the name without its extension, equals what you typed), then **name starts with** your first word, then **anything else that matched**. Within each group the **shorter full path** wins. Queries with wildcards, `|`, a path or a regex have no such groups: **shorter full path first, then name**. Recency and frequency come later.

### Settings

`%LOCALAPPDATA%\Search\config.toml` (created on first run; read at start, so quit and restart the popup after editing):

```toml
hotkey = "Alt+Space"     # modifiers Alt/Ctrl/Shift/Win + one key: "Ctrl+Alt+Space", "Win+Shift+F", ...
max_results = 50         # asked from the helper
visible_rows = 8         # shown at once, the rest scroll
width = 680              # px at 100% scaling
theme = "system"         # or "dark" / "light"
```

### How it is built

- **UI**: raw Win32 and GDI, no framework. The window is created once and kept hidden; the hotkey only moves and shows it (the time is logged to `searchpop.log` when it exceeds 16 ms, or always with `SEARCHPOP_TRACE=1`). Native EDIT control for typing (clipboard, IME, undo). Per-monitor-v2 DPI, opens on the monitor with the cursor, follows the system dark/light setting, rounded corners via DWM. The dark theme uses the acrylic backdrop; the light theme is a solid background, because GDI cannot draw dark text on glass (DWM treats dark pixels as transparent). Text is GDI grayscale anti-aliased, not DirectWrite. Fixing both properly means Direct2D/DirectComposition (a later polish step).
- **Protocol** (`src/ipc.rs`): length-prefixed binary frames over a byte-mode pipe `\\.\pipe\SearchHelper-<your SID>`. The pipe's DACL grants only your user, remote clients are refused, and clients connect with identification-level impersonation. The helper never executes anything; it answers queries (plus `Save` and `Shutdown`).
- **Snapshots no longer stall queries**: a snapshot write holds the read lock for about a second, and the tail's write used to queue behind it and block every new query. The tail now waits on a separate gate while a snapshot is written, so a queued writer never holds readers back (`live::tests::queries_never_wait_on_a_snapshot_write`).

### Know before you rely on it

- **Any process running as you can read the index** through the pipe, including names inside folders your account cannot list (the helper reads the MFT as admin). It sees names only, never contents, and cannot make the helper run anything.
- The pipe name is predictable. If another account on the same PC created `\\.\pipe\SearchHelper-<your SID>` before the helper did, the popup would talk to it. Not defended on this single-user PC; the fix would be to check the server's exe path before trusting a reply.
- The helper keeps the whole index in RAM (about 51 B per entry, so roughly 800 MB for 15.7 M files).
- The helper writes a snapshot right after any scan (first run, or when the old one was no use), then hourly when worth it (see the snapshot rules above), and on exit: the pipe's `Shutdown`, Windows logoff/shutdown (`WM_ENDSESSION`), or a close request from Task Scheduler (`WM_CLOSE`). A hard kill or crash loses nothing: the next start replays the USN journal.
- `searchd --walk DIR` indexes a folder without elevation (static; handy for trying the popup); `searchd --console` echoes the log to the terminal it was started from.
