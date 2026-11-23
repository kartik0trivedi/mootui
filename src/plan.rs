//! Planning: decide, before anything moves, what each file will do.
//!
//! Import (card/folder -> library):
//!   1. bring the catalog up to date (cheap: stat only, EXIF for new files)
//!   2. list the source
//!   3. duplicates FIRST, so known files are never dated or read in full:
//!      - suspected: original name + size + capture time (Lightroom's rule)
//!      - verified: size -> 128 KB quick hash -> full hash, staged; library
//!        hashes come from the catalog, computed once ever
//!   4. capture dates for what is left, one file per basename group
//!   5. route through the layout template; sidecars follow their photo
//!
//! Tidy / Reorganize (library -> library):
//!   loose files (and detached sidecars) are routed into the layout; with
//!   Reorganize, already-sorted files are re-routed too. Content duplicates
//!   inside the library are reported, never moved.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use walkdir::WalkDir;

use crate::catalog::{Catalog, Row};
use crate::config::{DupMode, LibraryConfig};
use crate::exif::{self, Meta};
use crate::jobs::Progress;
use crate::layout::{self, Fields};
use crate::library::{self, mtime_of};
use crate::media;
use crate::util::{hash_file, hash_head, hex, nfc};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Import,
    Tidy,
    Reorganize,
}

impl Kind {
    pub fn key(self) -> &'static str {
        match self {
            Kind::Import => "import",
            Kind::Tidy => "tidy",
            Kind::Reorganize => "reorganize",
        }
    }
}

#[derive(Clone)]
pub enum Status {
    /// Import: copy in. Tidy/Reorganize: move here.
    New,
    Duplicate { existing: PathBuf, rule: &'static str, hash: Option<String>, trashable: bool },
    Conflict { reason: String },
}

#[derive(Clone)]
pub struct Item {
    pub src: PathBuf,
    pub name: String,
    pub size: u64,
    pub mtime: i64,
    pub capture: String,
    pub date_src: String,
    pub camera: Option<String>,
    /// Destination relative to the library root (for duplicates: where the
    /// existing copy is, when it is inside the library).
    pub rel_dest: String,
    pub dest: PathBuf,
    pub status: Status,
    /// Content hash if the scan already computed it.
    pub full: Option<[u8; 32]>,
    /// Basename-group index (RAW + JPEG + sidecars share a group).
    pub group: usize,
}

#[derive(Clone)]
pub struct Plan {
    pub kind: Kind,
    pub source: PathBuf,
    pub root: PathBuf,
    pub layout: String,
    pub dup_mode: DupMode,
    pub items: Vec<Item>,
    pub undated: Vec<PathBuf>,
    pub scan_errors: Vec<String>,
    pub ignored: usize,
    pub in_place: usize,
    pub trash_dups: bool,
    /// (volume name, volume UUID) when importing from a card.
    pub volume: Option<(String, String)>,
}

impl Plan {
    pub fn new_items(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| matches!(i.status, Status::New))
    }
    pub fn duplicates(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| matches!(i.status, Status::Duplicate { .. }))
    }
    pub fn conflicts(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| matches!(i.status, Status::Conflict { .. }))
    }
    pub fn new_bytes(&self) -> u64 {
        self.new_items().map(|i| i.size).sum()
    }
    pub fn trashable(&self) -> usize {
        self.duplicates().filter(|i| matches!(i.status, Status::Duplicate { trashable: true, .. })).count()
    }
}

struct Cand {
    path: PathBuf,
    name: String,
    ext: String,
    size: u64,
    mtime: i64,
}

impl Cand {
    fn sidecar(&self) -> bool {
        media::is_sidecar(&self.ext)
    }
}

fn check_import_paths(source: &Path, root: &Path) -> Result<()> {
    let src = fs::canonicalize(source)?;
    let dst = fs::canonicalize(root)?;
    if src == dst || src.starts_with(&dst) {
        bail!("Source is inside the library — use \"Tidy library\" to sort files already there");
    }
    if dst.starts_with(&src) {
        bail!("Source contains the library ({})", dst.display());
    }
    Ok(())
}

