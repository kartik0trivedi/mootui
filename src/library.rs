//! Walking the library and keeping the catalog in step with it.
//!
//! sync() is the cheap reconcile run before every import and tidy: it stats
//! every media file and only does real work (EXIF) for files that are new or
//! changed since last time. Files removed outside mootui are marked missing;
//! files added outside mootui (Finder, digiKam) are adopted.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Result};
use rayon::prelude::*;
use walkdir::WalkDir;

use crate::catalog::{Catalog, NewRow, Row};
use crate::config::LibraryConfig;
use crate::exif;
use crate::jobs::Progress;
use crate::media;
use crate::util::{hash_file, hash_head, nfc};

/// macOS package folders: apps' private libraries that look like folders.
const PACKAGE_EXTS: &[&str] = &[
    "photoslibrary", "photolibrary", "migratedphotolibrary", "aplibrary", "lrdata", "lrlibrary", "app", "bundle",
];

/// mtimes on exFAT/FAT cards are coarse; allow a little slack.
const MTIME_SLACK: i64 = 2;

pub struct DiskFile {
    pub rel: String,
    pub abs: PathBuf,
    pub size: u64,
    pub mtime: i64,
}

pub fn mtime_of(m: &fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn is_excluded_dir(name: &str, cfg: &LibraryConfig) -> bool {
    if name.starts_with('.') {
        return true;
    }
    let ext = media::ext_lower(name);
    if PACKAGE_EXTS.contains(&ext.as_str()) {
        return true;
    }
    cfg.exclude.iter().any(|pat| match pat.strip_prefix("*.") {
        Some(e) => ext.eq_ignore_ascii_case(e),
        None => pat == name,
    })
}

pub fn walk(root: &Path, cfg: &LibraryConfig) -> Vec<DiskFile> {
    let mut out = Vec::new();
    let it = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
        e.depth() == 0 || !(e.file_type().is_dir() && is_excluded_dir(&e.file_name().to_string_lossy(), cfg))
    });
    for e in it.filter_map(Result::ok) {
        if !e.file_type().is_file() {
            continue;
        }
        let name = e.file_name().to_string_lossy();
        if name.starts_with('.') || !media::is_media(&media::ext_lower(&name)) {
            continue;
        }
        let Ok(rel) = e.path().strip_prefix(root) else { continue };
        let Ok(m) = e.metadata() else { continue };
        out.push(DiskFile { rel: rel.to_string_lossy().into_owned(), abs: e.path().to_path_buf(), size: m.len(), mtime: mtime_of(&m) });
    }
    out
}

pub struct SyncStats {
    pub added: usize,
    pub missing: usize,
    pub total: usize,
}

pub fn sync(cat: &Catalog, cfg: &LibraryConfig, exif: &mut exif::Reader, p: &Progress) -> Result<SyncStats> {
    p.phase("Checking library for changes", 0);
    let disk = walk(&cat.root, cfg);
    let known: HashMap<String, (i64, u64, i64, bool)> =
        cat.all_stat()?.into_iter().map(|(id, rel, size, mtime, present)| (rel, (id, size, mtime, present))).collect();

    let mut changed: Vec<&DiskFile> = Vec::new();
    let tx = cat.conn.unchecked_transaction()?;
    let mut seen = std::collections::HashSet::new();
    for f in &disk {
        seen.insert(f.rel.as_str());
        match known.get(&f.rel) {
            Some(&(id, size, mtime, present)) if size == f.size && (mtime - f.mtime).abs() <= MTIME_SLACK => {
                if !present {
                    cat.set_state(id, true)?;
                }
            }
            _ => changed.push(f),
        }
    }
    let mut missing = 0;
    for (rel, &(id, _, _, present)) in &known {
        if present && !seen.contains(rel.as_str()) {
            cat.set_state(id, false)?;
            missing += 1;
        }
    }
    tx.commit()?;

    if !changed.is_empty() {
        // Sidecars carry no useful date of their own.
        let to_date: Vec<PathBuf> =
            changed.iter().filter(|f| !media::is_sidecar(&media::ext_lower(&f.rel))).map(|f| f.abs.clone()).collect();
        p.phase(format!("Cataloguing {} new or changed file(s)", changed.len()), to_date.len());
        let metas = exif::read_all(exif, &to_date, |n| p.tick(n), &|| p.cancelled());
        if p.cancelled() {
            bail!("cancelled");
        }

        let tx = cat.conn.unchecked_transaction()?;
        for f in &changed {
            let m = metas.get(&f.abs);
            let name = nfc(f.abs.file_name().map(|n| n.to_string_lossy()).unwrap_or_default().as_ref());
            cat.upsert(&NewRow {
                rel: &f.rel,
                size: f.size,
                mtime: f.mtime,
                name: &name,
                capture: m.and_then(|m| m.capture.as_deref()),
                date_src: m.map(|m| m.src.as_str()),
                camera: m.and_then(|m| m.camera.as_deref()),
                head: None,
                full: None,
                import_id: None,
            })?;
        }
        tx.commit()?;
    }

    Ok(SyncStats { added: changed.len(), missing, total: disk.len() })
}

/// Fill in missing hashes for library rows, in parallel (the library is on a
/// local SSD; unlike a card, it reads fastest with many readers). Results are
/// stored, so each library file is hashed at most once, ever.
pub fn ensure_hashes(cat: &Catalog, rows: &mut [Row], full: bool, p: &Progress) -> Result<()> {
    let root = cat.root.clone();
    let todo: Vec<usize> =
        (0..rows.len()).filter(|&i| if full { rows[i].full.is_none() } else { rows[i].head.is_none() }).collect();
    if todo.is_empty() {
        return Ok(());
    }
    let done = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<(usize, Option<[u8; 32]>)> = todo
        .par_iter()
        .map(|&i| {
            if p.cancelled() {
                return (i, None);
            }
            let path = root.join(&rows[i].rel);
            let h = if full { hash_file(&path) } else { hash_head(&path) };
            let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n % 16 == 0 {
                p.tick(n);
            }
            (i, h.ok())
        })
        .collect();
    let tx = cat.conn.unchecked_transaction()?;
    for (i, h) in results {
        let Some(h) = h else { continue };
        if full {
            cat.set_hashes(rows[i].id, None, Some(&h))?;
            rows[i].full = Some(h.to_vec());
        } else {
            cat.set_hashes(rows[i].id, Some(&h), None)?;
            rows[i].head = Some(h.to_vec());
        }
    }
    tx.commit()?;
    if p.cancelled() {
        bail!("cancelled");
    }
    Ok(())
}
