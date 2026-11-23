//! Executing a plan.
//!
//! Import: a reader thread streams the card into a small bounded queue while
//! this thread hashes and writes, so the card never idles waiting for a write.
//! A finisher thread then fsyncs, verifies (re-read and hash) and renames each
//! copy into place and records it in the catalog, in batched transactions.
//! One full drive flush per batch (Move mode) and at the end of the run; Move
//! mode deletes a source only after its batch is flushed.
//!
//! Tidy/Reorganize: renames inside the library, each recorded so the whole
//! run can be undone. Duplicates are only moved to the Trash when asked.
//!
//! Every run writes LOG/<stamp>_<kind>.log; LOG/<stamp>_duplicates.csv too
//! when duplicates were found.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use anyhow::{bail, Result};
use chrono::Local;

use crate::catalog::{Catalog, NewRow};
use crate::config::LibraryConfig;
use crate::jobs::Progress;
use crate::plan::{Item, Kind, Plan, Status};
use crate::util::{csv_field, hash_file, home, human_bytes, now, read_full, tilde, HeadHasher};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Copy,
    Move,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Copy => "COPY",
            Mode::Move => "MOVE",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Mode::Copy => "copy",
            Mode::Move => "move",
        }
    }
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("move") {
            Mode::Move
        } else {
            Mode::Copy
        }
    }
}

pub struct Summary {
    pub kind: Kind,
    pub mode: Mode,
    pub transferred: usize,
    pub bytes: u64,
    pub failed: usize,
    pub warnings: usize,
    pub duplicates: usize,
    pub trashed: usize,
    pub conflicts: usize,
    pub undated: usize,
    pub cancelled: bool,
    pub log_path: PathBuf,
    pub csv_path: Option<PathBuf>,
    pub note: Option<String>,
}

