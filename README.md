# mootui — photo importer for the terminal (macOS)

**mootui** is a fast, keyboard-driven terminal app (TUI) for **importing photos
and videos from a camera SD card** or any folder into a dated photo library. It
**skips duplicates**, **organizes files into folders by date**, keeps RAW + JPEG
pairs and sidecars together, and can **tidy an existing messy photo folder**.
Every move is logged and can be undone.

Think of it as the import step of Lightroom, Photos or digiKam, in a
single small binary that you run from the terminal. It does no editing and no
cataloguing UI, and it leaves your files as plain folders that any app can read.

![mootui screenshot](docs/screenshot.png)

## Why mootui?

- **No duplicate imports.** Re-insert the same card as often as you like; only
  new shots are copied. Duplicates are detected by filename, size and capture
  time (as Lightroom does) or, optionally, by file content even after renaming.
- **Date-based folder structure.** Photos land in `2026-10/RAW/DSC_0412.NEF`,
  `2026/2026-10-13/…`, `Fujifilm X-T5/2026/…` or whatever layout you define.
- **Safe by design.** Copies are verified before they are put in place. Nothing
  is ever overwritten. Duplicates are never deleted on import. Tidy and
  reorganize runs can be undone.
- **Fast.** Streams large cards with read-ahead buffering, hashes with BLAKE3, and
  reads EXIF dates through a single persistent `exiftool` process.
- **Your files stay the source of truth.** The SQLite catalog is only an index:
  files added or deleted in Finder are picked up automatically, and the catalog
  can be rebuilt from scratch.
- **Works with your editor.** Folders you have already sorted are left alone, so
  Lightroom Classic, Capture One, darktable, RawTherapee and digiKam keep
  finding their files.
- **Scriptable.** Every action is also available as a non-interactive command.

Supported formats include RAW (`.NEF`, `.CR2`, `.CR3`, `.ARW`, `.RAF`, `.DNG`,
`.ORF`, `.RW2`, …), JPEG, HEIC/HEIF, PNG, TIFF, WebP, video (`.MOV`, `.MP4`,
`.MTS`, `.INSV`, …) and sidecars (`.XMP`, `.PP3`, `.AAE`, `.DOP`, …).

## Installation

mootui runs on **macOS**. It uses `diskutil` to detect and eject cards and the
macOS Trash for removed duplicates.

### Requirements

