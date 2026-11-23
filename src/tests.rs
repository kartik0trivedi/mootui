use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

use crate::catalog::{Catalog, NewRow};
use crate::config::{DupMode, LibraryConfig};
use crate::exif;
use crate::jobs::Progress;
use crate::library;
use crate::plan::{self, Status};
use crate::second;
use crate::transfer::{self, Mode, Summary, XferMsg};

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("mootui-it-{label}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Minimal valid JPEG containing an EXIF APP1 block with Model + DateTimeOriginal,
/// plus optional trailing padding so tests can control exact file sizes/content.
fn make_jpeg(model: &str, dto: &str, payload: &[u8]) -> Vec<u8> {
    let mut model_b = model.as_bytes().to_vec();
    model_b.push(0);
    let mut dto_b = dto.as_bytes().to_vec();
    dto_b.push(0);

    let ifd0 = 8u32;
    let exif_ifd = ifd0 + 2 + 24 + 4;
    let data_off = exif_ifd + 2 + 12 + 4;

    let mut tiff = Vec::new();
    tiff.extend(b"II");
    tiff.extend(42u16.to_le_bytes());
    tiff.extend(ifd0.to_le_bytes());

    let mut blob = Vec::new();
    let put = |b: &[u8], blob: &mut Vec<u8>| -> u32 {
        let off = data_off + blob.len() as u32;
        blob.extend(b);
        off
    };

    // IFD0: Model (0x0110), ExifIFD (0x8769)
    tiff.extend(2u16.to_le_bytes());
    let mo = put(&model_b, &mut blob);
    tiff.extend(0x0110u16.to_le_bytes());
    tiff.extend(2u16.to_le_bytes());
    tiff.extend((model_b.len() as u32).to_le_bytes());
    tiff.extend(mo.to_le_bytes());

    tiff.extend(0x8769u16.to_le_bytes());
    tiff.extend(4u16.to_le_bytes());
    tiff.extend(1u32.to_le_bytes());
    tiff.extend(exif_ifd.to_le_bytes());
    tiff.extend(0u32.to_le_bytes());

    // EXIF IFD: DateTimeOriginal (0x9003)
    tiff.extend(1u16.to_le_bytes());
    let dto_off = put(&dto_b, &mut blob);
    tiff.extend(0x9003u16.to_le_bytes());
    tiff.extend(2u16.to_le_bytes());
    tiff.extend((dto_b.len() as u32).to_le_bytes());
    tiff.extend(dto_off.to_le_bytes());
    tiff.extend(0u32.to_le_bytes());
    tiff.extend(blob);

    let mut jpg = vec![0xFF, 0xD8, 0xFF, 0xE1];
    jpg.extend(((tiff.len() + 8) as u16).to_be_bytes());
    jpg.extend(b"Exif\0\0");
    jpg.extend(tiff);
    jpg.extend([0xFF, 0xDA, 0x00, 0x02]);
    jpg.extend(payload);
    jpg.extend([0xFF, 0xD9]);
    jpg
}

/// Minimal Fujifilm RAF wrapping an embedded JPEG with EXIF.
fn make_raf(model: &str, dto: &str, payload: &[u8]) -> Vec<u8> {
    let jpg = make_jpeg(model, dto, payload);
    let mut raf = b"FUJIFILMCCD-RAW 0201FF383501".to_vec();
    raf.resize(100, 0);
    raf[84..88].copy_from_slice(&100u32.to_be_bytes());
    raf.extend(jpg);
    raf
}

fn run_transfer(plan: plan::Plan, mode: Mode, second: Option<PathBuf>) -> Summary {
    let (tx, rx) = mpsc::channel();
    transfer::run(plan, mode, None, second, Arc::new(AtomicBool::new(false)), tx);
    for msg in rx {
        if let XferMsg::Done(res) = msg {
            return res.expect("transfer failed");
        }
    }
    panic!("transfer channel closed without Done");
}

#[test]
fn raw_jpeg_sidecar_import_and_dedup() {
    let tmp = TempDir::new("group-import");
    let src = tmp.path().join("card/DCIM");
    let lib = tmp.path().join("lib");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&lib).unwrap();

    fs::write(src.join("DSCF0001.RAF"), make_raf("X-T5", "2026:10:13 14:30:22", b"raw-bytes")).unwrap();
    fs::write(src.join("DSCF0001.JPG"), make_jpeg("X-T5", "2026:10:13 14:30:23", b"jpg-bytes")).unwrap();
    fs::write(src.join("DSCF0001.RAF.pp3"), b"[Exposure]\nCompensation=0.5\n").unwrap();
    fs::write(src.join("DSCF0001.xmp"), b"<x:xmpmeta/>").unwrap();

    let cfg = LibraryConfig::default();
    let p = plan::plan_import(&src, &lib, &cfg, DupMode::Suspected, None, &Progress::silent()).unwrap();
    assert_eq!(p.new_items().count(), 4);
    assert_eq!(p.duplicates().count(), 0);
    assert!(p.scan_errors.is_empty());

    let s = run_transfer(p, Mode::Copy, None);
    assert_eq!(s.transferred, 4);
    assert_eq!(s.failed, 0);
    assert!(lib.join("2026-10/RAW/DSCF0001.RAF").exists());
    assert!(lib.join("2026-10/JPG/DSCF0001.JPG").exists());
    assert!(lib.join("2026-10/RAW/DSCF0001.RAF.pp3").exists());
    assert!(lib.join("2026-10/RAW/DSCF0001.xmp").exists());

    // Re-importing the same card skips all 4 files as duplicates.
    let p2 = plan::plan_import(&src, &lib, &cfg, DupMode::Suspected, None, &Progress::silent()).unwrap();
    assert_eq!(p2.new_items().count(), 0);
    assert_eq!(p2.duplicates().count(), 4);
}

