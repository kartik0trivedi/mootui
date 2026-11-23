//! The second copy: a folder (usually on another drive) that mirrors the
//! library's layout. Imports write to it as they go (transfer.rs); this
//! module checks and locks the folder, and catches it up with files the
//! library has but it lacks: after an interrupted import, after a failed
//! second copy, or when the second copy is turned on for an existing library.
//!
//! Catching up reads from the library, not the card. A file already in the
//! second copy counts as present when its size matches; one with a different
//! size is left alone and reported. Every new copy is verified like an
//! import's.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use anyhow::{bail, Result};
use chrono::Local;

use crate::catalog::Catalog;
use crate::config::LibraryConfig;
use crate::exif;
use crate::jobs::Progress;
use crate::library;
use crate::transfer::{copy_times, free_bytes, fsync};
use crate::util::{hash_file, human_bytes, now, tilde};

/// The folder exists and doesn't overlap the library or the source.
pub fn check_folder(second: &Path, root: &Path, source: Option<&Path>) -> Result<()> {
    if !second.is_dir() {
        bail!("Second copy folder not found: {} (is the drive connected?)", tilde(second));
    }
    let sc = fs::canonicalize(second)?;
    for (other, what) in [(Some(root), "library"), (source, "source")] {
        let Some(other) = other else { continue };
        let o = fs::canonicalize(other)?;
        if sc.starts_with(&o) || o.starts_with(&sc) {
            bail!("The second copy folder {} overlaps the {what} ({})", tilde(second), tilde(other));
        }
    }
    Ok(())
}

/// Hold while writing to the second copy, so two runs (say, from two
/// libraries pointed at the same drive) never write there at once. That also
/// makes it safe to clear a partial file left by a run that crashed.
pub fn lock(second: &Path) -> Result<File> {
    let f = File::create(second.join(".mootui-lock"))?;
    if f.try_lock().is_err() {
        bail!("Another mootui is writing to the second copy folder {}", tilde(second));
    }
    Ok(f)
}

#[derive(Default)]
pub struct CatchUp {
    pub copied: usize,
    pub bytes: u64,
    pub present: usize,
    /// Relative paths where the second copy has a different file.
    pub different: Vec<String>,
    /// Relative paths that could not be copied, with the reason.
    pub failed: Vec<(String, String)>,
    pub cancelled: bool,
}

/// Make sure each of `rels` (paths relative to the library) is in the second
/// copy. `each` is told about every file copied, as (rel, bytes so far).
pub fn catch_up(root: &Path, second: &Path, rels: &[String], cancel: &dyn Fn() -> bool, each: &mut dyn FnMut(&str, u64)) -> Result<CatchUp> {
    let mut r = CatchUp::default();
    let mut todo: Vec<(&String, u64)> = Vec::new();
    for rel in rels {
        let Ok(src) = fs::metadata(root.join(rel)) else { continue }; // gone from the library
        match fs::symlink_metadata(second.join(rel)) {
            Ok(m) if m.len() == src.len() => r.present += 1,
            Ok(_) => r.different.push(rel.clone()),
            Err(_) => todo.push((rel, src.len())),
        }
    }
    let need: u64 = todo.iter().map(|(_, n)| n).sum();
    if let Some(free) = free_bytes(second) {
        if need > free {
            bail!("Not enough space for the second copy on the disk holding {}: need {}, {} free", tilde(second), human_bytes(need), human_bytes(free));
        }
    }

    let mut last: Option<File> = None;
    let mut buf = if todo.is_empty() { Vec::new() } else { vec![0u8; 8 << 20] };
    for (rel, size) in todo {
        if cancel() {
            r.cancelled = true;
            break;
        }
        match copy_one(&root.join(rel), &second.join(rel), &mut buf) {
            Ok(f) => {
                last = Some(f);
                r.copied += 1;
                r.bytes += size;
                each(rel, r.bytes);
            }
            Err(e) => r.failed.push((rel.clone(), e)),
        }
    }
    // One full flush of the drive's cache covers every copy fsync'd before it.
    if let Some(f) = last {
        f.sync_all()?;
    }
    Ok(r)
}

/// Copy, fsync, verify by re-reading, then rename into place.
fn copy_one(src: &Path, dest: &Path, buf: &mut [u8]) -> Result<File, String> {
    let dir = dest.parent().ok_or("bad destination")?;
    fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.mootui-partial"));
    let _ = fs::remove_file(&tmp); // left by a crashed run; the lock rules out a live one
    let r = (|| {
        let mut out = OpenOptions::new().write(true).create_new(true).open(&tmp).map_err(|e| format!("create: {e}"))?;
        let mut inp = File::open(src).map_err(|e| format!("open: {e}"))?;
        let mut h = blake3::Hasher::new();
        loop {
            let n = inp.read(buf).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            out.write_all(&buf[..n]).map_err(|e| format!("write: {e}"))?;
        }
        copy_times(src, &out);
        fsync(&out).map_err(|e| format!("sync: {e}"))?;
        if hash_file(&tmp).map_err(|e| format!("verify: {e}"))? != *h.finalize().as_bytes() {
            return Err("verification failed: copy does not match the library file".to_string());
        }
        if dest.exists() {
            return Err("exists at destination".into());
        }
        fs::rename(&tmp, dest).map_err(|e| format!("rename: {e}"))?;
        Ok(out)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// The "Catch up second copy" action: every file in the library.
pub fn catch_up_library(root: &Path, cfg: &LibraryConfig, second: &Path, p: &Progress) -> Result<String> {
    check_folder(second, root, None)?;
    let _lock = lock(second)?;
    let cat = Catalog::open(root)?;
    library::sync(&cat, cfg, &mut exif::Reader::new(), p)?;
    let mut rels: Vec<String> = cat.present()?.into_iter().map(|r| r.rel).collect();
    rels.sort();

    let dir = root.join("LOG");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}_second-copy.log", Local::now().format("%Y-%m-%d_%H%M%S")));
    let mut log = BufWriter::new(File::create(&path)?);
    writeln!(log, "mootui second copy catch-up")?;
    writeln!(log, "started    {}", now())?;
    writeln!(log, "library    {}", root.display())?;
    writeln!(log, "2nd copy   {}\n", second.display())?;

    p.phase(format!("Catching up the second copy ({} library files)", rels.len()), rels.len());
    let mut n = 0;
    let r = catch_up(root, second, &rels, &|| p.cancelled(), &mut |rel, _| {
        n += 1;
        p.tick(n);
        let _ = writeln!(log, "{}  COPIED     {rel}", now());
    })?;
    for rel in &r.different {
        writeln!(log, "{}  DIFFERENT  {rel}  (a different file is already in the second copy; left alone)", now())?;
    }
    for (rel, e) in &r.failed {
        writeln!(log, "{}  FAILED     {rel}  ({e})", now())?;
    }
    let msg = summary(&r);
    writeln!(log, "\nfinished   {}\nresult     {msg}", now())?;
    log.flush()?;
    Ok(format!("{msg} — log {}", tilde(&path)))
}

pub fn summary(r: &CatchUp) -> String {
    let mut s = format!("Second copy: {} copied ({}), {} already there", r.copied, human_bytes(r.bytes), r.present);
    if !r.different.is_empty() {
        s += &format!(", {} different (left alone)", r.different.len());
    }
    if !r.failed.is_empty() {
        s += &format!(", {} failed", r.failed.len());
    }
    if r.cancelled {
        s += ", CANCELLED";
    }
    s
}