pub enum XferMsg {
    Progress { files_done: usize, bytes_done: u64, current: String, phase: &'static str },
    Line(String),
    Done(Result<Summary, String>),
}

const CHUNK: usize = 8 << 20;
/// Chunks buffered between reader and writer: 8 x 8 MB.
const QUEUE: usize = 8;

pub fn run(plan: Plan, mode: Mode, backup_dir: Option<PathBuf>, cancel: Arc<AtomicBool>, tx: Sender<XferMsg>) {
    let r = match plan.kind {
        Kind::Import => import(&plan, mode, &cancel, &tx),
        Kind::Tidy | Kind::Reorganize => reconcile(&plan, &cancel, &tx),
    };
    let r = r.map(|mut s| {
        if let Ok(cat) = Catalog::open(&plan.root) {
            if let Err(e) = cat.backup(backup_dir.as_deref()) {
                s.note = Some(format!("catalog backup failed: {e}"));
            }
        }
        s
    });
    let _ = tx.send(XferMsg::Done(r.map_err(|e| format!("{e:#}"))));
}

struct Log {
    w: BufWriter<File>,
}

impl Log {
    fn line(&mut self, tag: &str, text: &str) {
        let _ = writeln!(self.w, "{}  {tag:<10} {text}", now());
        let _ = self.w.flush();
    }
    fn raw(&mut self, text: &str) {
        let _ = writeln!(self.w, "{text}");
    }
}

fn open_log(plan: &Plan, kind: &str) -> Result<(Log, PathBuf, String)> {
    let dir = plan.root.join("LOG");
    fs::create_dir_all(&dir)?;
    let stamp = Local::now().format("%Y-%m-%d_%H%M%S").to_string();
    let file = if kind == "import" { "transfer" } else { kind };
    let path = dir.join(format!("{stamp}_{file}.log"));
    Ok((Log { w: BufWriter::new(File::create(&path)?) }, path, stamp))
}

fn write_dup_csv(plan: &Plan, stamp: &str, trashed: &[PathBuf]) -> Result<Option<PathBuf>> {
    let dups: Vec<&Item> = plan.duplicates().collect();
    if dups.is_empty() {
        return Ok(None);
    }
    let p = plan.root.join("LOG").join(format!("{stamp}_duplicates.csv"));
    let mut w = BufWriter::new(File::create(&p)?);
    writeln!(w, "source_file,size_bytes,capture_time,existing_file,match_rule,blake3,action")?;
    for d in dups {
        if let Status::Duplicate { existing, rule, hash, .. } = &d.status {
            let action = if trashed.contains(&d.src) { "moved to Trash" } else { "skipped" };
            writeln!(
                w,
                "{},{},{},{},{},{},{}",
                csv_field(&d.src.display().to_string()),
                d.size,
                d.capture,
                csv_field(&existing.display().to_string()),
                rule,
                hash.as_deref().unwrap_or(""),
                action
            )?;
        }
    }
    w.flush()?;
    Ok(Some(p))
}

fn header(log: &mut Log, plan: &Plan, mode: &str) {
    log.raw(&format!("mootui {} log", plan.kind.key()));
    log.raw(&format!("started    {}", now()));
    log.raw(&format!("source     {}", plan.source.display()));
    log.raw(&format!("library    {}", plan.root.display()));
    log.raw(&format!("layout     {}", plan.layout));
    log.raw(&format!("mode       {mode}"));
    if plan.kind == Kind::Import {
        log.raw(&format!("duplicates {}", plan.dup_mode.label()));
    }
    log.raw(&format!(
        "plan       {} to {}, {} duplicate, {} conflict, {} undated, {} non-media ignored, {} already in place",
        plan.new_items().count(),
        if plan.kind == Kind::Import { "transfer" } else { "move" },
        plan.duplicates().count(),
        plan.conflicts().count(),
        plan.undated.len(),
        plan.ignored,
        plan.in_place,
    ));
    log.raw("");
    for d in plan.duplicates() {
        if let Status::Duplicate { existing, rule, .. } = &d.status {
            log.line("DUPLICATE", &format!("{}  ==  {}  ({rule})", d.src.display(), existing.display()));
        }
    }
    for c in plan.conflicts() {
        if let Status::Conflict { reason } = &c.status {
            log.line("CONFLICT", &format!("{}  ->  {}  ({reason})", c.src.display(), c.rel_dest));
        }
    }
    for u in &plan.undated {
        log.line("UNDATED", &u.display().to_string());
    }
}

fn footer(log: &mut Log, s: &Summary) {
    log.raw("");
    log.raw(&format!("finished   {}", now()));
    log.raw(&format!(
        "result     {} done ({}), {} failed, {} warnings, {} duplicates ({} to Trash), {} conflicts{}",
        s.transferred,
        human_bytes(s.bytes),
        s.failed,
        s.warnings,
        s.duplicates,
        s.trashed,
        s.conflicts,
        if s.cancelled { ", CANCELLED" } else { "" }
    ));
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

enum Chunk {
    Data(Vec<u8>),
    End(std::io::Result<()>),
}

/// Reads files in order into a bounded queue. One reader: a card is fastest
/// read sequentially.
fn spawn_reader(paths: Vec<PathBuf>, cancel: Arc<AtomicBool>) -> Receiver<Chunk> {
    let (tx, rx) = sync_channel(QUEUE);
    thread::spawn(move || {
        for p in paths {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let r = (|| -> std::io::Result<()> {
                let mut f = File::open(&p)?;
                loop {
                    let mut buf = vec![0u8; CHUNK];
                    let n = read_full(&mut f, &mut buf)?;
                    if n == 0 {
                        return Ok(());
                    }
                    buf.truncate(n);
                    if tx.send(Chunk::Data(buf)).is_err() {
                        return Ok(());
                    }
                    if n < CHUNK {
                        return Ok(());
                    }
                }
            })();
            if tx.send(Chunk::End(r)).is_err() {
                return;
            }
        }
    });
    rx
}

fn import(plan: &Plan, mode: Mode, cancel: &AtomicBool, tx: &Sender<XferMsg>) -> Result<Summary> {
    let cat = Catalog::open(&plan.root)?;
    let vol = plan.volume.as_ref().map(|(n, u)| (n.as_str(), u.as_str()));
    let import_id = cat.begin_import("import", &plan.source.to_string_lossy(), vol, mode.key(), &plan.layout)?;
    let (mut log, log_path, stamp) = open_log(plan, "import")?;
    header(&mut log, plan, mode.label());
    let csv_path = write_dup_csv(plan, &stamp, &[])?;

    let dest_dev = fs::metadata(&plan.root)?.dev();
    let new: Vec<&Item> = plan.new_items().collect();
    let renames: Vec<bool> = new
        .iter()
        .map(|i| mode == Mode::Move && fs::metadata(&i.src).map(|m| m.dev() == dest_dev).unwrap_or(false))
        .collect();
    let cancel_arc = Arc::new(AtomicBool::new(false));
    let to_read: Vec<PathBuf> = new.iter().zip(&renames).filter(|(_, r)| !**r).map(|(i, _)| i.src.clone()).collect();
    let rx = spawn_reader(to_read, cancel_arc.clone());

    let s = Summary {
        kind: Kind::Import,
        mode,
        transferred: 0,
        bytes: 0,
        failed: 0,
        warnings: 0,
        duplicates: plan.duplicates().count(),
        trashed: 0,
        conflicts: plan.conflicts().count(),
        undated: plan.undated.len(),
        cancelled: false,
        log_path,
        csv_path,
        note: None,
    };

    let (ftx, frx) = sync_channel::<Written>(FINISH_QUEUE);
    let (cat, mut log, s) = thread::scope(|sc| {
        let fin = Finisher { new: &new, mode, cat, log, s, import_id, tx, open_tx: false, rows: 0, pending: Vec::new(), last: None };
        let handle = sc.spawn(move || fin.run(frx));

        // Writer: stream each file to its temp path and hand it on. Flushing,
        // verifying and cataloguing happen on the finisher, so this loop keeps
        // draining the reader queue.
        let mut bytes_done = 0u64;
        for (k, item) in new.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                cancel_arc.store(true, Ordering::Relaxed);
                let _ = ftx.send(Written::Cancelled { remaining: new.len() - k });
                break;
            }
            let base = bytes_done;
            let send = |n: u64, phase: &'static str| {
                let _ = tx.send(XferMsg::Progress { files_done: k, bytes_done: base + n, current: item.name.clone(), phase });
            };
            send(0, if renames[k] { "moving" } else { "copying" });
            let w = if renames[k] { Written::Done { k, result: rename_in(item), copied: false } } else { copy_in(k, item, &rx, &send) };
            bytes_done = base + item.size;
            if ftx.send(w).is_err() {
                break;
            }
        }
        drop(ftx);
        drop(rx);
        handle.join().expect("finisher thread panicked")
    });

    let _ = tx.send(XferMsg::Progress { files_done: s.transferred + s.failed, bytes_done: s.bytes, current: String::new(), phase: "done" });
    footer(&mut log, &s);
    cat.finish_import(
        import_id,
        &format!(
            "{{\"transferred\":{},\"failed\":{},\"duplicates\":{},\"conflicts\":{},\"cancelled\":{}}}",
            s.transferred, s.failed, s.duplicates, s.conflicts, s.cancelled
        ),
    )?;
    Ok(s)
}