#[test]
fn suspected_mode_missing_date_verifies_content() {
    let tmp = TempDir::new("missing-date-dup");
    let src = tmp.path().join("card");
    let lib = tmp.path().join("lib");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(lib.join("2025-01/JPG")).unwrap();

    // Put an existing file in the library and record it in the catalog with capture = None.
    let existing_bytes = make_jpeg("X-T5", "2025:01:02 10:00:00", b"AAAAAAAA");
    let existing_rel = "2025-01/JPG/IMG_0001.JPG";
    let existing_abs = lib.join(existing_rel);
    fs::write(&existing_abs, &existing_bytes).unwrap();
    let m = fs::metadata(&existing_abs).unwrap();

    let cat = Catalog::open(&lib).unwrap();
    cat.upsert(&NewRow {
        rel: existing_rel,
        size: m.len(),
        mtime: library::mtime_of(&m),
        name: "IMG_0001.JPG",
        capture: None,
        date_src: None,
        camera: None,
        head: None,
        full: None,
        import_id: None,
    })
    .unwrap();
    drop(cat);

    // 1) Different content, exact same filename and byte size -> MUST NOT be marked duplicate.
    let different_bytes = make_jpeg("X-T5", "2026:10:13 10:00:00", b"BBBBBBBB");
    assert_eq!(existing_bytes.len(), different_bytes.len());
    fs::write(src.join("IMG_0001.JPG"), &different_bytes).unwrap();

    let cfg = LibraryConfig::default();
    let p = plan::plan_import(&src, &lib, &cfg, DupMode::Suspected, None, &Progress::silent()).unwrap();
    assert_eq!(p.new_items().count(), 1, "different file of same size/name must not be skipped when catalog date is missing");
    assert_eq!(p.duplicates().count(), 0);

    // 2) Identical content -> SHOULD be caught as a content duplicate even in suspected mode.
    fs::write(src.join("IMG_0001.JPG"), &existing_bytes).unwrap();
    let p2 = plan::plan_import(&src, &lib, &cfg, DupMode::Suspected, None, &Progress::silent()).unwrap();
    assert_eq!(p2.new_items().count(), 0);
    assert_eq!(p2.duplicates().count(), 1);
    assert!(matches!(p2.items[0].status, Status::Duplicate { rule: "content", .. }));
}

#[test]
fn scan_errors_are_surfaced() {
    let tmp = TempDir::new("scan-errors");
    let src = tmp.path().join("card");
    let locked = src.join("unreadable");
    let lib = tmp.path().join("lib");
    fs::create_dir_all(&locked).unwrap();
    fs::create_dir_all(&lib).unwrap();

    fs::write(src.join("OK.JPG"), make_jpeg("X-T5", "2026:10:13 14:30:22", b"ok")).unwrap();
    fs::write(locked.join("HIDDEN.JPG"), make_jpeg("X-T5", "2026:10:13 14:30:22", b"no")).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let cfg = LibraryConfig::default();
    let p = plan::plan_import(&src, &lib, &cfg, DupMode::Suspected, None, &Progress::silent()).unwrap();

    // Restore permissions right away so TempDir cleanup always succeeds.
    let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));

    assert_eq!(p.new_items().count(), 1);
    assert!(!p.scan_errors.is_empty(), "expected unreadable directory error to be recorded");

    let s = run_transfer(p, Mode::Copy, None);
    assert_eq!(s.transferred, 1);
    assert!(s.scan_errors >= 1);
    let log = fs::read_to_string(&s.log_path).unwrap();
    assert!(log.contains("SCAN ERR"));
}

