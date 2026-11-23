<p align="center">
  <img src="docs/mootui-cow.svg" alt="mootui logo: a cross-eyed cow chewing on photos" width="160">
</p>

# mootui 

### Photo importer for the terminal (macOS)

**mootui** is a fast, keyboard-driven terminal app (TUI) for **importing photos and videos from a camera SD card** or any folder into a dated photo library. It **skips duplicates**, **organizes files into folders by date**, keeps RAW + JPEG pairs and sidecars together, and can **tidy an existing messy photo folder**. Every move is logged and can be undone.

Think of it as the import step of Lightroom, Photos or digiKam, in a single small binary that you run from the terminal. It does no editing and no cataloguing UI, and it leaves your files as plain folders that any app can read.

## Why mootui?

- **No duplicate imports.** Re-insert the same card as often as you like; only new shots are copied. Duplicates are detected by filename, size and capture time (as Lightroom does) or, optionally, by file content even after renaming.
- **Date-based folder structure.** Photos land in `2026-10/RAW/DSC_0412.NEF`, `2026/2026-10-13/…`, `Fujifilm X-T5/2026/…` or whatever layout you define.
- **Safe by design.** Copies are verified before they are put in place. Nothing is ever overwritten. Duplicates are never deleted on import. Tidy and reorganize runs can be undone.
- **Fast.** Imports as fast as Finder, `cp` or `ditto`, even though it also sorts by date, skips duplicates and verifies every copy (see [Performance](#performance)).
- **Your files stay the source of truth.** The SQLite catalog is only an index: files added or deleted in Finder are picked up automatically, and the catalog can be rebuilt from scratch.
- **Works with your editor.** Folders you have already sorted are left alone, so Lightroom Classic, Capture One, darktable, RawTherapee and digiKam keep finding their files.
- **Scriptable.** Every action is also available as a non-interactive command.

Supported formats include RAW (`.NEF`, `.CR2`, `.CR3`, `.ARW`, `.RAF`, `.DNG`, `.ORF`, `.RW2`, …), JPEG, HEIC/HEIF, PNG, TIFF, WebP, video (`.MOV`, `.MP4`, `.MTS`, `.INSV`, …) and sidecars (`.XMP`, `.PP3`, `.AAE`, `.DOP`, …).

## Installation

mootui runs on **macOS** (Apple Silicon and Intel). It uses `diskutil` to detect and eject cards and the macOS Trash for removed duplicates.

### Homebrew (recommended)

```sh
brew install kartik0trivedi/tap/mootui
```

This installs a ready-made binary, so you don't need Rust, and also installs [ExifTool](https://exiftool.org), which mootui uses for accurate capture dates. Update with `brew upgrade mootui`.

### Download a release

Download `mootui-vX.Y.Z-universal-apple-darwin.tar.gz` from [Releases](https://github.com/kartik0trivedi/mootui/releases), then:

```sh
tar -xzf mootui-v*-universal-apple-darwin.tar.gz
cd mootui-v*-universal-apple-darwin
xattr -d com.apple.quarantine mootui    # the binary isn't notarized; this lets macOS run it
mv mootui ~/.local/bin/                 # or any directory on your PATH
brew install exiftool                   # recommended, for accurate capture dates
```

### Build from source

Requires [Rust](https://rustup.rs) (stable).

```sh
git clone https://github.com/kartik0trivedi/mootui
cd mootui
cargo build --release
cp target/release/mootui ~/.local/bin/   # or any directory on your PATH
```

Or in one step: `cargo install --git https://github.com/kartik0trivedi/mootui`.

The binary is self-contained. Your catalog and logs live inside your photo library, not next to the binary.

## Quick start

```sh
mootui
```

Insert a card. It appears in the list. Select it, review what will be imported (new files, duplicates and conflicts), and press `enter`. When it finishes, press `e` to eject the card.

Your library defaults to `~/Pictures`. Press `L` to choose another one.

## Usage

```sh
mootui                                   # interactive: pick a card, folder or Tidy
mootui ~/Desktop/dump                    # open a folder straight away
mootui --plan /Volumes/SD/DCIM           # dry run: show what would happen
mootui --yes [--move] [--verify] SOURCE  # import without the UI (copies by default)
mootui --yes --second-copy DIR SOURCE    # ...and write a verified second copy to DIR
mootui --tidy [--yes] [--trash]          # sort loose files already in the library
mootui --reorganize [--yes]              # re-sort the whole library to the layout
mootui --undo                            # undo the last tidy/reorganize
mootui --index                           # build or refresh the catalog only
mootui --library DIR ...                 # use a different library for this run
```

### Keyboard shortcuts

| Key                     | Action                                               |
| ----------------------- | ---------------------------------------------------- |
| `j` / `k`           | move down / up                                       |
| `h` / `l` / `tab` | switch list                                          |
| `g` / `G`           | top / bottom                                         |
| `enter`               | start                                                |
| `m`                   | toggle copy ↔ move                                  |
| `d`                   | toggle suspected ↔ content-verified duplicate check |
| `t`                   | send loose duplicates to Trash (tidy)                |
| `e`                   | eject the card                                       |
| `u`                   | undo                                                 |
| `s`                   | settings                                             |
| `L`                   | change library                                       |
| `?`                   | help                                                 |
| `q`                   | quit / cancel                                        |

## How duplicate detection works

- **Suspected** (default, same rule as Lightroom): same original filename, size and capture time as a file already in the catalog. Instant; the card is barely read.
- **Content-verified**: identical content, even if the file was renamed. Checks run in stages, from size to a 128 KB quick hash to a full hash, and the full hash runs only when the quick hash matches. Library hashes are computed once and stored.

Duplicates are never copied or deleted during an import. A **conflict** is a destination name already taken by a different file; it is skipped, never overwritten.

## Folder layout

Set `layout` in `<library>/.mootui/library.toml`, or cycle through presets in Settings:

| Layout                                                   | Example                            |
| -------------------------------------------------------- | ---------------------------------- |
| `{year}-{month}/{kind}/{name}` (default)               | `2026-10/RAW/DSC_0412.NEF`       |
| `{year}/{year}-{month}-{day}/{name}` (Lightroom style) | `2026/2026-10-13/DSC_0412.NEF`   |
| `{camera}/{year}/{year}-{month}/{name}`                | `X-T5/2026/2026-10/DSCF0412.RAF` |

Available fields: `{year} {month} {day} {month_name} {kind} {camera} {ext} {stem} {name}`. An empty segment is dropped (videos have no `{kind}`, for example).

Files that share a base name (such as `DSC_0412.NEF` and `DSC_0412.JPG`) are dated from the RAW file and stay together. Sidecars (`.xmp`, `.pp3`, …) always go next to their photo.

## Tidy vs. Reorganize

- **Tidy** moves only *loose* files (in the library root or in folders that don't match the layout) and detached sidecars. Folders that are already sorted are left alone, so references from photo editors stay valid.
- **Reorganize** re-sorts the entire library to the current layout. mootui warns you before it starts.

Both only rename files, and both are logged and undoable (`u` or `--undo`). Duplicates inside the library are reported. With "trash duplicates" on, *loose* duplicates go to the macOS Trash, and undo brings them back.

mootui always skips `.mootui`, hidden folders, macOS packages (`*.photoslibrary`, `*.lrdata`, …) and anything listed under `exclude` in `library.toml`.

## Configuration and data files

| Path                               | Contents                                                                |
| ---------------------------------- | ----------------------------------------------------------------------- |
| `~/.config/mootui/config.toml`   | per-machine settings: default library, copy/move, extra backup folder, second copy folder |
| `<library>/.mootui/library.toml` | settings that travel with the library: layout, duplicate rule, excludes |
| `<library>/.mootui/catalog.db`   | SQLite index plus import and move history                               |
| `<library>/.mootui/backups/`     | the last 5 catalog snapshots, taken after every run                     |
| `<library>/LOG/`                 | transfer, tidy and undo logs, plus a CSV of duplicates for each run     |

The catalog is refreshed with a quick metadata scan before every import or tidy. To rebuild it, delete `.mootui/catalog.db`. Import history and original filenames can't be rebuilt, which is why mootui keeps backups. You can set an extra backup folder (for example in Google Drive or Dropbox) in Settings. Finished backups are safe to sync; the live catalog is not, so mootui refuses to use a library inside a cloud-synced folder.

### Second copy

The extra backup folder holds catalog backups only. To keep a second copy of the photos themselves, set **Second copy** in Settings (or pass `--second-copy DIR`; `--no-second-copy` skips it for one run). Every import then also writes each new file to that folder, using the same paths as in the library:

- The card is still read only once. Each chunk goes to both disks, and each copy is verified separately.
- The import won't start if the folder is missing (for example, the drive isn't connected), overlaps the library or the source, or either disk lacks space for the import.
- If a second copy fails, the file stays imported and the failure is reported as a warning. In move mode, a source file is deleted only after both copies are verified and flushed to disk.
- A file already in the second copy with identical content counts as copied. One with different content is left alone and reported.
- The second copy has no catalog of its own. Tidy and Reorganize change only the library, so afterwards the second copy keeps the old layout.

## Performance

mootui copies as fast as Finder, `cp` and `ditto`, while doing more work than they do:

|                                                                                 | Finder / `cp` / `ditto` | mootui |
| ------------------------------------------------------------------------------- | :-------------------------: | :----: |
| Copy the files                                                                  |             ✓             |   ✓   |
| Read capture dates (EXIF) and sort into dated folders                           |                            |   ✓   |
| Skip files already in the library (duplicate check)                             |                            |   ✓   |
| Hash every file with BLAKE3 as it streams                                       |                            |   ✓   |
| Re-read each copy and check it against the source                               |                            |   ✓   |
| Write to a temp file, rename into place only after verifying (never overwrites) |                            |   ✓   |
| Record each file in the catalog, and log the run                                |                            |   ✓   |

### Benchmarks

Apple M2 MacBook Air, macOS 27.2, copying to the internal SSD, 3 runs per tool. mootui's time is the whole import, including planning (dates and duplicate check) and verification.

**SD card in a USB 2.0 hub reader** (SanDisk SDXC UHS-I card, 174 MB, 10 files; file cache purged before every run, so every run reads the card):

| Tool             |             Time |               Speed |
| ---------------- | ---------------: | ------------------: |
| `ditto`        |           5.50 s |           31.6 MB/s |
| `cp -R`        |           5.56 s |           31.2 MB/s |
| **mootui** | **5.63 s** | **30.8 MB/s** |
| Finder           |           6.10 s |           28.4 MB/s |

Every tool hits the reader's USB 2.0 limit (about 30–35 MB/s in practice), and mootui stays within about 2% of the fastest. If your imports run around 30–40 MB/s, check the reader before the card: in System Information → USB, the reader should show a link speed of 5 Gb/s or more, not 480 Mb/s.

**Fast source** (190 photos, 2.1 GB of JPEG and RAF, on a separate disk-image volume, so files are really copied rather than cloned within APFS). This removes the card as the bottleneck and shows the overhead of each tool. The cache could not be purged for this test, so absolute speeds are higher than a real card would give; the comparison between tools still holds.

| Tool             |            Time |               Speed |
| ---------------- | --------------: | ------------------: |
| Finder           |      4.0–4.2 s |           ~490 MB/s |
| **mootui** | **4.1 s** | **~500 MB/s** |
| `cp -R`        |           4.6 s |           ~450 MB/s |
| `ditto`        |      5.0–5.9 s |           ~400 MB/s |
| `rsync -a`     |          11.5 s |           ~176 MB/s |

Finder's first run took 7.7 s; the range above is its two later runs.

To run the card benchmark on your own Mac and card:

```sh
cargo build --release
sudo ./bench-copy.sh [/Volumes/CARD/DCIM] [runs]
```

`sudo` is only used for `purge`, which clears the file cache between runs; the copies run as you. The card is only read, and the copies go to a temporary folder that is deleted afterwards.

### How it stays fast

An import is a three-stage pipeline, so the card is read without pauses:

1. **Reader thread** reads the card sequentially (a card is fastest read in order) into an 8 × 8 MB queue.
2. **Writer** hashes each chunk with BLAKE3 as it arrives and writes it to a temporary file, so the source is read only once.
3. **Finisher thread** `fsync`s each copy, re-reads and verifies it, renames it into place and records it in the catalog, while the writer is already streaming the next file.

Other details:

- **Batched disk flushes.** Each copy gets an `fsync`, but the slow full flush of the drive's cache (`F_FULLFSYNC`) runs once per 64 files in move mode (before any of those originals are deleted) and once at the end of the run, not after every file. With a second copy, both drives are flushed this way, and the two copies are verified in parallel.
- **Batched catalog writes.** The catalog is written in transactions of 64 files.
- **Fast date reads.** Capture dates are read only for one file per base-name group, and only for files not already known to be duplicates. A built-in EXIF reader handles JPEG, TIFF-based raws and Fujifilm RAF from each file's first 64 KB. Anything it can't date (HEIC, CR3, video, dates only in XMP) goes to a single `exiftool -stay_open -fast2` process, started only when needed. On a 1,464-file folder, planning went from 7–11 s to 0.1 s.
- **Parallel library scan.** The quick scan that keeps the catalog up to date reads the library's folders and file metadata in parallel (100,000 files: ~115 ms instead of ~300 ms on an SSD). The card is still listed in order.
- **No async runtime.** mootui uses plain threads and channels, because the job is bulk I/O limited by disk throughput.

## FAQ

**Does mootui modify or delete files on my SD card?**
Not by default; it copies. In move mode (`m` or `--move`) it deletes a source file only after the copy (and the second copy, if set) has been verified and flushed to disk.

**Can I use it alongside Lightroom, Capture One or Apple Photos?**
Yes. mootui writes ordinary folders. Point your editor at the library, or import from it. Tidy leaves already-sorted folders untouched.

**Does it run on Linux or Windows?**
Not yet. Card detection, eject and Trash rely on macOS.

## Contributing

Issues and pull requests are welcome.

## License

[MIT](LICENSE) © 2026 Kartik Trivedi. You are free to use, modify and redistribute mootui, including commercially, as long as you keep the copyright and license notice.