type Hashes = (Option<[u8; 32]>, Option<[u8; 32]>);

/// Files handed from the writer to the finisher, in plan order.
enum Written {
    /// Fully streamed to `tmp`; still to be flushed, verified and renamed.
    Copied { k: usize, tmp: PathBuf, file: File, hash: [u8; 32], head: [u8; 32] },
    /// Already settled: a same-disk rename, or a copy that failed.
    Done { k: usize, result: Result<Hashes, String>, copied: bool },
    Cancelled { remaining: usize },
}

/// Copied files waiting in the finisher queue (each holds an open file, no data).
const FINISH_QUEUE: usize = 16;
/// Catalog rows per transaction; in Move mode also how many sources wait
/// for one full drive flush before they are deleted.
const BATCH: usize = 64;

/// An outcome not yet written to the log. Move mode holds these until the
/// batch is durable, then deletes the sources; Copy mode emits them at once.
struct Pending {
    k: usize,
    outcome: Result<Option<String>, String>,
    delete_source: bool,
}

struct Finisher<'a> {
    new: &'a [&'a Item],
    mode: Mode,
    cat: Catalog,
    log: Log,
    s: Summary,
    import_id: i64,
    tx: &'a Sender<XferMsg>,
    open_tx: bool,
    rows: usize,
    pending: Vec<Pending>,
    /// Most recent copy, kept open so one F_FULLFSYNC on it flushes the
    /// drive cache for everything fsync'd before it.
    last: Option<File>,
}