- [Rust](https://rustup.rs) (stable)
- [ExifTool](https://exiftool.org), recommended for accurate capture dates:
  `brew install exiftool`

### Install with cargo

```sh
cargo install --git https://github.com/OWNER/mootui
```

### Build from source

```sh
git clone https://github.com/OWNER/mootui
cd mootui
cargo build --release
cp target/release/mootui ~/.local/bin/   # or any directory on your PATH
mootui --help
```

The binary is self-contained. After installing it you can delete the source
folder; your catalog and logs live inside your photo library.

## Quick start

```sh
mootui
```

Insert a card. It appears in the list. Select it, review what will be imported
(new files, duplicates and conflicts), and press `enter`. When it finishes,
press `e` to eject the card.

Your library defaults to `~/Pictures`. Press `L` to choose another one.

## Usage

```sh
mootui                                   # interactive: pick a card, folder or Tidy
mootui ~/Desktop/dump                    # open a folder straight away
mootui --plan /Volumes/SD/DCIM           # dry run: show what would happen
mootui --yes [--move] [--verify] SOURCE  # import without the UI (copies by default)
mootui --tidy [--yes] [--trash]          # sort loose files already in the library
mootui --reorganize [--yes]              # re-sort the whole library to the layout
mootui --undo                            # undo the last tidy/reorganize
mootui --index                           # build or refresh the catalog only
mootui --library DIR ...                 # use a different library for this run
```

### Keyboard shortcuts

| Key | Action |
|---|---|
| `j` / `k` | move down / up |
| `h` / `l` / `tab` | switch list |
| `g` / `G` | top / bottom |
| `enter` | start |
| `m` | toggle copy ↔ move |
| `d` | toggle suspected ↔ content-verified duplicate check |
| `t` | send loose duplicates to Trash (tidy) |
| `e` | eject the card |
| `u` | undo |
| `s` | settings |
| `L` | change library |
| `?` | help |
| `q` | quit / cancel |

## How duplicate detection works

- **Suspected** (default, same rule as Lightroom): same original filename,
  size and capture time as a file already in the catalog. Instant; the card is
  barely read.
- **Content-verified**: identical content, even if the file was renamed. Checks
  run in stages, from size to a 128 KB quick hash to a full hash, and the full
  hash runs only when the quick hash matches. Library hashes are computed once
  and stored.

Duplicates are never copied or deleted during an import. A **conflict** is a
destination name already taken by a different file; it is skipped, never
overwritten.

## Folder layout

Set `layout` in `<library>/.mootui/library.toml`, or cycle through presets in
Settings:

| Layout | Example |
|---|---|
| `{year}-{month}/{kind}/{name}` (default) | `2026-10/RAW/DSC_0412.NEF` |
| `{year}/{year}-{month}-{day}/{name}` (Lightroom style) | `2026/2026-10-13/DSC_0412.NEF` |
| `{camera}/{year}/{year}-{month}/{name}` | `X-T5/2026/2026-10/DSCF0412.RAF` |

Available fields: `{year} {month} {day} {month_name} {kind} {camera} {ext} {stem} {name}`.
An empty segment is dropped (videos have no `{kind}`, for example).

Files that share a base name (such as `DSC_0412.NEF` and `DSC_0412.JPG`) are
dated from the RAW file and stay together. Sidecars (`.xmp`, `.pp3`, …) always
go next to their photo.

## Tidy vs. Reorganize

- **Tidy** moves only *loose* files (in the library root or in folders that
  don't match the layout) and detached sidecars. Folders that are already
  sorted are left alone, so references from photo editors stay valid.
- **Reorganize** re-sorts the entire library to the current layout. mootui
  warns you before it starts.

Both only rename files, and both are logged and undoable (`u` or `--undo`).
Duplicates inside the library are reported. With "trash duplicates" on,
*loose* duplicates go to the macOS Trash, and undo brings them back.

mootui always skips `.mootui`, hidden folders, macOS packages
(`*.photoslibrary`, `*.lrdata`, …) and anything listed under `exclude` in
`library.toml`.

## Configuration and data files

| Path | Contents |
|---|---|
| `~/.config/mootui/config.toml` | per-machine settings: default library, copy/move, extra backup folder |
| `<library>/.mootui/library.toml` | settings that travel with the library: layout, duplicate rule, excludes |
| `<library>/.mootui/catalog.db` | SQLite index plus import and move history |
| `<library>/.mootui/backups/` | the last 5 catalog snapshots, taken after every run |
| `<library>/LOG/` | transfer, tidy and undo logs, plus a CSV of duplicates for each run |

The catalog is refreshed with a quick metadata scan before every import or
tidy. To rebuild it, delete `.mootui/catalog.db`. Import history and original
filenames can't be rebuilt, which is why mootui keeps backups. You can set an
extra backup folder (for example in Google Drive or Dropbox) in Settings.
Finished backups are safe to sync; the live catalog is not, so mootui refuses
to use a library inside a cloud-synced folder.

## Performance

When copying, one reader thread streams the card into a 4 × 8 MB queue while
the main thread hashes and writes. Each copy is re-read and verified before it
is renamed into place. A single `exiftool -stay_open -fast2` process reads
capture dates, only for one file per base-name group and only for files not
already known to be duplicates. mootui uses plain threads and channels instead
of an async runtime, because the job is bulk I/O limited by disk throughput.

## FAQ

**Does mootui modify or delete files on my SD card?**
Not by default; it copies. In move mode (`m` or `--move`) it deletes a source
file only after the copy has been verified.

**Can I use it alongside Lightroom, Capture One or Apple Photos?**
Yes. mootui writes ordinary folders. Point your editor at the library, or import
from it. Tidy leaves already-sorted folders untouched.

**Does it run on Linux or Windows?**
Not yet. Card detection, eject and Trash rely on macOS.

## Contributing

Issues and pull requests are welcome.

## License

[MIT](LICENSE) © 2026 Kartik Trivedi. You are free to use, modify and
redistribute mootui, including commercially, as long as you keep the copyright
and license notice.