fn list_source(source: &Path, p: &Progress) -> (Vec<Cand>, usize, Vec<String>) {
    let mut cands = Vec::new();
    let mut ignored = 0;
    let mut errors = Vec::new();
    let walker = WalkDir::new(source)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'));
    for entry in walker {
        let e = match entry {
            Ok(e) => e,
            Err(err) => {
                errors.push(err.to_string());
                continue;
            }
        };
        if !e.file_type().is_file() {
            continue;
        }
        let name = nfc(&e.file_name().to_string_lossy());
        let ext = media::ext_lower(&name);
        if !media::is_media(&ext) || e.path().to_string_lossy().contains(['\n', '\r']) {
            ignored += 1;
            continue;
        }
        let m = match e.metadata() {
            Ok(m) => m,
            Err(err) => {
                errors.push(format!("{}: {err}", e.path().display()));
                continue;
            }
        };
        cands.push(Cand { path: e.into_path(), name, ext, size: m.len(), mtime: mtime_of(&m) });
        if cands.len() % 200 == 0 {
            p.tick(cands.len());
        }
    }
    (cands, ignored, errors)
}

/// Content-verify a single source file against catalog rows that share its
/// name and size when either side is missing a capture date.
fn match_by_content(cat: &Catalog, cand: &Cand, rows: &mut [Row], p: &Progress) -> Result<Option<(PathBuf, [u8; 32])>> {
    if rows.is_empty() {
        return Ok(None);
    }
    let Ok(cand_head) = hash_head(&cand.path) else { return Ok(None) };
    library::ensure_hashes(cat, rows, false, p)?;
    let mut head_matches: Vec<Row> = rows
        .iter()
        .filter(|r| r.head.as_deref() == Some(cand_head.as_slice()))
        .cloned()
        .collect();
    if head_matches.is_empty() {
        return Ok(None);
    }
    let Ok(cand_full) = hash_file(&cand.path) else { return Ok(None) };
    library::ensure_hashes(cat, &mut head_matches, true, p)?;
    if let Some(r) = head_matches.iter().find(|r| r.full.as_deref() == Some(cand_full.as_slice())) {
        return Ok(Some((cat.root.join(&r.rel), cand_full)));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

pub fn plan_import(
    source: &Path,
    root: &Path,
    cfg: &LibraryConfig,
    dup_mode: DupMode,
    volume: Option<(String, String)>,
    p: &Progress,
) -> Result<Plan> {
    check_import_paths(source, root)?;
    let cat = Catalog::open(root)?;
    let mut et = exif::Reader::new();
    let sync_stats = library::sync(&cat, cfg, &mut et, p)?;
    let mut scan_errors = sync_stats.errors;

    p.phase("Listing files on source", 0);
    let (cands, ignored, src_errors) = list_source(source, p);
    scan_errors.extend(src_errors);
    let n = cands.len();

    // Basename groups: DSCF0001.RAF / .JPG / .RAF.pp3 travel together.
    let mut groups: Vec<(PathBuf, String, Vec<usize>)> = Vec::new();
    let mut gidx: HashMap<(PathBuf, String), usize> = HashMap::new();
    for (i, c) in cands.iter().enumerate() {
        let key = (c.path.parent().unwrap_or(Path::new("")).to_path_buf(), media::group_stem(&c.name));
        let g = *gidx.entry(key.clone()).or_insert_with(|| {
            groups.push((key.0, key.1, Vec::new()));
            groups.len() - 1
        });
        groups[g].2.push(i);
    }

    let mut status: Vec<Option<Status>> = vec![None; n];
    let mut fulls: Vec<Option<[u8; 32]>> = vec![None; n];

    if dup_mode == DupMode::Verified {
        verified_check(&cat, &cands, &mut status, &mut fulls, p)?;
    }

    // Capture dates: one file per group, and only for groups with something
    // still undecided. In suspected mode the date is part of the match.
    let mut rep_of: Vec<Option<usize>> = vec![None; groups.len()];
    let mut to_date = Vec::new();
    for (g, (_, _, members)) in groups.iter().enumerate() {
        if members.iter().all(|&i| status[i].is_some() || cands[i].sidecar()) && dup_mode == DupMode::Verified {
            continue;
        }
        let rep = members
            .iter()
            .copied()
            .filter(|&i| !cands[i].sidecar())
            .min_by_key(|&i| (media::date_rank(&cands[i].ext), cands[i].name.clone()));
        if let Some(r) = rep {
            rep_of[g] = Some(r);
            to_date.push(cands[r].path.clone());
        }
    }
    p.phase("Reading capture dates", to_date.len());
    let metas = exif::read_all(&mut et, &to_date, |k| p.tick(k), &|| p.cancelled());
    drop(et);
    if p.cancelled() {
        bail!("cancelled");
    }
    let group_meta = |g: usize| -> Option<&Meta> { rep_of[g].and_then(|r| metas.get(&cands[r].path)) };

    if dup_mode == DupMode::Suspected {
        type UndatedEntry = (PathBuf, [u8; 32], Option<[u8; 32]>);
        p.phase("Checking for suspected duplicates", n);
        let mut seen: HashMap<(String, u64, String), PathBuf> = HashMap::new();
        let mut seen_undated: HashMap<(String, u64), Vec<UndatedEntry>> = HashMap::new();
        for (g, (_, _, members)) in groups.iter().enumerate() {
            let capture = group_meta(g).and_then(|m| m.capture.clone());
            for &i in members {
                let c = &cands[i];
                if c.sidecar() {
                    continue;
                }
                let rows = cat.by_name_size(&c.name, c.size)?;
                if let Some(cap) = &capture {
                    if let Some(r) = rows.iter().find(|r| r.capture.as_deref() == Some(cap.as_str())) {
                        status[i] = Some(Status::Duplicate {
                            existing: root.join(&r.rel),
                            rule: "suspected",
                            hash: None,
                            trashable: false,
                        });
                        continue;
                    }
                }
                // Missing capture date on either side: don't guess — verify by content.
                let mut ambiguous: Vec<Row> = rows
                    .into_iter()
                    .filter(|r| capture.is_none() || r.capture.is_none())
                    .collect();
                if let Some((existing, h)) = match_by_content(&cat, c, &mut ambiguous, p)? {
                    fulls[i] = Some(h);
                    status[i] = Some(Status::Duplicate { existing, rule: "content", hash: Some(hex(&h)), trashable: false });
                    continue;
                }
                if let Some(cap) = &capture {
                    let key = (c.name.clone(), c.size, cap.clone());
                    if let Some(prev) = seen.get(&key) {
                        status[i] = Some(Status::Duplicate { existing: prev.clone(), rule: "suspected", hash: None, trashable: false });
                    } else {
                        seen.insert(key, c.path.clone());
                    }
                } else if let Ok(head) = hash_head(&c.path) {
                    let entry = seen_undated.entry((c.name.clone(), c.size)).or_default();
                    let mut matched = None;
                    let mut my_full = None;
                    for (prev_path, prev_head, prev_full) in entry.iter_mut() {
                        if *prev_head != head {
                            continue;
                        }
                        if prev_full.is_none() {
                            *prev_full = hash_file(prev_path).ok();
                        }
                        if my_full.is_none() {
                            my_full = hash_file(&c.path).ok();
                        }
                        if let (Some(a), Some(b)) = (*prev_full, my_full) {
                            if a == b {
                                matched = Some((prev_path.clone(), b));
                                break;
                            }
                        }
                    }
                    if let Some((existing, h)) = matched {
                        fulls[i] = Some(h);
                        status[i] = Some(Status::Duplicate { existing, rule: "content", hash: Some(hex(&h)), trashable: false });
                    } else {
                        entry.push((c.path.clone(), head, my_full));
                    }
                }
            }
        }
    }

    // Route.
    p.phase("Planning destinations", 0);
    let mut planned: HashSet<String> = HashSet::new();
    let mut items = Vec::with_capacity(n);
    let mut undated = Vec::new();
    for (g, (_, _, members)) in groups.iter().enumerate() {
        let meta = group_meta(g);
        let rep_raw = rep_of[g].is_some_and(|r| media::is_raw(&cands[r].ext));
        let mut member_dir: HashMap<&str, String> = HashMap::new();

        for &i in members.iter().filter(|&&i| !cands[i].sidecar()) {
            let c = &cands[i];
            let mut item = Item {
                src: c.path.clone(),
                name: c.name.clone(),
                size: c.size,
                mtime: c.mtime,
                capture: String::new(),
                date_src: String::new(),
                camera: meta.and_then(|m| m.camera.clone()),
                rel_dest: String::new(),
                dest: PathBuf::new(),
                status: Status::New,
                full: fulls[i],
                group: g,
            };
            if let Some(m) = meta {
                item.capture = m.capture.clone().unwrap_or_default();
                item.date_src = if rep_raw && !media::is_raw(&c.ext) { format!("{} (via RAW)", m.src) } else { m.src.clone() };
            }
            if let Some(Status::Duplicate { existing, .. }) = &status[i] {
                item.rel_dest = existing.strip_prefix(root).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
                if let Some(parent) = Path::new(&item.rel_dest).parent().filter(|_| !item.rel_dest.is_empty()) {
                    member_dir.insert(&c.name, parent.to_string_lossy().into_owned());
                }
                if item.capture.is_empty() {
                    // Content duplicates are never dated; borrow the library copy's date.
                    item.capture = cat.capture_of(&item.rel_dest).unwrap_or_default();
                }
                item.dest = existing.clone();
                item.status = status[i].clone().unwrap();
                items.push(item);
                continue;
            }
            if item.capture.is_empty() {
                undated.push(c.path.clone());
                continue;
            }
            let rel = layout::render(&cfg.layout, &Fields { capture: &item.capture, camera: item.camera.as_deref(), name: &c.name, ext: &c.ext });
            member_dir.insert(&c.name, parent_of(&rel));
            item.status = route_status(&cat, root, &rel, &mut planned)?;
            item.dest = root.join(&rel);
            item.rel_dest = rel;
            items.push(item);
        }

        let names: Vec<&str> = members.iter().filter(|&&i| !cands[i].sidecar()).map(|&i| cands[i].name.as_str()).collect();
        for &i in members.iter().filter(|&&i| cands[i].sidecar()) {
            let c = &cands[i];
            let dir = media::sidecar_parent(&c.name, &names).and_then(|p| member_dir.get(p).cloned());
            let (rel, capture, src) = match dir {
                Some(d) => (join_rel(&d, &c.name), meta.and_then(|m| m.capture.clone()).unwrap_or_default(), "follows its photo".to_string()),
                None => {
                    // Orphan sidecar: route it on its own date.
                    let m = exif::fallback(&c.path);
                    let Some(cap) = m.capture else {
                        undated.push(c.path.clone());
                        continue;
                    };
                    let rel = layout::render(&cfg.layout, &Fields { capture: &cap, camera: None, name: &c.name, ext: &c.ext });
                    (rel, cap, m.src)
                }
            };
            let dest = root.join(&rel);
            // A sidecar is "already there" only if the file at its destination
            // is byte-identical; otherwise it is a different edit -> conflict.
            let status = if dest.exists() {
                match (hash_file(&c.path), hash_file(&dest)) {
                    (Ok(a), Ok(b)) if a == b => {
                        Status::Duplicate { existing: dest.clone(), rule: "content", hash: Some(hex(&a)), trashable: false }
                    }
                    _ => Status::Conflict { reason: "a different sidecar with this name is already there".into() },
                }
            } else if !planned.insert(rel.clone()) {
                Status::Conflict { reason: "another file in this import has the same name".into() }
            } else {
                Status::New
            };
            items.push(Item {
                src: c.path.clone(),
                name: c.name.clone(),
                size: c.size,
                mtime: c.mtime,
                capture,
                date_src: src,
                camera: None,
                rel_dest: rel,
                dest,
                status,
                full: None,
                group: g,
            });
        }
    }

    items.sort_by(|a, b| a.rel_dest.cmp(&b.rel_dest).then(a.src.cmp(&b.src)));
    Ok(Plan {
        kind: Kind::Import,
        source: source.to_path_buf(),
        root: root.to_path_buf(),
        layout: cfg.layout.clone(),
        dup_mode,
        items,
        undated,
        scan_errors,
        ignored,
        in_place: 0,
        trash_dups: false,
        volume,
    })
}

fn parent_of(rel: &str) -> String {
    rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default()
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn route_status(cat: &Catalog, root: &Path, rel: &str, planned: &mut HashSet<String>) -> Result<Status> {
    Ok(if root.join(rel).exists() || cat.has_rel(rel)? {
        Status::Conflict { reason: "a different file with this name is already in the library".into() }
    } else if !planned.insert(rel.to_string()) {
        Status::Conflict { reason: "another file in this import has the same name".into() }
    } else {
        Status::New
    })
}

/// Staged content check. The card is read by one thread, in order (parallel
/// reads make a card seek); library hashes come from the catalog or are
/// computed in parallel and stored.
fn verified_check(cat: &Catalog, cands: &[Cand], status: &mut [Option<Status>], fulls: &mut [Option<[u8; 32]>], p: &Progress) -> Result<()> {
    let idx: Vec<usize> = (0..cands.len()).filter(|&i| !cands[i].sidecar()).collect();

    let mut size_count: HashMap<u64, usize> = HashMap::new();
    for &i in &idx {
        *size_count.entry(cands[i].size).or_default() += 1;
    }
    let mut lib_rows: Vec<Row> = Vec::new();
    for size in size_count.keys() {
        lib_rows.extend(cat.by_size(*size)?.into_iter().filter(|r| !media::is_sidecar(&media::ext_lower(&r.rel))));
    }
    let lib_sizes: HashSet<u64> = lib_rows.iter().map(|r| r.size).collect();
    let need: Vec<usize> = idx.iter().copied().filter(|&i| lib_sizes.contains(&cands[i].size) || size_count[&cands[i].size] > 1).collect();
    if need.is_empty() {
        return Ok(());
    }

    // Stage 1: quick hash (128 KB per file).
    p.phase("Duplicate check: quick-hashing library matches", lib_rows.len());
    library::ensure_hashes(cat, &mut lib_rows, false, p)?;
    p.phase("Duplicate check: quick-hashing card files", need.len());
    let mut card_head: HashMap<usize, [u8; 32]> = HashMap::new();
    for (k, &i) in need.iter().enumerate() {
        if p.cancelled() {
            bail!("cancelled");
        }
        if let Ok(h) = hash_head(&cands[i].path) {
            card_head.insert(i, h);
        }
        p.tick(k + 1);
    }

    let mut head_count: HashMap<(u64, [u8; 32]), usize> = HashMap::new();
    for (&i, h) in &card_head {
        *head_count.entry((cands[i].size, *h)).or_default() += 1;
    }
    let lib_heads: HashSet<(u64, Vec<u8>)> = lib_rows.iter().filter_map(|r| r.head.clone().map(|h| (r.size, h))).collect();
    let mut want: Vec<usize> = need
        .iter()
        .copied()
        .filter(|i| {
            card_head.get(i).is_some_and(|h| lib_heads.contains(&(cands[*i].size, h.to_vec())) || head_count[&(cands[*i].size, *h)] > 1)
        })
        .collect();
    want.sort_by(|a, b| cands[*a].path.cmp(&cands[*b].path));
    if want.is_empty() {
        return Ok(());
    }

    // Stage 2: full hash, only where the quick hash matched.
    let want_heads: HashSet<(u64, Vec<u8>)> = want.iter().map(|i| (cands[*i].size, card_head[i].to_vec())).collect();
    let mut lib_match: Vec<Row> =
        lib_rows.into_iter().filter(|r| r.head.as_ref().is_some_and(|h| want_heads.contains(&(r.size, h.clone())))).collect();
    p.phase("Duplicate check: verifying library matches", lib_match.len());
    library::ensure_hashes(cat, &mut lib_match, true, p)?;
    let lib_by_full: HashMap<Vec<u8>, &Row> = lib_match.iter().filter_map(|r| r.full.clone().map(|f| (f, r))).collect();

    p.phase("Duplicate check: verifying card files", want.len());
    let mut seen: HashMap<[u8; 32], PathBuf> = HashMap::new();
    for (k, &i) in want.iter().enumerate() {
        if p.cancelled() {
            bail!("cancelled");
        }
        p.tick(k + 1);
        let Ok(h) = hash_file(&cands[i].path) else { continue };
        fulls[i] = Some(h);
        if let Some(r) = lib_by_full.get(h.as_slice()) {
            status[i] = Some(Status::Duplicate { existing: cat.root.join(&r.rel), rule: "content", hash: Some(hex(&h)), trashable: false });
        } else if let Some(prev) = seen.get(&h) {
            status[i] = Some(Status::Duplicate { existing: prev.clone(), rule: "content", hash: Some(hex(&h)), trashable: false });
        } else {
            seen.insert(h, cands[i].path.clone());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tidy / Reorganize
// ---------------------------------------------------------------------------

pub fn plan_reconcile(root: &Path, cfg: &LibraryConfig, kind: Kind, p: &Progress) -> Result<Plan> {
    let cat = Catalog::open(root)?;
    let mut et = exif::Reader::new();
    let sync_stats = library::sync(&cat, cfg, &mut et, p)?;
    drop(et);

    let mut rows = cat.present()?;
    let top = |rel: &str| rel.split('/').next().unwrap_or("").to_string();
    let sorted = |rel: &str| rel.contains('/') && layout::first_segment_matches(&cfg.layout, &top(rel));
    let is_side = |r: &Row| media::is_sidecar(&media::ext_lower(&r.rel));

    // --- duplicates inside the library (content, sidecars excluded) ---
    p.phase("Looking for duplicates inside the library", 0);
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if !is_side(r) && r.size > 0 {
            by_size.entry(r.size).or_default().push(i);
        }
    }
    let multi: Vec<usize> = by_size.values().filter(|v| v.len() > 1).flatten().copied().collect();
    let mut dup_of: HashMap<usize, (usize, String)> = HashMap::new();
    if !multi.is_empty() {
        let mut sub: Vec<Row> = multi.iter().map(|&i| rows[i].clone()).collect();
        p.phase("Duplicate check: quick-hashing same-size files", sub.len());
        library::ensure_hashes(&cat, &mut sub, false, p)?;
        let mut by_head: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
        for (k, r) in sub.iter().enumerate() {
            if let Some(h) = &r.head {
                by_head.entry(h.clone()).or_default().push(k);
            }
        }
        let cand: Vec<usize> = by_head.values().filter(|v| v.len() > 1).flatten().copied().collect();
        let mut sub2: Vec<Row> = cand.iter().map(|&k| sub[k].clone()).collect();
        p.phase("Duplicate check: verifying matches", sub2.len());
        library::ensure_hashes(&cat, &mut sub2, true, p)?;
        let mut by_full: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
        let id_to_row: HashMap<i64, usize> = rows.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
        for r in &sub2 {
            if let Some(f) = &r.full {
                by_full.entry(f.clone()).or_default().push(id_to_row[&r.id]);
            }
        }
        for (h, mut group) in by_full.into_iter().filter(|(_, v)| v.len() > 1) {
            // Keep the copy that is already sorted, then the oldest known.
            group.sort_by_key(|&i| (!sorted(&rows[i].rel), rows[i].first_seen.clone(), rows[i].rel.len(), rows[i].rel.clone()));
            let keeper = group[0];
            for &d in &group[1..] {
                dup_of.insert(d, (keeper, hex(&h)));
            }
        }
        // Remember computed hashes on our copy of the rows.
        for r in sub.into_iter().chain(sub2) {
            if let Some(&i) = id_to_row.get(&r.id) {
                rows[i].head = rows[i].head.take().or(r.head);
                rows[i].full = rows[i].full.take().or(r.full);
            }
        }
    }

    // --- groups: sorted files group within their top folder (RAW/ and JPG/
    // siblings), loose files only with files in the same folder ---
    p.phase("Planning moves", 0);
    let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        let name = r.rel.rsplit('/').next().unwrap_or(&r.rel);
        let scope = if sorted(&r.rel) { top(&r.rel) } else { parent_of(&r.rel) };
        groups.entry((scope, media::group_stem(name))).or_default().push(i);
    }
    let mut keys: Vec<_> = groups.keys().cloned().collect();
    keys.sort();

    let mut items = Vec::new();
    let mut undated = Vec::new();
    let mut in_place = 0;
    let mut planned: HashSet<String> = HashSet::new();
    let reorganize = kind == Kind::Reorganize;

    for (g, key) in keys.into_iter().enumerate() {
        let members = &groups[&key];
        let name_of = |i: usize| rows[i].rel.rsplit('/').next().unwrap_or(&rows[i].rel).to_string();
        let rep = members
            .iter()
            .copied()
            .filter(|&i| !is_side(&rows[i]) && rows[i].capture.is_some())
            .min_by_key(|&i| (media::date_rank(&media::ext_lower(&rows[i].rel)), name_of(i)));
        let group_capture = rep.and_then(|i| rows[i].capture.clone());
        let mut final_dir: HashMap<String, String> = HashMap::new();

        for &i in members.iter().filter(|&&i| !is_side(&rows[i])) {
            let r = &rows[i];
            let name = name_of(i);
            if let Some((keeper, h)) = dup_of.get(&i) {
                final_dir.insert(name.clone(), parent_of(&r.rel));
                items.push(Item {
                    src: root.join(&r.rel),
                    name,
                    size: r.size,
                    mtime: r.mtime,
                    capture: r.capture.clone().unwrap_or_default(),
                    date_src: r.date_src.clone().unwrap_or_default(),
                    camera: r.camera.clone(),
                    rel_dest: rows[*keeper].rel.clone(),
                    dest: root.join(&rows[*keeper].rel),
                    status: Status::Duplicate {
                        existing: root.join(&rows[*keeper].rel),
                        rule: "content",
                        hash: Some(h.clone()),
                        trashable: !sorted(&r.rel),
                    },
                    full: None,
                    group: g,
                });
                continue;
            }
            let Some(capture) = group_capture.clone().or(r.capture.clone()) else {
                undated.push(root.join(&r.rel));
                final_dir.insert(name, parent_of(&r.rel));
                continue;
            };
            let ext = media::ext_lower(&name);
            let expected = layout::render(&cfg.layout, &Fields { capture: &capture, camera: r.camera.as_deref(), name: &name, ext: &ext });
            let moving = expected != r.rel && (reorganize || !sorted(&r.rel));
            if !moving {
                in_place += 1;
                final_dir.insert(name, parent_of(&r.rel));
                continue;
            }
            final_dir.insert(name.clone(), parent_of(&expected));
            let status = move_status(root, &r.rel, &expected, &mut planned);
            items.push(Item {
                src: root.join(&r.rel),
                name,
                size: r.size,
                mtime: r.mtime,
                capture,
                date_src: r.date_src.clone().unwrap_or_default(),
                camera: r.camera.clone(),
                dest: root.join(&expected),
                rel_dest: expected,
                status,
                full: None,
                group: g,
            });
        }

        let names: Vec<String> = members.iter().filter(|&&i| !is_side(&rows[i])).map(|&i| name_of(i)).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        for &i in members.iter().filter(|&&i| is_side(&rows[i])) {
            let r = &rows[i];
            let name = name_of(i);
            let target = media::sidecar_parent(&name, &name_refs).and_then(|p| final_dir.get(p));
            match target {
                Some(dir) if *dir != parent_of(&r.rel) => {
                    let expected = join_rel(dir, &name);
                    let mut status = move_status(root, &r.rel, &expected, &mut planned);
                    // A detached copy identical to the sidecar already beside
                    // its photo is a leftover duplicate, not a conflict.
                    if matches!(status, Status::Conflict { .. }) {
                        if let (Ok(a), Ok(b)) = (hash_file(&root.join(&r.rel)), hash_file(&root.join(&expected))) {
                            if a == b {
                                status = Status::Duplicate {
                                    existing: root.join(&expected),
                                    rule: "content",
                                    hash: Some(hex(&a)),
                                    trashable: true,
                                };
                            }
                        }
                    }
                    items.push(Item {
                        src: root.join(&r.rel),
                        name,
                        size: r.size,
                        mtime: r.mtime,
                        capture: String::new(),
                        date_src: "follows its photo".into(),
                        camera: None,
                        dest: root.join(&expected),
                        rel_dest: expected,
                        status,
                        full: None,
                        group: g,
                    });
                }
                _ => in_place += 1,
            }
        }
    }

    items.sort_by(|a, b| a.rel_dest.cmp(&b.rel_dest).then(a.src.cmp(&b.src)));
    Ok(Plan {
        kind,
        source: root.to_path_buf(),
        root: root.to_path_buf(),
        layout: cfg.layout.clone(),
        dup_mode: DupMode::Verified,
        items,
        undated,
        scan_errors: sync_stats.errors,
        ignored: 0,
        in_place,
        trash_dups: cfg.trash_duplicates,
        volume: None,
    })
}

fn move_status(root: &Path, from: &str, to: &str, planned: &mut HashSet<String>) -> Status {
    let dest = root.join(to);
    // On a case-insensitive disk "raw/x" and "RAW/x" are the same file.
    let same_file = || {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(root.join(from)), fs::metadata(&dest)) {
            (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
            _ => false,
        }
    };
    if dest.exists() && !same_file() {
        Status::Conflict { reason: "a different file already has this name there".into() }
    } else if !planned.insert(to.to_string()) {
        Status::Conflict { reason: "another file is being moved to the same name".into() }
    } else {
        Status::New
    }
}