impl Finisher<'_> {
    fn run(mut self, rx: Receiver<Written>) -> (Catalog, Log, Summary) {
        for w in rx {
            match w {
                Written::Copied { k, tmp, file, hash, head } => {
                    let r = finish_copy(self.new[k], &tmp, &file, hash, head);
                    if r.is_err() {
                        let _ = fs::remove_file(&tmp);
                    } else {
                        self.last = Some(file);
                    }
                    self.record(k, r, true);
                }
                Written::Done { k, result, copied } => self.record(k, result, copied),
                Written::Cancelled { remaining } => {
                    self.flush(false);
                    self.s.cancelled = true;
                    self.log.line("CANCELLED", &format!("{remaining} file(s) not transferred"));
                }
            }
        }
        self.flush(true);
        (self.cat, self.log, self.s)
    }

    fn record(&mut self, k: usize, r: Result<Hashes, String>, copied: bool) {
        let item = self.new[k];
        let outcome = r.map(|(head, full)| {
            if !self.open_tx {
                self.open_tx = self.cat.conn.execute_batch("BEGIN").is_ok();
            }
            let rec = self.cat.upsert(&NewRow {
                rel: &item.rel_dest,
                size: item.size,
                mtime: item.mtime,
                name: &item.name,
                capture: Some(&item.capture).filter(|c| !c.is_empty()).map(|c| c.as_str()),
                date_src: Some(&item.date_src),
                camera: item.camera.as_deref(),
                head: head.as_ref().map(|h| &h[..]),
                full: full.as_ref().map(|h| &h[..]),
                import_id: Some(self.import_id),
            });
            self.rows += 1;
            self.s.transferred += 1;
            self.s.bytes += item.size;
            rec.err().map(|e| format!("catalog: {e}"))
        });
        self.pending.push(Pending { k, delete_source: self.mode == Mode::Move && copied && outcome.is_ok(), outcome });
        if self.mode == Mode::Copy {
            self.emit(true);
        }
        if self.rows >= BATCH {
            self.flush(false);
        }
    }

    /// Make the batch durable (Move mode, or the end of the run), commit the
    /// catalog, then delete sources and write the held log lines.
    fn flush(&mut self, end: bool) {
        let mut durable = true;
        if end || self.pending.iter().any(|p| p.delete_source) {
            if let Some(f) = self.last.take() {
                // std's sync_all is F_FULLFSYNC on macOS: flushes the drive cache.
                durable = f.sync_all().is_ok();
            }
        }
        if self.open_tx {
            if let Err(e) = self.cat.conn.execute_batch("COMMIT") {
                self.s.warnings += 1;
                self.log.line("WARNING", &format!("catalog commit failed: {e}"));
            }
            self.open_tx = false;
        }
        self.rows = 0;
        self.emit(durable);
    }

    fn emit(&mut self, durable: bool) {
        for p in std::mem::take(&mut self.pending) {
            let item = self.new[p.k];
            let (tag, text) = match p.outcome {
                Ok(mut warn) => {
                    if p.delete_source {
                        if !durable {
                            warn = Some("flush to disk failed; source kept".into());
                        } else if let Err(e) = fs::remove_file(&item.src) {
                            warn = Some(format!("could not delete source: {e}"));
                        }
                    }
                    match warn {
                        Some(w) => {
                            self.s.warnings += 1;
                            ("WARNING", format!("{}  ->  {}  ({w})", item.src.display(), item.rel_dest))
                        }
                        None => (if self.mode == Mode::Move { "MOVED" } else { "COPIED" }, format!("{}  ->  {}", item.src.display(), item.rel_dest)),
                    }
                }
                Err(e) => {
                    self.s.failed += 1;
                    ("FAILED", format!("{}  ->  {}  ({e})", item.src.display(), item.rel_dest))
                }
            };
            self.log.line(tag, &text);
            let _ = self.tx.send(XferMsg::Line(format!("{tag:<9} {text}")));
        }
    }
}

