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

use std::collections::HashMap;
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
use crate::second;
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
    pub scan_errors: usize,
    pub cancelled: bool,
    pub log_path: PathBuf,
    pub csv_path: Option<PathBuf>,
    pub note: Option<String>,
    /// Import: the second-copy folder, if one was made.
    pub second: Option<PathBuf>,
    /// Files whose second copy failed (they are in the library).
    pub second_failed: usize,
    /// Files already in the library that were missing from the second copy
    /// (e.g. after an interrupted import) and were copied there.
    pub second_caught_up: usize,
}

pub enum XferMsg {
    Progress { files_done: usize, bytes_done: u64, current: String, phase: &'static str },
    Line(String),
    Done(Result<Summary, String>),
}

const CHUNK: usize = 8 << 20;
/// Chunks buffered between reader and writer: 8 x 8 MB.
const QUEUE: usize = 8;

pub fn run(plan: Plan, mode: Mode, backup_dir: Option<PathBuf>, second: Option<PathBuf>, cancel: Arc<AtomicBool>, tx: Sender<XferMsg>) {
    let r = match plan.kind {
        Kind::Import => import(&plan, mode, second.as_deref(), &cancel, &tx),
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

fn header(log: &mut Log, plan: &Plan, mode: &str, second: Option<&Path>) {
    log.raw(&format!("mootui {} log", plan.kind.key()));
    log.raw(&format!("started    {}", now()));
    log.raw(&format!("source     {}", plan.source.display()));
    log.raw(&format!("library    {}", plan.root.display()));
    log.raw(&format!("layout     {}", plan.layout));
    log.raw(&format!("mode       {mode}"));
    if let Some(sec) = second {
        log.raw(&format!("2nd copy   {}", sec.display()));
    }
    if plan.kind == Kind::Import {
        log.raw(&format!("duplicates {}", plan.dup_mode.label()));
    }
    log.raw(&format!(
        "plan       {} to {}, {} duplicate, {} conflict, {} undated, {} non-media ignored, {} already in place{}",
        plan.new_items().count(),
        if plan.kind == Kind::Import { "transfer" } else { "move" },
        plan.duplicates().count(),
        plan.conflicts().count(),
        plan.undated.len(),
        plan.ignored,
        plan.in_place,
        if plan.scan_errors.is_empty() { String::new() } else { format!(", {} scan error(s)", plan.scan_errors.len()) },
    ));
    log.raw("");
    for err in &plan.scan_errors {
        log.line("SCAN ERR", err);
    }
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
    if let Some(sec) = &s.second {
        log.raw(&format!("2nd copy   {}: {} failed", sec.display(), s.second_failed));
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

enum Chunk {
    Data(Vec<u8>, usize),
    End(std::io::Result<()>),
}

/// Reads files in order into a bounded queue. One reader: a card is fastest
/// read sequentially. Reuses buffers returned on `pool` so large imports don't
/// allocate and free an 8 MB buffer per file.
fn spawn_reader(paths: Vec<PathBuf>, cancel: Arc<AtomicBool>) -> (Receiver<Chunk>, Sender<Vec<u8>>) {
    let (tx, rx) = sync_channel(QUEUE);
    let (pool_tx, pool_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        for p in paths {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let r = (|| -> std::io::Result<()> {
                let mut f = File::open(&p)?;
                loop {
                    let mut buf = pool_rx.try_recv().unwrap_or_else(|_| vec![0u8; CHUNK]);
                    let n = read_full(&mut f, &mut buf)?;
                    if n == 0 {
                        return Ok(());
                    }
                    if tx.send(Chunk::Data(buf, n)).is_err() {
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
    (rx, pool_tx)
}

/// Checks before anything is written: the second-copy folder is there and
/// doesn't overlap the library or the source, and each disk has room.
fn preflight(plan: &Plan, second: Option<&Path>, need_lib: u64, need_second: u64) -> Result<()> {
    let mut need: Vec<(u64, &Path, u64)> = vec![(fs::metadata(&plan.root)?.dev(), &plan.root, need_lib)];
    if let Some(sec) = second {
        second::check_folder(sec, &plan.root, Some(&plan.source))?;
        let dev = fs::metadata(sec)?.dev();
        match need.iter_mut().find(|(d, _, _)| *d == dev) {
            Some(n) => n.2 += need_second,
            None => need.push((dev, sec, need_second)),
        }
    }
    for (_, p, bytes) in need {
        if let Some(free) = free_bytes(p) {
            if bytes > free {
                bail!("Not enough space on the disk holding {}: need {}, {} free", tilde(p), human_bytes(bytes), human_bytes(free));
            }
        }
    }
    Ok(())
}

/// Space available to this user on the disk holding `p`.
pub(crate) fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: c is a valid NUL-terminated path and st a valid out-pointer.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(c.as_ptr(), &mut st) } == 0).then_some(st.f_bavail as u64 * st.f_bsize as u64)
}

fn import(plan: &Plan, mode: Mode, second: Option<&Path>, cancel: &AtomicBool, tx: &Sender<XferMsg>) -> Result<Summary> {
    let dest_dev = fs::metadata(&plan.root)?.dev();
    let new: Vec<&Item> = plan.new_items().collect();
    // A move within the library's disk is a rename. Not with a second copy:
    // that needs the bytes, so every file is read.
    let renames: Vec<bool> = new
        .iter()
        .map(|i| second.is_none() && mode == Mode::Move && fs::metadata(&i.src).map(|m| m.dev() == dest_dev).unwrap_or(false))
        .collect();
    let need_lib = new.iter().zip(&renames).filter(|(_, r)| !**r).map(|(i, _)| i.size).sum();
    preflight(plan, second, need_lib, plan.new_bytes())?;
    let _second_lock = second.map(second::lock).transpose()?;

    let cat = Catalog::open(&plan.root)?;
    let vol = plan.volume.as_ref().map(|(n, u)| (n.as_str(), u.as_str()));
    let import_id = cat.begin_import("import", &plan.source.to_string_lossy(), vol, mode.key(), &plan.layout)?;
    let (mut log, log_path, stamp) = open_log(plan, "import")?;
    header(&mut log, plan, mode.label(), second);
    let csv_path = write_dup_csv(plan, &stamp, &[])?;

    let cancel_arc = Arc::new(AtomicBool::new(false));
    let to_read: Vec<PathBuf> = new.iter().zip(&renames).filter(|(_, r)| !**r).map(|(i, _)| i.src.clone()).collect();
    let (rx, pool_tx) = spawn_reader(to_read, cancel_arc.clone());

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
        scan_errors: plan.scan_errors.len(),
        cancelled: false,
        log_path,
        csv_path,
        note: None,
        second: second.map(Path::to_path_buf),
        second_failed: 0,
        second_caught_up: 0,
    };

    let mut group_sizes: HashMap<usize, usize> = HashMap::new();
    for item in &new {
        *group_sizes.entry(item.group).or_default() += 1;
    }

    let (ftx, frx) = sync_channel::<Written>(FINISH_QUEUE);
    let (cat, mut log, s) = thread::scope(|sc| {
        let fin = Finisher {
            new: &new,
            mode,
            cat,
            log,
            s,
            import_id,
            tx,
            open_tx: false,
            rows: 0,
            pending: Vec::new(),
            group_sizes,
            group_held: HashMap::new(),
            last: None,
            last2: None,
        };
        let handle = sc.spawn(move || fin.run(frx));

        // Writer: stream each file to its temp path(s) and hand it on.
        // Flushing, verifying and cataloguing happen on the finisher, so this
        // loop keeps draining the reader queue.
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
            let w = if renames[k] {
                Written::Done { k, result: rename_in(item), copied: false }
            } else {
                copy_in(k, item, second, &rx, &pool_tx, &send)
            };
            bytes_done = base + item.size;
            if ftx.send(w).is_err() {
                break;
            }
        }
        drop(ftx);
        drop(pool_tx);
        drop(rx);
        handle.join().expect("finisher thread panicked")
    });

    let mut s = s;
    if let (Some(sec), false) = (second, s.cancelled) {
        catch_up_duplicates(plan, sec, cancel, tx, &mut log, &mut s);
    }

    let _ = tx.send(XferMsg::Progress { files_done: s.transferred + s.failed, bytes_done: s.bytes, current: String::new(), phase: "done" });
    footer(&mut log, &s);
    cat.finish_import(
        import_id,
        &format!(
            "{{\"transferred\":{},\"failed\":{},\"duplicates\":{},\"conflicts\":{},\"second_copy_failed\":{},\"cancelled\":{}}}",
            s.transferred, s.failed, s.duplicates, s.conflicts, s.second_failed, s.cancelled
        ),
    )?;
    Ok(s)
}

/// Files this import skipped as already in the library may still be missing
/// from the second copy: the run that brought them in was interrupted, or
/// their second copy failed, or the second copy was set up later. Copy those
/// from the library (not the card).
fn catch_up_duplicates(plan: &Plan, sec: &Path, cancel: &AtomicBool, tx: &Sender<XferMsg>, log: &mut Log, s: &mut Summary) {
    let mut rels: Vec<String> = plan
        .duplicates()
        .filter_map(|d| match &d.status {
            Status::Duplicate { existing, .. } => existing.strip_prefix(&plan.root).ok().map(|r| r.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    rels.sort();
    rels.dedup();
    if rels.is_empty() {
        return;
    }
    let files_done = s.transferred + s.failed;
    let r = second::catch_up(&plan.root, sec, &rels, &|| cancel.load(Ordering::Relaxed), &mut |rel, _| {
        log.line("2ND COPY", &format!("{rel}  (already in the library; was missing from the second copy)"));
        let _ = tx.send(XferMsg::Line(format!("2ND COPY  {rel}")));
        let _ = tx.send(XferMsg::Progress { files_done, bytes_done: s.bytes, current: rel.to_string(), phase: "second copy" });
    });
    match r {
        Ok(r) => {
            s.second_caught_up = r.copied;
            for rel in &r.different {
                s.second_failed += 1;
                log.line("WARNING", &format!("{rel}: a different file is already in the second copy; left alone"));
            }
            for (rel, e) in &r.failed {
                s.second_failed += 1;
                log.line("WARNING", &format!("{rel}: second copy failed: {e}"));
            }
            s.warnings += r.different.len() + r.failed.len();
        }
        Err(e) => {
            s.warnings += 1;
            log.line("WARNING", &format!("second copy catch-up: {e:#}"));
        }
    }
}

type Hashes = (Option<[u8; 32]>, Option<[u8; 32]>);

/// Files handed from the writer to the finisher, in plan order.
enum Written {
    /// Fully streamed to `tmp` (and to the second copy, if any); still to be
    /// flushed, verified and renamed.
    Copied { k: usize, tmp: PathBuf, file: File, hash: [u8; 32], head: [u8; 32], second: Option<Second> },
    /// Already settled: a same-disk rename, or a copy that failed.
    Done { k: usize, result: Result<Hashes, String>, copied: bool },
    Cancelled { remaining: usize },
}

/// A file's copy in the second-copy folder.
enum Second {
    /// Streamed to `tmp`; flushed, verified and renamed like the main copy.
    Tmp { tmp: PathBuf, file: File, dest: PathBuf },
    /// Something is already at that path: fine if it has the same content.
    Existing(PathBuf),
    Failed(String),
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
    group_ok: bool,
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
    /// Number of New items in each photo group; Move mode holds deletion of a
    /// multi-file group until every member of that group is verified & durable.
    group_sizes: HashMap<usize, usize>,
    group_held: HashMap<usize, Vec<(Pending, bool)>>,
    /// Most recent copy on each disk (library, second copy), kept open so
    /// one F_FULLFSYNC on it flushes that drive's cache for everything
    /// fsync'd before it.
    last: Option<File>,
    last2: Option<File>,
}

impl Finisher<'_> {
    fn run(mut self, rx: Receiver<Written>) -> (Catalog, Log, Summary) {
        for w in rx {
            match w {
                Written::Copied { k, tmp, file, hash, head, second } => {
                    let item = self.new[k];
                    // The two disks verify at the same time.
                    let (r, r2) = thread::scope(|sc| {
                        let h = second.map(|sec| sc.spawn(move || finish_second(item, sec, hash)));
                        let r = finish_copy(item, &tmp, &file, hash, head);
                        (r, h.map(|h| h.join().unwrap_or_else(|_| Err("thread panicked".into()))))
                    });
                    if r.is_err() {
                        let _ = fs::remove_file(&tmp);
                    } else {
                        self.last = Some(file);
                    }
                    let r2 = match r2 {
                        Some(Ok(f)) => {
                            if f.is_some() {
                                self.last2 = f;
                            }
                            Some(Ok(()))
                        }
                        Some(Err(e)) => Some(Err(e)),
                        None => None,
                    };
                    self.record(k, r, true, r2);
                }
                Written::Done { k, result, copied } => self.record(k, result, copied, None),
                Written::Cancelled { remaining } => {
                    self.flush(false);
                    self.s.cancelled = true;
                    self.log.line("CANCELLED", &format!("{remaining} file(s) not transferred"));
                }
            }
        }
        self.flush(true);
        // Any multi-file group still held was never completed (e.g. cancelled);
        // keep all of its source files.
        let mut leftover: Vec<(usize, Vec<(Pending, bool)>)> = self.group_held.drain().collect();
        leftover.sort_by_key(|(g, _)| *g);
        for (_, members) in leftover {
            self.settle_group(members, false);
        }
        (self.cat, self.log, self.s)
    }

    fn record(&mut self, k: usize, r: Result<Hashes, String>, copied: bool, second: Option<Result<(), String>>) {
        let item = self.new[k];
        let second_err = second.and_then(Result::err);
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
            let mut warn: Vec<String> = rec.err().map(|e| format!("catalog: {e}")).into_iter().collect();
            if let Some(e) = &second_err {
                self.s.second_failed += 1;
                warn.push(format!("second copy failed: {e}{}", if self.mode == Mode::Move { "; source kept" } else { "" }));
            }
            (!warn.is_empty()).then(|| warn.join("; "))
        });
        // A source goes only once every copy of it (and its photo group) is verified.
        let group_ok = outcome.is_ok() && second_err.is_none();
        let delete_source = self.mode == Mode::Move && copied && group_ok;
        self.pending.push(Pending { k, delete_source, group_ok, outcome });
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
            // std's sync_all is F_FULLFSYNC on macOS: flushes the drive cache.
            for f in [self.last.take(), self.last2.take()].into_iter().flatten() {
                durable &= f.sync_all().is_ok();
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
            let g = self.new[p.k].group;
            let gsize = self.group_sizes.get(&g).copied().unwrap_or(1);
            if self.mode == Mode::Move && gsize > 1 {
                let entry = self.group_held.entry(g).or_default();
                entry.push((p, durable));
                if entry.len() == gsize {
                    if let Some(members) = self.group_held.remove(&g) {
                        self.settle_group(members, true);
                    }
                }
            } else {
                self.emit_one(p, durable, None);
            }
        }
    }

    fn settle_group(&mut self, members: Vec<(Pending, bool)>, complete: bool) {
        let all_ok = complete && members.iter().all(|(p, d)| p.group_ok && (!p.delete_source || *d));
        let keep_reason = if all_ok {
            None
        } else if complete {
            Some("another file in this photo group failed; source kept")
        } else {
            Some("photo group incomplete; source kept")
        };
        for (p, d) in members {
            self.emit_one(p, d, keep_reason);
        }
    }

    fn emit_one(&mut self, p: Pending, durable: bool, keep_reason: Option<&str>) {
        let item = self.new[p.k];
        let (tag, text) = match p.outcome {
            Ok(mut warn) => {
                if p.delete_source {
                    let add = if let Some(reason) = keep_reason {
                        Some(reason.to_string())
                    } else if !durable {
                        Some("flush to disk failed; source kept".into())
                    } else if let Err(e) = fs::remove_file(&item.src) {
                        Some(format!("could not delete source: {e}"))
                    } else {
                        None
                    };
                    if let Some(msg) = add {
                        warn = Some(match warn {
                            Some(w) => format!("{w}; {msg}"),
                            None => msg,
                        });
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
/// stream stays aligned with the item list. Each chunk is written to the
/// library and, if set, the second copy: the card is read once.
fn copy_in(
    k: usize,
    item: &Item,
    second_root: Option<&Path>,
    rx: &Receiver<Chunk>,
    pool_tx: &Sender<Vec<u8>>,
    send: &dyn Fn(u64, &'static str),
) -> Written {
    let Some(dir) = item.dest.parent() else {
        // Still drain this file's chunks.
        while let Ok(Chunk::Data(buf, _)) = rx.recv() {
            let _ = pool_tx.send(buf);
        }
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
        Ok(()) => {
            // A partial file here is left by a crashed run: the library lock
            // rules out a live one.
            let _ = fs::remove_file(&tmp);
            match OpenOptions::new().write(true).create_new(true).open(&tmp) {
                Ok(f) => Some(f),
                Err(e) => {
                    err = Some(format!("create: {e}"));
                    None
                }
            }
        }
    };
    let mut second = if err.is_none() { second_root.map(|r| open_second(r, item)) } else { None };

    let mut h = blake3::Hasher::new();
    let mut quick = HeadHasher::new();
    let mut n = 0u64;
    loop {
        match rx.recv() {
            Ok(Chunk::Data(buf, len)) => {
                let data = &buf[..len];
                if err.is_none() {
                    h.update(data);
                    quick.update(data);
                    if let Some(f) = w.as_mut() {
                        if let Err(e) = f.write_all(data) {
                            err = Some(format!("write: {e}"));
                        }
                    }
                    let failed = match &mut second {
                        Some(Second::Tmp { file, .. }) => file.write_all(data).err(),
                        _ => None,
                    };
                    if let Some(e) = failed {
                        drop_second(&mut second, format!("write: {e}"));
                    }
                }
                n += len as u64;
                let _ = pool_tx.send(buf);
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
        (None, Some(file)) => Written::Copied { k, tmp, file, hash: *h.finalize().as_bytes(), head: quick.finalize(), second },
        (err, _) => {
            let _ = fs::remove_file(&tmp);
            drop_second(&mut second, String::new());
            Written::Done { k, result: Err(err.unwrap_or_else(|| "no file".into())), copied: true }
        }
    }
}

/// Where this file goes in the second copy: the same path as in the library.
fn open_second(root: &Path, item: &Item) -> Second {
    let dest = root.join(&item.rel_dest);
    let Some(dir) = dest.parent() else { return Second::Failed("bad destination".into()) };
    if let Err(e) = fs::create_dir_all(dir) {
        return Second::Failed(format!("mkdir: {e}"));
    }
    if dest.exists() {
        return Second::Existing(dest);
    }
    let tmp = dir.join(format!(".{}.mootui-partial", item.name));
    let _ = fs::remove_file(&tmp); // left by a crashed run; the second-copy lock rules out a live one
    match OpenOptions::new().write(true).create_new(true).open(&tmp) {
        Ok(file) => Second::Tmp { tmp, file, dest },
        Err(e) => Second::Failed(format!("create: {e}")),
    }
}

/// Give up on the second copy of this file, removing its partial copy.
fn drop_second(second: &mut Option<Second>, why: String) {
    if let Some(Second::Tmp { tmp, .. }) = second {
        let _ = fs::remove_file(tmp);
    }
    if second.is_some() {
        *second = Some(Second::Failed(why));
    }
}

/// Copy the source's modified, accessed (and on macOS created) times.
pub(crate) fn copy_times(src: &Path, f: &File) {
    if let Ok(meta) = fs::metadata(src) {
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
}

/// Runs on the finisher thread: copy times, fsync, verify, rename into place.
/// Both hashes were taken from the stream; the full re-read confirms the
/// copy holds those same bytes, so the quick hash holds for it too.
fn finish_copy(item: &Item, tmp: &Path, f: &File, streamed: [u8; 32], head: [u8; 32]) -> Result<Hashes, String> {
    copy_times(&item.src, f);
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

/// The same for the second copy, alongside finish_copy. Returns the file
/// when a new copy was written, for the batch's drive flush.
fn finish_second(item: &Item, second: Second, streamed: [u8; 32]) -> Result<Option<File>, String> {
    match second {
        Second::Failed(e) => Err(e),
        Second::Existing(dest) => match hash_file(&dest) {
            Ok(h) if h == streamed => Ok(None),
            Ok(_) => Err(format!("a different file is already at {}", dest.display())),
            Err(e) => Err(format!("reading {}: {e}", dest.display())),
        },
        Second::Tmp { tmp, file, dest } => {
            let r = (|| {
                copy_times(&item.src, &file);
                fsync(&file).map_err(|e| format!("sync: {e}"))?;
                if hash_file(&tmp).map_err(|e| format!("verify: {e}"))? != streamed {
                    return Err("verification failed: copy does not match source".to_string());
                }
                if dest.exists() {
                    return Err("exists at destination".into());
                }
                fs::rename(&tmp, &dest).map_err(|e| format!("rename: {e}"))
            })();
            match r {
                Ok(()) => Ok(Some(file)),
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    Err(e)
                }
            }
        }
    }
}

/// Plain fsync(2): hands the data to the drive. Unlike std's sync_all on
/// macOS it does not also flush the drive's cache (F_FULLFSYNC), which is
/// slow per file; Finisher::flush does that once per batch instead.
pub(crate) fn fsync(f: &File) -> std::io::Result<()> {
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
    header(&mut log, plan, "MOVE (within library)", None);

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
        scan_errors: plan.scan_errors.len(),
        cancelled: false,
        log_path,
        csv_path: None,
        note: None,
        second: None,
        second_failed: 0,
        second_caught_up: 0,
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
