# Search

A local, universal search and launcher for Windows 11, built in Rust. Personal-PC tool: no installer, no updater, no encryption layer.

## Status: step 1 of the roadmap

In-RAM filename index plus a timing CLI (`s`).

- **Index**: one flat, case-folded name buffer plus parent links. A query is a single parallel SIMD substring scan; paths are rebuilt on demand.
- **Source `mft`** (default): enumerates NTFS volumes with `FSCTL_ENUM_USN_DATA`, no directory walking. Needs an **elevated** shell (raw volume handle).
- **Source `walk`**: parallel directory walk. No elevation, any file system (FAT32, ReFS, network). Slower; also serves as ground truth for the MFT path.
- **Timings**: the CLI prints enumeration time (kernel read / parse / resolve), index memory, process working set, and per-query latency.

### Run

```powershell
cargo build --release

# Elevated shell (Administrator): every fixed NTFS drive
.\target\release\s.exe notepad
.\target\release\s.exe -v C -v E --bench 200 --queries-file benches\queries.txt

# Or let the script self-elevate and save results to bench-results\ (serial vs parallel, thread sweep, query bench)
.\scripts\bench-mft.ps1

# No elevation: walk folders instead
.\target\release\s.exe --source walk --root C:\Users notepad
```

With no query it drops into an interactive prompt (index once, then type queries).

Query rules: case-insensitive substring on the **file name**; several words must all appear (any order); `-d` dirs only, `-f` files only, `--no-count` stops after the first page.

### Known limits (step 1)

- Name-only matching; no path matching, ranking, wildcards or regex yet.
- Index is rebuilt on every run (no snapshot, no USN tail): that is step 2.
- NTFS only for `mft`; volumes without an active USN journal are skipped (not created).
- One name per file (hard links show a single name).

### Tests

```powershell
cargo test --release
```

Covers the USN buffer parser (including hostile input), the index against a naive scan, and the walker. The MFT ioctl path itself needs an elevated shell and is exercised by `scripts\bench-mft.ps1`.