fn rename_in(item: &Item) -> Result<Hashes, String> {
    let dir = item.dest.parent().ok_or("bad destination")?;
    fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
    if item.dest.exists() {
        return Err("exists at destination".into());
    }
    fs::rename(&item.src, &item.dest).map_err(|e| format!("rename: {e}"))?;
    Ok((None, item.full))
}

/// Consume exactly one file's chunks from the reader, even on error, so the
/// stream stays aligned with the item list.
fn copy_in(k: usize, item: &Item, rx: &Receiver<Chunk>, send: &dyn Fn(u64, &'static str)) -> Written {
    let Some(dir) = item.dest.parent() else {
        // Still drain this file's chunks.
        while let Ok(Chunk::Data(_)) = rx.recv() {}
        return Written::Done { k, result: Err("bad destination".into()), copied: true };
    };
    let tmp = dir.join(format!(".{}.mootui-partial", item.name));
    let mut err: Option<String> = None;
    let mut w = match fs::create_dir_all(dir) {
        Err(e) => {
            err = Some(format!("mkdir: {e}"));
            None
        }
        Ok(()) if item.dest.exists() => {
            err = Some("exists at destination".into());
            None
        }
        Ok(()) => match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => Some(f),
            Err(e) => {
                err = Some(format!("create: {e}"));
                None
            }
        },
    };

    let mut h = blake3::Hasher::new();
    let mut quick = HeadHasher::new();
    let mut n = 0u64;
    loop {
        match rx.recv() {
            Ok(Chunk::Data(buf)) => {
                if err.is_none() {
                    h.update(&buf);
                    quick.update(&buf);
                    if let Some(f) = w.as_mut() {
                        if let Err(e) = f.write_all(&buf) {
                            err = Some(format!("write: {e}"));
                        }
                    }
                }
                n += buf.len() as u64;
                send(n, "copying");
            }
            Ok(Chunk::End(r)) => {
                if let Err(e) = r {
                    err.get_or_insert(format!("read: {e}"));
                }
                break;
            }
            Err(_) => {
                err.get_or_insert("cancelled".into());
                break;
            }
        }
    }

    match (err, w) {
        (None, Some(file)) => Written::Copied { k, tmp, file, hash: *h.finalize().as_bytes(), head: quick.finalize() },
        (err, _) => {
            let _ = fs::remove_file(&tmp);
            Written::Done { k, result: Err(err.unwrap_or_else(|| "no file".into())), copied: true }
        }
    }
}

/// Runs on the finisher thread: copy times, fsync, verify, rename into place.
/// Both hashes were taken from the stream; the full re-read confirms the
/// copy holds those same bytes, so the quick hash holds for it too.
fn finish_copy(item: &Item, tmp: &Path, f: &File, streamed: [u8; 32], head: [u8; 32]) -> Result<Hashes, String> {
    if let Ok(meta) = fs::metadata(&item.src) {
        let mut times = fs::FileTimes::new();
        if let Ok(t) = meta.modified() {
            times = times.set_modified(t);
        }
        if let Ok(t) = meta.accessed() {
            times = times.set_accessed(t);
        }
        #[cfg(target_os = "macos")]
        if let Ok(t) = meta.created() {
            use std::os::macos::fs::FileTimesExt;
            times = times.set_created(t);
        }
        let _ = f.set_times(times);
    }
    fsync(f).map_err(|e| format!("sync: {e}"))?;

    let written = hash_file(tmp).map_err(|e| format!("verify: {e}"))?;
    if streamed != written || item.full.is_some_and(|f| f != streamed) {
        return Err("verification failed: copy does not match source".into());
    }
    if item.dest.exists() {
        return Err("exists at destination".into());
    }
    fs::rename(tmp, &item.dest).map_err(|e| format!("rename: {e}"))?;
    Ok((Some(head), Some(streamed)))
}