#[test]
fn move_mode_keeps_whole_group_when_companion_fails() {
    let tmp = TempDir::new("move-group-safe");
    let src = tmp.path().join("card");
    let lib = tmp.path().join("lib");
    let sec = tmp.path().join("second");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&lib).unwrap();
    fs::create_dir_all(sec.join("2026-10/RAW")).unwrap();

    // Group 1: DSCF0002.RAF + DSCF0002.JPG (second copy of RAF will fail due to conflict in second copy)
    let raf_src = src.join("DSCF0002.RAF");
    let jpg_src = src.join("DSCF0002.JPG");
    fs::write(&raf_src, make_raf("X-T5", "2026:10:13 14:30:22", b"raf-2")).unwrap();
    fs::write(&jpg_src, make_jpeg("X-T5", "2026:10:13 14:30:22", b"jpg-2")).unwrap();
    // Pre-populate a different file at the second-copy path for the RAF so its second copy fails.
    fs::write(sec.join("2026-10/RAW/DSCF0002.RAF"), b"corrupt-or-different-file").unwrap();

    // Group 2: DSCF0003.JPG (independent photo, should succeed and be deleted from source)
    let ok_src = src.join("DSCF0003.JPG");
    fs::write(&ok_src, make_jpeg("X-T5", "2026:10:13 14:31:00", b"jpg-3")).unwrap();

    let cfg = LibraryConfig::default();
    let p = plan::plan_import(&src, &lib, &cfg, DupMode::Suspected, None, &Progress::silent()).unwrap();
    assert_eq!(p.new_items().count(), 3);

    let s = run_transfer(p, Mode::Move, Some(sec.clone()));
    assert_eq!(s.transferred, 3);
    assert_eq!(s.second_failed, 1);

    // Group 1: BOTH source files must remain on the card because the RAF's second copy failed.
    assert!(raf_src.exists(), "RAF source should be kept when its second copy failed");
    assert!(jpg_src.exists(), "JPG companion source should be kept when its group's RAF failed");

    // Group 2: succeeded completely -> source deleted, present in both library and second copy.
    assert!(!ok_src.exists(), "independent photo whose copies succeeded should be removed from source");
    assert!(lib.join("2026-10/JPG/DSCF0003.JPG").exists());
    assert!(sec.join("2026-10/JPG/DSCF0003.JPG").exists());
}

#[test]
fn interrupted_import_and_second_copy_catch_up() {
    let tmp = TempDir::new("interrupted-catchup");
    let lib = tmp.path().join("lib");
    let sec = tmp.path().join("second");
    fs::create_dir_all(lib.join("2026-10/JPG")).unwrap();
    fs::create_dir_all(&sec).unwrap();

    // Seed a library photo and a leftover partial copy from a crashed run.
    fs::write(lib.join("2026-10/JPG/A.JPG"), make_jpeg("X-T5", "2026:10:13 12:00:00", b"photo-a")).unwrap();
    let partial = lib.join("2026-10/JPG/.B.JPG.mootui-partial");
    fs::write(&partial, b"half-written").unwrap();

    let cat = Catalog::open(&lib).unwrap();
    let _unfinished = cat.begin_import("import", "/Volumes/SD/DCIM", Some(("SD", "UUID-1")), "copy", "{year}-{month}/{kind}/{name}").unwrap();
    let interrupted = cat.close_interrupted().unwrap();
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0].volume.as_deref(), Some("SD"));
    // Reported only once.
    assert!(cat.close_interrupted().unwrap().is_empty());

    // Library sync cleans up leftover partial files.
    let cfg = LibraryConfig::default();
    let stats = library::sync(&cat, &cfg, &mut exif::Reader::new(), &Progress::silent()).unwrap();
    assert_eq!(stats.total, 1);
    assert!(!partial.exists(), "library::sync should remove leftover .mootui-partial files");
    drop(cat);

    // Second copy catch-up copies the library file into the second copy.
    let msg = second::catch_up_library(&lib, &cfg, &sec, &Progress::silent()).unwrap();
    assert!(msg.contains("1 copied"), "unexpected catch-up summary: {msg}");
    assert!(sec.join("2026-10/JPG/A.JPG").exists());
}
