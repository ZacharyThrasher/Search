# Search

A local, universal search and launcher for Windows 11, built in Rust. Personal-PC tool: no installer, no updater, no encryption layer.

## Status: step 2 of the roadmap (live updates + snapshot)

An in-RAM filename index that stays current while you work and restarts without rescanning (`s` CLI).

- **Index**: one flat, case-folded name buffer plus parent links. A query is a single parallel SIMD substring scan; paths are rebuilt on demand. It can be edited in place: new names are appended, removed names are tombstoned (and squeezed out later), and renaming or moving a folder never touches its children.
- **Live updates**: after loading, a tail thread per volume polls the NTFS USN journal every 100 ms and applies creates, renames and deletes. A change shows up in the next query.
- **Snapshot**: each volume's index is saved to `%LOCALAPPDATA%\Search\<drive>.snap` together with the journal ID and position. On start the snapshot is loaded and only the journal records since it was written are replayed, so a restart takes about a second instead of a full MFT scan.
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

Query rules: case-insensitive substring on the **file name**; several words must all appear (any order); `-d` dirs only, `-f` files only, `--no-count` stops after the first page.

### How a start works (per volume)

1. Load the snapshot if there is one and ask the volume for its journal (read-only; a journal is never created).
2. Journal ID matches and the saved position is still covered: replay the records since, done.
3. Otherwise (no snapshot, unreadable snapshot, journal reset, journal wrapped past the snapshot, snapshot ahead of the journal): full scan. The journal position is read **before** the scan and replayed after it, and replay is idempotent, so changes that race the scan are not lost.
4. While running, if the journal is found reset or wrapped, the volume rescans in the background and keeps answering from the old index until the new one is ready.

Not elevated but a snapshot exists: the snapshot is served read-only, marked stale, with no live updates.

Snapshots are rewritten on exit or every 6 h only when worth it: more than 20,000 changes since the last one, or the snapshot is over 12 h old (or `--save`). Replay makes an older snapshot cheap, and this avoids rewriting ~700 MB for a handful of changes. A snapshot is a temp file renamed into place and checked on load (size, version, checksum, column invariants); anything wrong means a rescan, never a guess.

### Known limits

- Name-only matching; no path matching, ranking, wildcards or regex yet.
- Live updates and the journal both need an elevated process. A launcher that runs unelevated will need a small elevated helper (a later decision).
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