/// Plain fsync(2): hands the data to the drive. Unlike std's sync_all on
/// macOS it does not also flush the drive's cache (F_FULLFSYNC), which is
/// slow per file; Finisher::flush does that once per batch instead.
fn fsync(f: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: fd is valid for the lifetime of `f`.
    if unsafe { libc::fsync(f.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

// ---------------------------------------------------------------------------
// Tidy / Reorganize
// ---------------------------------------------------------------------------

/// Where the Trash is for files on this disk; None if not the home disk
/// (a rename can't cross disks, and copying to Trash defeats the point).
fn trash_dir(root: &Path) -> Option<PathBuf> {
    let t = home().join(".Trash");
    let same = fs::metadata(&t).ok()?.dev() == fs::metadata(root).ok()?.dev();
    same.then_some(t)
}

fn unique_in(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = name.rsplit_once('.').map(|(s, e)| (s, format!(".{e}"))).unwrap_or((name, String::new()));
    (1..).map(|i| dir.join(format!("{stem} (mootui {i}){ext}"))).find(|p| !p.exists()).unwrap()
}

/// Remove folders left empty by moves (a lone .DS_Store counts as empty),
/// walking up but never past the library root.
fn prune_empty(root: &Path, mut dir: PathBuf) {
    while dir.starts_with(root) && dir != root {
        let Ok(entries) = fs::read_dir(&dir) else { return };
        let names: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
        if names.iter().any(|n| n != ".DS_Store") {
            return;
        }
        let _ = fs::remove_file(dir.join(".DS_Store"));
        if fs::remove_dir(&dir).is_err() {
            return;
        }
        if !dir.pop() {
            return;
        }
    }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()
}

fn reconcile(plan: &Plan, cancel: &AtomicBool, tx: &Sender<XferMsg>) -> Result<Summary> {
    let root = &plan.root;
    let cat = Catalog::open(root)?;
    let id = cat.begin_import(plan.kind.key(), &root.to_string_lossy(), None, "move", &plan.layout)?;
    let (mut log, log_path, stamp) = open_log(plan, plan.kind.key())?;
    header(&mut log, plan, "MOVE (within library)");

    let mut s = Summary {
        kind: plan.kind,
        mode: Mode::Move,
        transferred: 0,
        bytes: 0,
        failed: 0,
        warnings: 0,
        duplicates: plan.duplicates().count(),
        trashed: 0,
        conflicts: plan.conflicts().count(),
        undated: plan.undated.len(),
        cancelled: false,
        log_path,
        csv_path: None,
        note: None,
    };

    let moves: Vec<&Item> = plan.new_items().collect();
    let mut vacated: Vec<PathBuf> = Vec::new();
    for (k, item) in moves.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            s.cancelled = true;
            log.line("CANCELLED", &format!("{} move(s) not done", moves.len() - k));
            break;
        }
        let _ = tx.send(XferMsg::Progress { files_done: k, bytes_done: s.bytes, current: item.name.clone(), phase: "moving" });
        let (from, to) = (rel(root, &item.src), item.rel_dest.clone());
        let r = (|| -> Result<(), String> {
            let dir = item.dest.parent().ok_or("bad destination")?;
            fs::create_dir_all(dir).map_err(|e| format!("mkdir: {e}"))?;
            let same_file = matches!((fs::metadata(&item.src), fs::metadata(&item.dest)),
                (Ok(a), Ok(b)) if a.ino() == b.ino() && a.dev() == b.dev());
            if item.dest.exists() && !same_file {
                return Err("exists at destination".into());
            }
            fs::rename(&item.src, &item.dest).map_err(|e| format!("rename: {e}"))?;
            Ok(())
        })();
        let (tag, text) = match r {
            Ok(()) => {
                if let Err(e) = cat.rename_rel(&from, &to).and_then(|_| cat.record_move(id, &from, &to, "move", None)) {
                    s.warnings += 1;
                    log.line("WARNING", &format!("catalog: {e}"));
                }
                s.transferred += 1;
                s.bytes += item.size;
                if let Some(p) = item.src.parent() {
                    vacated.push(p.to_path_buf());
                }
                ("MOVED", format!("{from}  ->  {to}"))
            }
            Err(e) => {
                s.failed += 1;
                ("FAILED", format!("{from}  ->  {to}  ({e})"))
            }
        };
        log.line(tag, &text);
        let _ = tx.send(XferMsg::Line(format!("{tag:<9} {text}")));
    }

    let mut trashed = Vec::new();
    if plan.trash_dups && !s.cancelled {
        let trash = trash_dir(root);
        for d in plan.duplicates().filter(|d| matches!(d.status, Status::Duplicate { trashable: true, .. })) {
            let from = rel(root, &d.src);
            let Some(t) = &trash else {
                s.warnings += 1;
                log.line("WARNING", &format!("{from}: library is not on the home disk, cannot use Trash; left in place"));
                continue;
            };
            let dest = unique_in(t, &d.name);
            match fs::rename(&d.src, &dest) {
                Ok(()) => {
                    let _ = cat.delete_rel(&from);
                    let _ = cat.record_move(id, &from, "", "trash", Some(&dest.to_string_lossy()));
                    s.trashed += 1;
                    trashed.push(d.src.clone());
                    if let Some(p) = d.src.parent() {
                        vacated.push(p.to_path_buf());
                    }
                    log.line("TRASHED", &format!("{from}  ->  {}", tilde(&dest)));
                    let _ = tx.send(XferMsg::Line(format!("TRASHED   {from}")));
                }
                Err(e) => {
                    s.failed += 1;
                    log.line("FAILED", &format!("{from}  ->  Trash  ({e})"));
                }
            }
        }
    }
    s.csv_path = write_dup_csv(plan, &stamp, &trashed)?;

    vacated.sort();
    vacated.dedup();
    for d in vacated {
        prune_empty(root, d);
    }

    footer(&mut log, &s);
    cat.finish_import(
        id,
        &format!("{{\"moved\":{},\"failed\":{},\"trashed\":{},\"cancelled\":{}}}", s.transferred, s.failed, s.trashed, s.cancelled),
    )?;
    Ok(s)
}

// ---------------------------------------------------------------------------
// Undo
// ---------------------------------------------------------------------------

/// Reverse the most recent tidy/reorganize. Files that have since moved
/// again, or whose old spot is now taken, are left where they are.
pub fn undo(root: &Path, cfg: &LibraryConfig) -> Result<String> {
    let cat = Catalog::open(root)?;
    let Some((id, kind, started)) = cat.last_undoable()? else {
        bail!("Nothing to undo");
    };
    let dir = root.join("LOG");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}_undo.log", Local::now().format("%Y-%m-%d_%H%M%S")));
    let mut log = Log { w: BufWriter::new(File::create(&path)?) };
    log.raw(&format!("mootui undo of {kind} started {started}"));
    log.raw("");

    let (mut ok, mut skipped) = (0, 0);
    for m in cat.moves_for(id)? {
        let (src, dest) = match m.action.as_str() {
            "trash" => (PathBuf::from(m.trash_path.clone().unwrap_or_default()), root.join(&m.from)),
            _ => (root.join(&m.to), root.join(&m.from)),
        };
        if !src.exists() || dest.exists() {
            skipped += 1;
            log.line("SKIPPED", &format!("{}  (source gone or original spot taken)", m.from));
            continue;
        }
        if let Some(p) = dest.parent() {
            let _ = fs::create_dir_all(p);
        }
        match fs::rename(&src, &dest) {
            Ok(()) => {
                if m.action == "move" {
                    let _ = cat.rename_rel(&m.to, &m.from);
                    if let Some(p) = src.parent() {
                        prune_empty(root, p.to_path_buf());
                    }
                }
                ok += 1;
                log.line("RESTORED", &m.from);
            }
            Err(e) => {
                skipped += 1;
                log.line("FAILED", &format!("{}  ({e})", m.from));
            }
        }
    }
    cat.mark_undone(id)?;
    // Files restored from the Trash were dropped from the catalog; re-adopt.
    let mut et = crate::exif::Reader::new();
    crate::library::sync(&cat, cfg, &mut et, &Progress::silent())?;
    log.raw("");
    log.raw(&format!("result     {ok} restored, {skipped} skipped"));
    Ok(format!("Undid {kind} from {started}: {ok} restored, {skipped} skipped — log {}", tilde(&path)))
}
