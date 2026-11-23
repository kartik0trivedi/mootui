//! mootui -- import photos from a camera card (or any folder) into a
//! catalogued library, skipping duplicates; tidy files already there.
//!
//! Usage:
//!   mootui [--library DIR] [SOURCE]           interactive TUI
//!   mootui --plan SOURCE                      preview an import, change nothing
//!   mootui --yes [--move] [--verify] SOURCE   import without the UI (default: copy)
//!   mootui --tidy [--yes] [--trash]           sort loose files already in the library
//!   mootui --reorganize [--yes]               re-sort the whole library to its layout
//!   mootui --undo                             undo the last tidy/reorganize
//!   mootui --index                            build/refresh the catalog only

mod catalog;
mod config;
mod exif;
mod exif_native;
mod jobs;
mod layout;
mod library;
mod media;
mod plan;
mod second;
mod transfer;
mod util;
mod volumes;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::fs::File;
use std::io::stdout;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row, Table, TableState, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};

use catalog::Catalog;
use config::{AppConfig, DupMode, LibraryConfig};
use jobs::{JobMsg, Progress};
use plan::{Kind, Plan, Status};
use transfer::{Mode, Summary, XferMsg};
use util::{clean_typed_path, human_bytes, human_duration, is_cloud_synced, tilde};

const ACCENT: Color = Color::Cyan;

// ---------------------------------------------------------------------------
// Library handle: catalog connection + exclusive lock
// ---------------------------------------------------------------------------

struct Library {
    root: PathBuf,
    cfg: LibraryConfig,
    cat: Catalog,
    _lock: File,
    /// Runs that never finished, found when the library was opened.
    notice: Option<String>,
    /// Cached count of present files, refreshed after jobs/transfers.
    present_count: i64,
}

fn check_library_dir(root: &Path) -> Result<()> {
    if !root.is_dir() {
        bail!("Library folder not found: {}", root.display());
    }
    if is_cloud_synced(root) {
        bail!("{} is in a cloud-synced folder; a live catalog there can be corrupted by sync. Pick a local folder.", tilde(root));
    }
    Ok(())
}

/// Open (creating if needed) the catalog and take the library lock, so two
/// mootui runs can never move files in the same library at once.
fn attach(root: &Path) -> Result<Library> {
    check_library_dir(root)?;
    let cat = Catalog::open(root)?;
    let lock = File::create(root.join(catalog::DIR).join("lock"))?;
    if lock.try_lock().is_err() {
        bail!("Another mootui is already using {}", tilde(root));
    }
    if !LibraryConfig::path(root).exists() {
        LibraryConfig::default().save(root)?;
    }
    let cfg = LibraryConfig::load(root);
    let notice = interrupted_notice(&cat.close_interrupted()?);
    let present_count = cat.count_present();
    Ok(Library { root: root.to_path_buf(), cfg, cat, _lock: lock, notice, present_count })
}

/// What to tell the user about runs that never finished.
fn interrupted_notice(runs: &[catalog::Interrupted]) -> Option<String> {
    let lines: Vec<String> = runs
        .iter()
        .map(|r| {
            if r.kind == "import" {
                let from = r.volume.clone().unwrap_or_else(|| tilde(Path::new(&r.source)));
                format!(
                    "The import from {from} started {} didn't finish. Import from it again to finish: files already in the library are skipped.",
                    r.started
                )
            } else {
                format!("The {} started {} didn't finish ({} move(s) made). Run it again to finish, or undo it.", r.kind, r.started, r.moves)
            }
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join(" "))
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Args {
    library: Option<PathBuf>,
    source: Option<PathBuf>,
    plan_only: bool,
    yes: bool,
    mv: bool,
    verify: bool,
    tidy: bool,
    reorganize: bool,
    trash: bool,
    undo: bool,
    index: bool,
    second_copy: Option<PathBuf>,
    no_second_copy: bool,
    catch_up: bool,
}

const HELP: &str = "mootui -- import photos into a catalogued library, skipping duplicates

usage:
  mootui [--library DIR] [SOURCE]           interactive
  mootui --plan SOURCE                      preview an import, change nothing
  mootui --yes [--move] [--verify] SOURCE   import without the UI (default: copy)
        [--second-copy DIR | --no-second-copy]  also write a verified copy to DIR
                                            (default: the Second copy setting)
  mootui --tidy [--yes] [--trash]           sort loose files already in the library
  mootui --reorganize [--yes]               re-sort the whole library to its layout
  mootui --undo                             undo the last tidy/reorganize
  mootui --index                            build/refresh the catalog only
  mootui --catch-up-second-copy [--second-copy DIR]
                                            copy library files missing from the second copy

The library defaults to ~/Pictures (change it in Settings or ~/.config/mootui/config.toml).
Catalog and library settings live in <library>/.mootui/, logs in <library>/LOG/.";

fn parse_args() -> Result<Args> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--library" | "--dest" => a.library = Some(clean_typed_path(&it.next().unwrap_or_default())),
            "--plan" => a.plan_only = true,
            "--yes" => a.yes = true,
            "--move" => a.mv = true,
            "--copy" => a.mv = false,
            "--verify" => a.verify = true,
            "--tidy" => a.tidy = true,
            "--reorganize" => a.reorganize = true,
            "--trash" => a.trash = true,
            "--undo" => a.undo = true,
            "--index" => a.index = true,
            "--second-copy" => a.second_copy = Some(clean_typed_path(&it.next().unwrap_or_default())),
            "--no-second-copy" => a.no_second_copy = true,
            "--catch-up-second-copy" => a.catch_up = true,
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            s if s.starts_with('-') => bail!("unknown option: {s} (see --help)"),
            s => a.source = Some(clean_typed_path(s)),
        }
    }
    Ok(a)
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let app_cfg = AppConfig::load();
    let root = args.library.clone().unwrap_or_else(|| app_cfg.library_path());

    if args.plan_only || args.yes || args.tidy || args.reorganize || args.undo || args.index || args.catch_up {
        return headless(args, app_cfg, root);
    }

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableBracketedPaste);
    let res = App::new(app_cfg, root, args.source).run(&mut terminal);
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    res
}

fn wait_job(rx: &Receiver<JobMsg>) -> Result<JobMsg> {
    loop {
        match rx.recv()? {
            JobMsg::Phase(p, _) => eprintln!("… {p}"),
            JobMsg::Tick(_) => {}
            done => return Ok(done),
        }
    }
}

fn headless(args: Args, app_cfg: AppConfig, root: PathBuf) -> Result<()> {
    if !catalog::exists(&root) {
        eprintln!("creating catalog in {}", tilde(&root.join(catalog::DIR)));
    }
    // Holding `lib` keeps the library lock for the whole run.
    let lib = attach(&root)?;
    if let Some(n) = &lib.notice {
        eprintln!("note: {n}");
    }
    let mut cfg = lib.cfg.clone();
    if args.trash {
        cfg.trash_duplicates = true;
    }

    if args.undo {
        println!("{}", transfer::undo(&root, &cfg)?);
        return Ok(());
    }

    let (tx, rx) = mpsc::channel();
    let p = Progress::new(tx, Arc::new(AtomicBool::new(false)));

    if args.catch_up {
        let Some(sec) = args.second_copy.clone().or_else(|| app_cfg.second_copy_path()) else {
            bail!("No second copy folder: set one in Settings or pass --second-copy DIR");
        };
        let (r, c) = (root.clone(), cfg.clone());
        thread::spawn(move || {
            let res = second::catch_up_library(&r, &c, &sec, &p).map_err(|e| format!("{e:#}"));
            p.send(JobMsg::Info(res));
        });
        if let JobMsg::Info(r) = wait_job(&rx)? {
            println!("{}", r.map_err(anyhow::Error::msg)?);
        }
        return Ok(());
    }

    if args.index {
        let r = root.clone();
        thread::spawn(move || {
            let mut et = exif::Reader::new();
            let res = Catalog::open(&r)
                .map_err(|e| e.to_string())
                .and_then(|c| library::sync(&c, &cfg, &mut et, &p).map_err(|e| e.to_string()).map(|s| index_message(&s)));
            p.send(JobMsg::Info(res));
        });
        if let JobMsg::Info(r) = wait_job(&rx)? {
            println!("{}", r.map_err(anyhow::Error::msg)?);
        }
        return Ok(());
    }

    let kind = if args.reorganize {
        Kind::Reorganize
    } else if args.tidy {
        Kind::Tidy
    } else {
        Kind::Import
    };
    if kind == Kind::Import && args.source.is_none() {
        bail!("--plan/--yes need a SOURCE");
    }
    let dup_mode = if args.verify { DupMode::Verified } else { cfg.dup_mode() };
    let r = root.clone();
    let src = args.source.clone();
    thread::spawn(move || {
        let res = match kind {
            Kind::Import => plan::plan_import(src.as_deref().unwrap(), &r, &cfg, dup_mode, None, &p),
            k => plan::plan_reconcile(&r, &cfg, k, &p),
        };
        p.send(JobMsg::Planned(res.map_err(|e| format!("{e:#}"))));
    });
    let JobMsg::Planned(plan) = wait_job(&rx)? else { bail!("unexpected message") };
    let plan = plan.map_err(anyhow::Error::msg)?;

    for i in &plan.items {
        let (tag, extra) = match &i.status {
            Status::New => (if kind == Kind::Import { "new      " } else { "move     " }, String::new()),
            Status::Duplicate { existing, rule, trashable, .. } => (
                "duplicate",
                format!("  == {} ({rule}{})", tilde(existing), if *trashable && plan.trash_dups { ", to Trash" } else { "" }),
            ),
            Status::Conflict { reason } => ("conflict ", format!("  ({reason})")),
        };
        let from = i.src.strip_prefix(&plan.source).unwrap_or(&i.src).display().to_string();
        println!("  {tag}  {from:<40} -> {:<32} [{}]{extra}", i.rel_dest, i.date_src);
    }
    for err in &plan.scan_errors {
        eprintln!("  scan err   {err}");
    }
    println!(
        "plan: {} to {} ({}), {} duplicate, {} conflict, {} undated, {} ignored, {} already in place{}",
        plan.new_items().count(),
        if kind == Kind::Import { "import" } else { "move" },
        human_bytes(plan.new_bytes()),
        plan.duplicates().count(),
        plan.conflicts().count(),
        plan.undated.len(),
        plan.ignored,
        plan.in_place,
        if plan.scan_errors.is_empty() { String::new() } else { format!(", {} scan error(s)", plan.scan_errors.len()) }
    );
    if args.plan_only || (kind != Kind::Import && !args.yes) {
        if kind != Kind::Import {
            println!("(preview only; add --yes to apply)");
        }
        return Ok(());
    }

    let mode = if args.mv { Mode::Move } else { Mode::Copy };
    let (tx, rx) = mpsc::channel();
    let backup = app_cfg.backup_path();
    let second = if args.no_second_copy { None } else { args.second_copy.clone().or_else(|| app_cfg.second_copy_path()) };
    if let Some(sec) = &second {
        println!("second copy: {}", tilde(sec));
    }
    thread::spawn(move || transfer::run(plan, mode, backup, second, Arc::new(AtomicBool::new(false)), tx));
    loop {
        match rx.recv()? {
            XferMsg::Line(l) => println!("  {l}"),
            XferMsg::Progress { .. } => {}
            XferMsg::Done(r) => {
                let s = r.map_err(anyhow::Error::msg)?;
                println!("{}", summary_line(&s));
                println!("log: {}", tilde(&s.log_path));
                if let Some(c) = &s.csv_path {
                    println!("duplicates: {}", tilde(c));
                }
                if let Some(n) = &s.note {
                    println!("note: {n}");
                }
                drop(lib);
                return Ok(());
            }
        }
    }
}

fn index_message(s: &library::SyncStats) -> String {
    let err = if s.errors.is_empty() { String::new() } else { format!(", {} scan error(s)", s.errors.len()) };
    format!("Catalog up to date: {} files ({} new or changed, {} missing{err})", s.total, s.added, s.missing)
}

fn summary_line(s: &Summary) -> String {
    let verb = match (s.kind, s.mode) {
        (Kind::Import, Mode::Copy) => "copied",
        _ => "moved",
    };
    format!(
        "{} {verb} ({}), {} failed, {} warnings, {} duplicates{}, {} conflicts{}{}",
        s.transferred,
        human_bytes(s.bytes),
        s.failed,
        s.warnings,
        s.duplicates,
        if s.trashed > 0 { format!(" ({} to Trash)", s.trashed) } else { String::new() },
        s.conflicts,
        if s.scan_errors > 0 { format!(", {} scan error(s)", s.scan_errors) } else { String::new() },
        if s.cancelled { " — CANCELLED" } else { "" }
    ) + &match &s.second {
        Some(sec) => format!(
            "; second copy in {} ({} failed{})",
            tilde(sec),
            s.second_failed,
            if s.second_caught_up > 0 { format!(", {} caught up", s.second_caught_up) } else { String::new() }
        ),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq, Clone, Copy)]
enum Screen {
    Init,
    Pick,
    Input,
    Settings,
    Working,
    Review,
    Transfer,
    Done,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum InputFor {
    Source,
    Library,
    BackupDir,
    SecondCopy,
}

#[derive(Clone, Copy)]
enum Entry {
    Volume(usize),
    Other,
    Tidy,
    Reorganize,
    Undo,
    CatchUpSecond,
}

struct App {
    screen: Screen,
    quit: bool,
    help: bool,
    message: Option<(String, bool)>,

    app_cfg: AppConfig,
    lib: Option<Library>,
    pending_root: PathBuf,

    volumes: Vec<volumes::Volume>,
    /// Cached last-import (started, count) per entry in `volumes`.
    volume_last_imports: Vec<Option<(String, i64)>>,
    pick: ListState,
    last_refresh: Instant,
    input: String,
    input_for: InputFor,
    settings_sel: usize,
    undo_label: Option<String>,

    job_rx: Option<Receiver<JobMsg>>,
    job_cancel: Arc<AtomicBool>,
    job_title: String,
    phase: String,
    phase_total: usize,
    phase_done: usize,
    phase_started: Instant,

    source: Option<PathBuf>,
    source_volume: Option<(String, String)>,
    eject_path: Option<PathBuf>,
    plan: Option<Plan>,
    tab: usize,
    table: TableState,
    mode: Mode,
    dup_mode: DupMode,

    xfer_rx: Option<Receiver<XferMsg>>,
    cancel: Arc<AtomicBool>,
    files_done: usize,
    bytes_done: u64,
    current: String,
    xphase: &'static str,
    samples: VecDeque<(Instant, u64)>,
    xfer_started: Instant,
    xfer_elapsed: Option<Duration>,
    lines: Vec<String>,
    summary: Option<Summary>,
}

impl App {
    fn new(app_cfg: AppConfig, root: PathBuf, source: Option<PathBuf>) -> Self {
        let mode = Mode::parse(&app_cfg.mode);
        let mut app = App {
            screen: Screen::Pick,
            quit: false,
            help: false,
            message: None,
            app_cfg,
            lib: None,
            pending_root: root.clone(),
            volumes: Vec::new(),
            volume_last_imports: Vec::new(),
            pick: ListState::default().with_selected(Some(0)),
            last_refresh: Instant::now(),
            input: String::new(),
            input_for: InputFor::Source,
            settings_sel: 0,
            undo_label: None,
            job_rx: None,
            job_cancel: Arc::new(AtomicBool::new(false)),
            job_title: String::new(),
            phase: String::new(),
            phase_total: 0,
            phase_done: 0,
            phase_started: Instant::now(),
            source: None,
            source_volume: None,
            eject_path: None,
            plan: None,
            tab: 0,
            table: TableState::default().with_selected(Some(0)),
            mode,
            dup_mode: DupMode::Suspected,
            xfer_rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            files_done: 0,
            bytes_done: 0,
            current: String::new(),
            xphase: "",
            samples: VecDeque::new(),
            xfer_started: Instant::now(),
            xfer_elapsed: None,
            lines: Vec::new(),
            summary: None,
        };
        app.open_library(root, false);
        app.refresh_volumes();
        if let (Some(src), true) = (source, app.lib.is_some()) {
            app.start_import_scan(src, None, None);
        }
        app
    }

    fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
                    Event::Paste(s) if self.screen == Screen::Input => self.input.push_str(&s),
                    _ => {}
                }
            }
            self.pump();
        }
        Ok(())
    }

    // --- library -------------------------------------------------------------

    /// Switch to a library. Without a catalog yet, ask first (Init screen).
    fn open_library(&mut self, root: PathBuf, make_default: bool) {
        if let Err(e) = check_library_dir(&root) {
            self.message = Some((e.to_string(), true));
            self.pending_root = root;
            self.screen = Screen::Init;
            return;
        }
        if !catalog::exists(&root) {
            self.pending_root = root;
            self.screen = Screen::Init;
            return;
        }
        self.lib = None; // release the old lock first
        match attach(&root) {
            Ok(lib) => {
                self.dup_mode = lib.cfg.dup_mode();
                let notice = lib.notice.clone();
                self.lib = Some(lib);
                if make_default {
                    self.app_cfg.library = tilde(&root);
                    let _ = self.app_cfg.save();
                    self.message = Some((format!("Library set to {} (now the default)", tilde(&root)), false));
                }
                if let Some(n) = notice {
                    self.message = Some((n, true));
                }
                self.refresh_catalog_cache();
                self.screen = Screen::Pick;
            }
            Err(e) => {
                self.message = Some((e.to_string(), true));
                self.pending_root = root;
                self.screen = Screen::Init;
            }
        }
    }

    fn create_catalog(&mut self) {
        let root = self.pending_root.clone();
        self.lib = None;
        match attach(&root) {
            Ok(lib) => {
                self.dup_mode = lib.cfg.dup_mode();
                self.lib = Some(lib);
                self.app_cfg.library = tilde(&root);
                let _ = self.app_cfg.save();
                self.start_index("Building catalog");
            }
            Err(e) => self.message = Some((e.to_string(), true)),
        }
    }

    fn refresh_catalog_cache(&mut self) {
        if let Some(l) = self.lib.as_mut() {
            l.present_count = l.cat.count_present();
        }
        self.undo_label = self
            .lib
            .as_ref()
            .and_then(|l| l.cat.last_undoable().ok().flatten())
            .map(|(_, kind, started)| format!("Undo last {kind} ({started})"));
        self.refresh_volume_history();
    }

    fn refresh_volume_history(&mut self) {
        self.volume_last_imports = self
            .volumes
            .iter()
            .map(|v| {
                v.uuid
                    .as_deref()
                    .and_then(|u| self.lib.as_ref().and_then(|l| l.cat.last_import_from(u).ok().flatten()))
            })
            .collect();
    }

    fn save_lib_cfg(&mut self) {
        if let Some(l) = &self.lib {
            if let Err(e) = l.cfg.save(&l.root) {
                self.message = Some((format!("Could not save settings: {e}"), true));
            }
        }
    }

    // --- background work ---------------------------------------------------

    fn start_job(&mut self, title: &str, f: impl FnOnce(Progress) + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        self.job_cancel = Arc::new(AtomicBool::new(false));
        let p = Progress::new(tx, self.job_cancel.clone());
        thread::spawn(move || f(p));
        self.job_rx = Some(rx);
        self.job_title = title.to_string();
        self.phase = "Starting".into();
        self.phase_total = 0;
        self.phase_done = 0;
        self.phase_started = Instant::now();
        self.message = None;
        self.screen = Screen::Working;
    }

    fn start_index(&mut self, title: &str) {
        let Some(lib) = &self.lib else { return };
        let (root, cfg) = (lib.root.clone(), lib.cfg.clone());
        self.start_job(title, move |p| {
            let mut et = exif::Reader::new();
            let res = Catalog::open(&root)
                .map_err(|e| e.to_string())
                .and_then(|c| library::sync(&c, &cfg, &mut et, &p).map_err(|e| format!("{e:#}")).map(|s| index_message(&s)));
            p.send(JobMsg::Info(res));
        });
    }

    fn start_import_scan(&mut self, src: PathBuf, volume: Option<(String, String)>, eject: Option<PathBuf>) {
        if !src.is_dir() {
            self.message = Some((format!("Not a folder: {}", src.display()), true));
            self.screen = Screen::Pick;
            return;
        }
        let Some(lib) = &self.lib else { return };
        let (root, cfg, dup) = (lib.root.clone(), lib.cfg.clone(), self.dup_mode);
        self.source = Some(src.clone());
        self.source_volume = volume.clone();
        self.eject_path = eject;
        self.start_job("Scanning source", move |p| {
            let res = plan::plan_import(&src, &root, &cfg, dup, volume, &p);
            p.send(JobMsg::Planned(res.map_err(|e| format!("{e:#}"))));
        });
    }

    fn start_reconcile(&mut self, kind: Kind) {
        let Some(lib) = &self.lib else { return };
        let (root, cfg) = (lib.root.clone(), lib.cfg.clone());
        self.source = Some(root.clone());
        self.source_volume = None;
        self.eject_path = None;
        let title = if kind == Kind::Tidy { "Tidying library" } else { "Planning reorganize" };
        self.start_job(title, move |p| {
            let res = plan::plan_reconcile(&root, &cfg, kind, &p);
            p.send(JobMsg::Planned(res.map_err(|e| format!("{e:#}"))));
        });
    }

    fn start_catch_up(&mut self) {
        let Some(lib) = &self.lib else { return };
        let Some(sec) = self.app_cfg.second_copy_path() else { return };
        let (root, cfg) = (lib.root.clone(), lib.cfg.clone());
        self.start_job("Catching up second copy", move |p| {
            p.send(JobMsg::Info(second::catch_up_library(&root, &cfg, &sec, &p).map_err(|e| format!("{e:#}"))));
        });
    }

    fn start_undo(&mut self) {
        let Some(lib) = &self.lib else { return };
        let (root, cfg) = (lib.root.clone(), lib.cfg.clone());
        self.start_job("Undoing", move |p| {
            p.send(JobMsg::Info(transfer::undo(&root, &cfg).map_err(|e| format!("{e:#}"))));
        });
    }

    fn start_transfer(&mut self) {
        let Some(plan) = self.plan.clone() else { return };
        let (tx, rx) = mpsc::channel();
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let mode = self.mode;
        let backup = self.app_cfg.backup_path();
        let second = self.app_cfg.second_copy_path();
        thread::spawn(move || transfer::run(plan, mode, backup, second, cancel, tx));
        self.xfer_rx = Some(rx);
        self.files_done = 0;
        self.bytes_done = 0;
        self.current.clear();
        self.xphase = "";
        self.samples.clear();
        self.xfer_started = Instant::now();
        self.xfer_elapsed = None;
        self.lines.clear();
        self.summary = None;
        self.message = None;
        self.screen = Screen::Transfer;
    }

    fn pump(&mut self) {
        if let Some(rx) = &self.job_rx {
            let mut finished = false;
            while let Ok(m) = rx.try_recv() {
                match m {
                    JobMsg::Phase(p, total) => {
                        self.phase = p;
                        self.phase_total = total;
                        self.phase_done = 0;
                        self.phase_started = Instant::now();
                    }
                    JobMsg::Tick(n) => self.phase_done = n,
                    JobMsg::Planned(Ok(plan)) => {
                        self.plan = Some(plan);
                        self.tab = 0;
                        self.table.select(Some(0));
                        self.screen = Screen::Review;
                        finished = true;
                    }
                    JobMsg::Planned(Err(e)) | JobMsg::Info(Err(e)) => {
                        let cancelled = e == "cancelled";
                        self.message = Some((if cancelled { "Cancelled".into() } else { e }, !cancelled));
                        self.screen = Screen::Pick;
                        finished = true;
                    }
                    JobMsg::Info(Ok(s)) => {
                        self.message = Some((s, false));
                        self.screen = Screen::Pick;
                        finished = true;
                    }
                }
            }
            if finished {
                self.job_rx = None;
                self.refresh_catalog_cache();
            }
        }

        if let Some(rx) = &self.xfer_rx {
            let mut finished = false;
            while let Ok(m) = rx.try_recv() {
                match m {
                    XferMsg::Progress { files_done, bytes_done, current, phase } => {
                        self.files_done = files_done;
                        self.bytes_done = bytes_done;
                        self.current = current;
                        self.xphase = phase;
                        let now = Instant::now();
                        self.samples.push_back((now, bytes_done));
                        while self.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(3)) {
                            self.samples.pop_front();
                        }
                    }
                    XferMsg::Line(l) => {
                        self.lines.push(l);
                        if self.lines.len() > 500 {
                            self.lines.drain(..100);
                        }
                    }
                    XferMsg::Done(r) => {
                        match r {
                            Ok(s) => self.summary = Some(s),
                            Err(e) => self.message = Some((format!("Stopped: {e}"), true)),
                        }
                        self.xfer_elapsed = Some(self.xfer_started.elapsed());
                        self.screen = Screen::Done;
                        finished = true;
                    }
                }
            }
            if finished {
                self.xfer_rx = None;
                self.refresh_catalog_cache();
            }
        }

        if self.screen == Screen::Pick && self.last_refresh.elapsed() > Duration::from_secs(2) {
            self.refresh_volumes();
        }
    }

    fn refresh_volumes(&mut self) {
        self.volumes = volumes::list();
        self.refresh_volume_history();
        self.last_refresh = Instant::now();
        let n = self.entries().len();
        if self.pick.selected().is_none_or(|i| i >= n) {
            self.pick.select(Some(0));
        }
    }

    fn entries(&self) -> Vec<Entry> {
        let mut e: Vec<Entry> = (0..self.volumes.len()).map(Entry::Volume).collect();
        e.extend([Entry::Other, Entry::Tidy, Entry::Reorganize]);
        if self.undo_label.is_some() {
            e.push(Entry::Undo);
        }
        if self.app_cfg.second_copy_path().is_some() {
            e.push(Entry::CatchUpSecond);
        }
        e
    }

    /// Bytes/s over the last ~3 seconds.
    fn speed(&self) -> Option<f64> {
        let (t0, b0) = self.samples.front()?;
        let (t1, b1) = self.samples.back()?;
        let dt = t1.duration_since(*t0).as_secs_f64();
        (dt > 0.5).then(|| b1.saturating_sub(*b0) as f64 / dt)
    }

    // --- input -------------------------------------------------------------

    fn on_key(&mut self, k: KeyEvent) {
        let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
        if self.help {
            self.help = false;
            if ctrl_c {
                self.on_ctrl_c();
            }
            return;
        }
        if ctrl_c {
            self.on_ctrl_c();
            return;
        }
        if k.code == KeyCode::Char('?') && self.screen != Screen::Input {
            self.help = true;
            return;
        }
        match self.screen {
            Screen::Init => self.key_init(k),
            Screen::Pick => self.key_pick(k),
            Screen::Input => self.key_input(k),
            Screen::Settings => self.key_settings(k),
            Screen::Working => {
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                    self.job_cancel.store(true, Ordering::Relaxed);
                    self.phase = "Cancelling…".into();
                }
            }
            Screen::Review => self.key_review(k),
            Screen::Transfer => {
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                    self.on_ctrl_c();
                }
            }
            Screen::Done => self.key_done(k),
        }
    }

    /// Never kill mid-file or mid-plan; ask the worker to stop.
    fn on_ctrl_c(&mut self) {
        match self.screen {
            Screen::Transfer => {
                self.cancel.store(true, Ordering::Relaxed);
                self.message = Some(("Cancelling after the current file…".into(), false));
            }
            Screen::Working => self.job_cancel.store(true, Ordering::Relaxed),
            _ => self.quit = true,
        }
    }

    fn key_init(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('y') | KeyCode::Enter if check_library_dir(&self.pending_root).is_ok() => self.create_catalog(),
            KeyCode::Char('l') => self.begin_input(InputFor::Library),
            KeyCode::Char('n') | KeyCode::Esc if self.lib.is_some() => self.screen = Screen::Pick,
            KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Esc => self.quit = true,
            _ => {}
        }
    }

    fn begin_input(&mut self, purpose: InputFor) {
        self.input_for = purpose;
        self.input = match purpose {
            InputFor::BackupDir => self.app_cfg.backup_dir.clone(),
            InputFor::SecondCopy => self.app_cfg.second_copy.clone(),
            _ => String::new(),
        };
        self.message = None;
        self.screen = Screen::Input;
    }

    fn key_pick(&mut self, k: KeyEvent) {
        let entries = self.entries();
        let n = entries.len();
        let sel = self.pick.selected().unwrap_or(0).min(n - 1);
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.pick.select(Some((sel + n - 1) % n)),
            KeyCode::Down | KeyCode::Char('j') => self.pick.select(Some((sel + 1) % n)),
            KeyCode::Char('g') | KeyCode::Home => self.pick.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => self.pick.select(Some(n - 1)),
            KeyCode::Char('r') => {
                self.refresh_volumes();
                self.message = Some((format!("{} external drive(s) found", self.volumes.len()), false));
            }
            KeyCode::Char('s') => {
                self.settings_sel = 0;
                self.message = None;
                self.screen = Screen::Settings;
            }
            KeyCode::Char('L') => self.begin_input(InputFor::Library),
            KeyCode::Char('i') => self.start_index("Refreshing catalog"),
            KeyCode::Enter => match entries[sel] {
                Entry::Volume(i) => {
                    let v = &self.volumes[i];
                    // Cards: scan DCIM only, skipping the camera's own files.
                    let src = if v.has_dcim { v.path.join("DCIM") } else { v.path.clone() };
                    let vol = v.uuid.clone().map(|u| (v.name.clone(), u));
                    let eject = Some(v.path.clone());
                    self.start_import_scan(src, vol, eject);
                }
                Entry::Other => self.begin_input(InputFor::Source),
                Entry::Tidy => self.start_reconcile(Kind::Tidy),
                Entry::Reorganize => self.start_reconcile(Kind::Reorganize),
                Entry::Undo => self.start_undo(),
                Entry::CatchUpSecond => self.start_catch_up(),
            },
            _ => {}
        }
    }

    fn key_input(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => {
                self.screen = match self.input_for {
                    InputFor::Source => Screen::Pick,
                    InputFor::BackupDir | InputFor::SecondCopy => Screen::Settings,
                    InputFor::Library if self.lib.is_some() => Screen::Pick,
                    InputFor::Library => Screen::Init,
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                match self.input_for {
                    InputFor::Source => self.start_import_scan(clean_typed_path(&text), None, None),
                    InputFor::Library => {
                        let p = clean_typed_path(&text);
                        let p = std::fs::canonicalize(&p).unwrap_or(p);
                        self.open_library(p, true);
                    }
                    InputFor::BackupDir => {
                        self.app_cfg.backup_dir = if text.is_empty() { String::new() } else { tilde(&clean_typed_path(&text)) };
                        let _ = self.app_cfg.save();
                        self.screen = Screen::Settings;
                    }
                    InputFor::SecondCopy => {
                        self.app_cfg.second_copy = if text.is_empty() { String::new() } else { tilde(&clean_typed_path(&text)) };
                        let _ = self.app_cfg.save();
                        self.screen = Screen::Settings;
                    }
                }
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }
    }

    const SETTINGS: usize = 7;

    fn key_settings(&mut self, k: KeyEvent) {
        let n = Self::SETTINGS;
        let step: isize = match k.code {
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => 1,
            KeyCode::Left | KeyCode::Char('h') => -1,
            _ => 0,
        };
        match k.code {
            KeyCode::Esc | KeyCode::Char('s') => self.screen = Screen::Pick,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.settings_sel = (self.settings_sel + n - 1) % n,
            KeyCode::Down | KeyCode::Char('j') => self.settings_sel = (self.settings_sel + 1) % n,
            KeyCode::Enter if self.settings_sel == 0 => self.begin_input(InputFor::Library),
            KeyCode::Enter if self.settings_sel == 5 => self.begin_input(InputFor::BackupDir),
            KeyCode::Enter if self.settings_sel == 6 => self.begin_input(InputFor::SecondCopy),
            _ if step != 0 => match self.settings_sel {
                1 => {
                    if let Some(l) = self.lib.as_mut() {
                        l.cfg.layout = layout::next_preset(&l.cfg.layout, step).to_string();
                    }
                    self.save_lib_cfg();
                }
                2 => {
                    self.dup_mode = self.dup_mode.toggle();
                    if let Some(l) = self.lib.as_mut() {
                        l.cfg.duplicates = self.dup_mode.key().into();
                    }
                    self.save_lib_cfg();
                }
                3 => {
                    self.mode = if self.mode == Mode::Copy { Mode::Move } else { Mode::Copy };
                    self.app_cfg.mode = self.mode.key().into();
                    let _ = self.app_cfg.save();
                }
                4 => {
                    if let Some(l) = self.lib.as_mut() {
                        l.cfg.trash_duplicates = !l.cfg.trash_duplicates;
                    }
                    self.save_lib_cfg();
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn tab_len(&self) -> usize {
        let Some(p) = &self.plan else { return 0 };
        match self.tab {
            0 => p.new_items().count(),
            1 => p.duplicates().count(),
            2 => p.conflicts().count(),
            _ => p.undated.len(),
        }
    }

    fn key_review(&mut self, k: KeyEvent) {
        let len = self.tab_len();
        let sel = self.table.selected().unwrap_or(0);
        let kind = self.plan.as_ref().map(|p| p.kind).unwrap_or(Kind::Import);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                self.plan = None;
                self.refresh_volumes();
                self.screen = Screen::Pick;
            }
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.tab = (self.tab + 1) % 4;
                self.table.select(Some(0));
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.tab = (self.tab + 3) % 4;
                self.table.select(Some(0));
            }
            KeyCode::Down | KeyCode::Char('j') => self.table.select(Some((sel + 1).min(len.saturating_sub(1)))),
            KeyCode::Up | KeyCode::Char('k') => self.table.select(Some(sel.saturating_sub(1))),
            KeyCode::PageDown => self.table.select(Some((sel + 20).min(len.saturating_sub(1)))),
            KeyCode::PageUp => self.table.select(Some(sel.saturating_sub(20))),
            KeyCode::Char('g') | KeyCode::Home => self.table.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => self.table.select(Some(len.saturating_sub(1))),
            KeyCode::Char('m') if kind == Kind::Import => {
                self.mode = if self.mode == Mode::Copy { Mode::Move } else { Mode::Copy };
            }
            KeyCode::Char('d') if kind == Kind::Import => {
                // The rule changes what counts as a duplicate: re-plan.
                self.dup_mode = self.dup_mode.toggle();
                if let Some(src) = self.source.clone() {
                    let (vol, ej) = (self.source_volume.clone(), self.eject_path.clone());
                    self.start_import_scan(src, vol, ej);
                }
            }
            KeyCode::Char('t') if kind != Kind::Import => {
                if let Some(p) = self.plan.as_mut() {
                    p.trash_dups = !p.trash_dups;
                }
            }
            KeyCode::Enter => self.start_transfer(),
            _ => {}
        }
    }

    fn key_done(&mut self, k: KeyEvent) {
        let undoable = self.undo_label.is_some() && self.summary.as_ref().is_some_and(|s| s.kind != Kind::Import);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('e') if self.eject_path.is_some() => {
                let vol = self.eject_path.take().unwrap();
                self.message = Some(match volumes::eject(&vol) {
                    Ok(()) => (format!("Ejected {} — safe to remove the card", vol.display()), false),
                    Err(e) => (format!("Eject failed: {e}"), true),
                });
            }
            KeyCode::Char('u') if undoable => {
                self.plan = None;
                self.summary = None;
                self.start_undo();
            }
            KeyCode::Enter | KeyCode::Esc => {
                self.plan = None;
                self.summary = None;
                self.message = None;
                self.refresh_volumes();
                self.screen = Screen::Pick;
            }
            _ => {}
        }
    }

    // --- drawing -----------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let [head, body, msg, foot] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(5), Constraint::Length(1), Constraint::Length(1)]).areas(f.area());

        let (lib_txt, extra) = match &self.lib {
            Some(l) => (
                tilde(&l.root),
                format!(
                    "  ·  {} files  ·  layout: {}  ·  duplicates: {}  ·  {}",
                    l.present_count,
                    layout::preset_name(&l.cfg.layout),
                    self.dup_mode.label(),
                    self.mode.label()
                ),
            ),
            None => (tilde(&self.pending_root), String::new()),
        };
        let title = Line::from(vec![
            Span::styled(" mootui ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw("  library "),
            Span::styled(lib_txt, Style::new().fg(ACCENT).bold()),
            Span::styled(extra, Style::new().dark_gray()),
        ]);
        let src = match (&self.source, self.screen) {
            (Some(s), Screen::Working | Screen::Review | Screen::Transfer | Screen::Done) => {
                Line::from(vec![Span::raw("  from "), Span::styled(tilde(s), Style::new().bold())])
            }
            _ => Line::from(""),
        };
        f.render_widget(Paragraph::new(vec![title, src]), head);

        match self.screen {
            Screen::Init => self.draw_init(f, body),
            Screen::Pick => self.draw_pick(f, body),
            Screen::Input => self.draw_input(f, body),
            Screen::Settings => self.draw_settings(f, body),
            Screen::Working => self.draw_working(f, body),
            Screen::Review => self.draw_review(f, body),
            Screen::Transfer => self.draw_transfer(f, body),
            Screen::Done => self.draw_done(f, body),
        }

        if let Some((m, err)) = &self.message {
            let style = if *err { Style::new().fg(Color::Red).bold() } else { Style::new().fg(Color::Green) };
            f.render_widget(Paragraph::new(format!(" {m}")).style(style), msg);
        }

        let kind = self.plan.as_ref().map(|p| p.kind);
        let keys: Vec<(&str, &str)> = match self.screen {
            Screen::Init => vec![("y", "create catalog"), ("l", "choose another folder"), ("q", "quit")],
            Screen::Pick => vec![("↑↓", "select"), ("enter", "go"), ("s", "settings"), ("L", "library"), ("i", "refresh catalog"), ("q", "quit")],
            Screen::Input => vec![("type/drag a folder", ""), ("enter", "ok"), ("esc", "back")],
            Screen::Settings => vec![("↑↓", "select"), ("←→", "change"), ("enter", "edit path"), ("esc", "back")],
            Screen::Working => vec![("q", "cancel")],
            Screen::Review if kind == Some(Kind::Import) => {
                vec![("tab", "list"), ("↑↓", "scroll"), ("m", "copy/move"), ("d", "duplicate rule"), ("enter", "start"), ("esc", "back")]
            }
            Screen::Review => vec![("tab", "list"), ("↑↓", "scroll"), ("t", "trash dups on/off"), ("enter", "apply"), ("esc", "back")],
            Screen::Transfer => vec![("q", "cancel after current file")],
            Screen::Done => {
                let mut v = vec![];
                if self.eject_path.is_some() {
                    v.push(("e", "eject card"));
                }
                if self.undo_label.is_some() && self.summary.as_ref().is_some_and(|s| s.kind != Kind::Import) {
                    v.push(("u", "undo"));
                }
                v.extend([("enter", "back"), ("q", "quit")]);
                v
            }
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, d) in keys.iter().chain([("?", "help")].iter()) {
            spans.push(Span::styled(*k, Style::new().fg(ACCENT).bold()));
            if !d.is_empty() {
                spans.push(Span::raw(format!(" {d}")));
            }
            spans.push(Span::styled("  ·  ", Style::new().dark_gray()));
        }
        spans.pop();
        f.render_widget(Paragraph::new(Line::from(spans)), foot);

        if self.help {
            draw_help(f);
        }
    }

    fn block(title: &str) -> Block<'_> {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().dark_gray())
            .title(Span::styled(format!(" {title} "), Style::new().bold()))
    }

    fn draw_init(&self, f: &mut Frame, area: Rect) {
        let root = &self.pending_root;
        let mut lines = vec![
            Line::from(""),
            Line::from(vec![Span::raw("  Library folder: "), Span::styled(tilde(root), Style::new().fg(ACCENT).bold())]),
            Line::from(""),
        ];
        if check_library_dir(root).is_ok() {
            lines.extend([
                Line::from("  mootui keeps a catalog of this library so duplicate checks are instant."),
                Line::from(format!("  It will create {}/ (hidden) containing:", tilde(&root.join(catalog::DIR)))),
                Line::from("    catalog.db     index of your photos + import history (SQLite)"),
                Line::from("    library.toml   layout and duplicate settings for this library"),
                Line::from("    backups/       the last few catalog snapshots"),
                Line::from(""),
                Line::from("  Nothing in the library is moved or changed. The first build reads each"),
                Line::from("  photo's capture date once; later runs only look at new files."),
                Line::from(""),
                Line::from(Span::styled("  Create the catalog here?  y = yes   l = use a different folder", Style::new().bold())),
            ]);
        } else {
            lines.push(Line::from(Span::styled("  This folder can't be used. Press l to choose another.", Style::new().fg(Color::Red))));
        }
        f.render_widget(Paragraph::new(lines).block(Self::block("Set up library")).wrap(Wrap { trim: false }), area);
    }

    fn draw_pick(&mut self, f: &mut Frame, area: Rect) {
        let entries = self.entries();
        let items: Vec<ListItem> = entries
            .iter()
            .map(|e| match *e {
                Entry::Volume(i) => {
                    let v = &self.volumes[i];
                    let mut spans = vec![Span::raw("  ▸ "), Span::styled(v.name.clone(), Style::new().bold())];
                    if v.has_dcim {
                        spans.push(Span::styled("  camera card", Style::new().fg(Color::Green)));
                    }
                    let last = self.volume_last_imports.get(i).and_then(|x| x.as_ref());
                    if let Some((when, n)) = last {
                        spans.push(Span::styled(
                            format!("  · last imported {} ({n} files)", &when[..when.len().min(16)]),
                            Style::new().fg(Color::Yellow),
                        ));
                    }
                    spans.push(Span::styled(format!("  {}", v.path.display()), Style::new().dark_gray()));
                    ListItem::new(Line::from(spans))
                }
                Entry::Other => ListItem::new(Line::from(vec![Span::raw("  ▸ "), Span::styled("Other folder…", Style::new().italic())])),
                Entry::Tidy => ListItem::new(Line::from(vec![
                    Span::raw("  ◆ "),
                    Span::styled("Tidy library", Style::new().bold()),
                    Span::styled("  sort loose files already in the library into the layout", Style::new().dark_gray()),
                ])),
                Entry::Reorganize => ListItem::new(Line::from(vec![
                    Span::raw("  ◆ "),
                    Span::styled("Reorganize whole library…", Style::new().bold()),
                    Span::styled("  re-sort everything to the current layout", Style::new().dark_gray()),
                ])),
                Entry::Undo => ListItem::new(Line::from(vec![
                    Span::raw("  ↶ "),
                    Span::styled(self.undo_label.clone().unwrap_or_default(), Style::new().fg(Color::Yellow)),
                ])),
                Entry::CatchUpSecond => ListItem::new(Line::from(vec![
                    Span::raw("  ◆ "),
                    Span::styled("Catch up second copy", Style::new().bold()),
                    Span::styled(
                        format!("  copy library files missing from {}", self.app_cfg.second_copy_path().map(|p| tilde(&p)).unwrap_or_default()),
                        Style::new().dark_gray(),
                    ),
                ])),
            })
            .collect();

        let title = if self.volumes.is_empty() {
            "Import from — no external drives yet (insert a card; this list refreshes itself)"
        } else {
            "Import from a drive or folder — or work on the library"
        };
        let list = List::new(items)
            .block(Self::block(title))
            .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("›");
        f.render_stateful_widget(list, area, &mut self.pick);
    }

    fn draw_input(&self, f: &mut Frame, area: Rect) {
        let [a, _] = Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).areas(area);
        let (title, hint) = match self.input_for {
            InputFor::Source => ("Import from folder", "Type a path, or drag a folder from Finder into this window:"),
            InputFor::Library => ("Library folder", "Folder to import into; it becomes the default. Type or drag a folder:"),
            InputFor::BackupDir => ("Extra catalog backup folder", "e.g. a Google Drive folder. Leave empty for none:"),
            InputFor::SecondCopy => ("Second copy folder", "Imports also write a verified copy of every photo here, e.g. a backup drive. Leave empty for none:"),
        };
        let p = Paragraph::new(vec![
            Line::from(Span::styled(hint, Style::new().dark_gray())),
            Line::from(vec![Span::raw(self.input.clone()), Span::styled("█", Style::new().fg(ACCENT))]),
        ])
        .block(Self::block(title))
        .wrap(Wrap { trim: false });
        f.render_widget(p, a);
    }

    fn draw_settings(&self, f: &mut Frame, area: Rect) {
        let Some(l) = &self.lib else { return };
        let preset = layout::preset_name(&l.cfg.layout);
        let rows: [(&str, String, &str); Self::SETTINGS] = [
            ("Library folder", tilde(&l.root), "enter to change · becomes the default"),
            ("Layout", format!("{preset}   {}", l.cfg.layout), "←→ presets · custom: edit library.toml"),
            ("Duplicate check", self.dup_mode.label().into(), "Suspected = name+size+capture time · Content-verified = hash"),
            ("Default transfer", self.mode.label().into(), "COPY keeps the card intact"),
            (
                "Tidy: duplicates",
                if l.cfg.trash_duplicates { "move loose duplicates to Trash".into() } else { "report only".into() },
                "←→ toggle",
            ),
            (
                "Extra backup folder",
                if self.app_cfg.backup_dir.is_empty() { "none".into() } else { self.app_cfg.backup_dir.clone() },
                "enter to change · catalog backups only",
            ),
            (
                "Second copy",
                if self.app_cfg.second_copy.is_empty() { "none".into() } else { self.app_cfg.second_copy.clone() },
                "enter to change · imports copy every photo here too",
            ),
        ];
        let mut lines = vec![Line::from("")];
        for (i, (name, value, hint)) in rows.iter().enumerate() {
            let sel = i == self.settings_sel;
            let style = if sel { Style::new().bg(Color::DarkGray).bold() } else { Style::new() };
            lines.push(Line::from(vec![
                Span::styled(format!(" {} {name:<20}", if sel { "›" } else { " " }), style),
                Span::styled(format!(" {value} "), style.fg(ACCENT)),
                Span::styled(format!("  {hint}"), Style::new().dark_gray()),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(format!("  Excluded folders: {}", l.cfg.exclude.join(", ")), Style::new().dark_gray())));
        lines.push(Line::from(Span::styled("  (macOS library packages like *.photoslibrary are always skipped)", Style::new().dark_gray())));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(format!("  This library: {}", tilde(&LibraryConfig::path(&l.root))), Style::new().dark_gray())));
        lines.push(Line::from(Span::styled(format!("  This Mac:     {}", tilde(&AppConfig::path())), Style::new().dark_gray())));
        f.render_widget(Paragraph::new(lines).block(Self::block("Settings")), area);
    }

    fn draw_working(&self, f: &mut Frame, area: Rect) {
        let [a, _] = Layout::vertical([Constraint::Length(5), Constraint::Min(0)]).areas(area);
        let ratio = if self.phase_total > 0 { (self.phase_done as f64 / self.phase_total as f64).min(1.0) } else { 0.0 };
        let mut label = if self.phase_total > 0 {
            format!("{}  {}/{}", self.phase, self.phase_done, self.phase_total)
        } else if self.phase_done > 0 {
            format!("{}  {} found", self.phase, self.phase_done)
        } else {
            format!("{}…", self.phase)
        };
        if self.phase_total > 0 && self.phase_done > 5 {
            let el = self.phase_started.elapsed().as_secs_f64();
            let eta = el / self.phase_done as f64 * self.phase_total.saturating_sub(self.phase_done) as f64;
            label.push_str(&format!("  · ETA {}", human_duration(eta)));
        }
        let g = Gauge::default()
            .block(Self::block(&self.job_title))
            .gauge_style(Style::new().fg(ACCENT).bg(Color::Black))
            .ratio(ratio)
            .label(label);
        f.render_widget(g, a);
    }

    fn draw_review(&mut self, f: &mut Frame, area: Rect) {
        let Some(plan) = &self.plan else { return };
        let import = plan.kind == Kind::Import;
        let [stats, tabs, table] = Layout::vertical([Constraint::Length(5), Constraint::Length(1), Constraint::Min(3)]).areas(area);

        let n_new = plan.new_items().count();
        let n_dup = plan.duplicates().count();
        let n_con = plan.conflicts().count();
        let mut stat_spans = vec![
            Span::styled(format!(" {n_new} {} ", if import { "new" } else { "to move" }), Style::new().fg(Color::Green).bold()),
            Span::raw(format!("({})   ", human_bytes(plan.new_bytes()))),
            Span::styled(format!("{n_dup} duplicates"), Style::new().fg(Color::Yellow).bold()),
            Span::raw("   "),
            Span::styled(format!("{n_con} conflicts"), Style::new().fg(Color::Red).bold()),
            Span::raw(format!("   {} undated", plan.undated.len())),
        ];
        if !plan.scan_errors.is_empty() {
            stat_spans.push(Span::styled(format!("   {} scan error(s)", plan.scan_errors.len()), Style::new().fg(Color::Red).bold()));
        }
        stat_spans.push(Span::styled(
            if import { format!("   {} non-media ignored", plan.ignored) } else { format!("   {} already in place", plan.in_place) },
            Style::new().dark_gray(),
        ));
        let mut lines = vec![Line::from(stat_spans)];
        if import {
            let ms = if self.mode == Mode::Move {
                Style::new().fg(Color::Black).bg(Color::Yellow).bold()
            } else {
                Style::new().fg(Color::Black).bg(Color::Green).bold()
            };
            let note = if self.mode == Mode::Move {
                "  verified copy, then delete from source (duplicates stay)"
            } else {
                "  verified copy, source untouched"
            };
            let second = self.app_cfg.second_copy_path().map(|p| format!(" · second copy → {}", tilde(&p))).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::raw(" mode "),
                Span::styled(format!(" {} ", self.mode.label()), ms),
                Span::styled(format!("{note}{second}"), Style::new().dark_gray()),
            ]));
            let dn = match plan.dup_mode {
                DupMode::Suspected => "  same original name + size + capture time as a catalogued file",
                DupMode::Verified => "  identical content (hash) to a catalogued file, even if renamed",
            };
            lines.push(Line::from(vec![
                Span::raw(" duplicates "),
                Span::styled(format!(" {} ", plan.dup_mode.label()), Style::new().fg(Color::Black).bg(ACCENT).bold()),
                Span::styled(dn, Style::new().dark_gray()),
            ]));
        } else {
            let t = plan.trashable();
            lines.push(Line::from(vec![
                Span::raw(" duplicates "),
                Span::styled(
                    if plan.trash_dups { format!(" {t} loose → Trash ") } else { " report only ".into() },
                    Style::new().fg(Color::Black).bg(if plan.trash_dups { Color::Yellow } else { Color::Green }).bold(),
                ),
                Span::styled("  sorted copies are never touched; undo restores from Trash", Style::new().dark_gray()),
            ]));
            lines.push(Line::from(Span::styled(
                if plan.kind == Kind::Reorganize {
                    " Moves already-sorted files too. Lightroom may lose track of moved files; backup services may re-upload them."
                } else {
                    " Only loose files and detached sidecars move; files already in sorted folders stay put."
                },
                if plan.kind == Kind::Reorganize { Style::new().fg(Color::Red) } else { Style::new().dark_gray() },
            )));
        }
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().dark_gray()).title(" Review ")),
            stats,
        );

        let counts = [n_new, n_dup, n_con, plan.undated.len()];
        let names = if import { ["New", "Duplicates", "Conflicts", "Undated"] } else { ["To move", "Duplicates", "Conflicts", "Undated"] };
        let titles: Vec<String> = names.iter().zip(counts).map(|(t, c)| format!("{t} ({c})")).collect();
        f.render_widget(Tabs::new(titles).select(self.tab).highlight_style(Style::new().fg(ACCENT).bold().underlined()).divider(" │ "), tabs);

        let root = &plan.root;
        let relp = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();
        let srcp = |p: &Path| p.strip_prefix(&plan.source).unwrap_or(p).display().to_string();
        let (header, widths, rows): (Vec<&str>, Vec<Constraint>, Vec<Row>) = match self.tab {
            0 => (
                vec!["file", "→ destination", "size", "date from"],
                vec![Constraint::Percentage(32), Constraint::Percentage(36), Constraint::Length(10), Constraint::Fill(1)],
                plan.new_items().map(|i| Row::new(vec![srcp(&i.src), i.rel_dest.clone(), human_bytes(i.size), i.date_src.clone()])).collect(),
            ),
            1 => (
                vec!["file", "same as", "rule", "size"],
                vec![Constraint::Percentage(32), Constraint::Fill(1), Constraint::Length(10), Constraint::Length(10)],
                plan.duplicates()
                    .map(|i| {
                        let (ex, rule) = match &i.status {
                            Status::Duplicate { existing, rule, trashable, .. } => {
                                let ex = if import && existing.starts_with(&plan.source) {
                                    format!("this import: {}", srcp(existing))
                                } else {
                                    relp(existing)
                                };
                                let rule = if *trashable && plan.trash_dups { "→ Trash" } else { rule };
                                (ex, rule.to_string())
                            }
                            _ => Default::default(),
                        };
                        Row::new(vec![srcp(&i.src), ex, rule, human_bytes(i.size)])
                    })
                    .collect(),
            ),
            2 => (
                vec!["file", "→ wanted", "why skipped"],
                vec![Constraint::Percentage(30), Constraint::Percentage(30), Constraint::Fill(1)],
                plan.conflicts()
                    .map(|i| {
                        let why = if let Status::Conflict { reason } = &i.status { reason.clone() } else { String::new() };
                        Row::new(vec![srcp(&i.src), i.rel_dest.clone(), why])
                    })
                    .collect(),
            ),
            _ => (
                vec!["file (no date found, left in place)"],
                vec![Constraint::Fill(1)],
                plan.undated.iter().map(|p| Row::new(vec![srcp(p)])).collect(),
            ),
        };
        let empty = rows.is_empty();
        let t = Table::new(rows, widths)
            .header(Row::new(header.into_iter().map(|h| Cell::from(h).style(Style::new().dark_gray().bold()))))
            .row_highlight_style(Style::new().bg(Color::DarkGray))
            .block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().dark_gray()));
        f.render_stateful_widget(t, table, &mut self.table);
        if empty {
            let inner = Rect { x: table.x + 2, y: table.y + 2, width: table.width.saturating_sub(4), height: 1 };
            f.render_widget(Paragraph::new("nothing here").dark_gray(), inner);
        }
    }

    fn draw_transfer(&self, f: &mut Frame, area: Rect) {
        let Some(plan) = &self.plan else { return };
        let [g, log] = Layout::vertical([Constraint::Length(5), Constraint::Min(3)]).areas(area);
        let total_files = plan.new_items().count();
        let total_bytes = plan.new_bytes();
        let ratio = if total_bytes > 0 { (self.bytes_done as f64 / total_bytes as f64).min(1.0) } else { 0.0 };

        // 42/318 files · 1.4 GB / 10.6 GB @ 87.40 MB/s · ETA 1m 48s · DSCF7319.RAF
        let mut label = format!("{}/{} files · {} / {}", self.files_done, total_files, human_bytes(self.bytes_done), human_bytes(total_bytes));
        if plan.kind != Kind::Import || self.xphase == "moving" {
            let el = self.xfer_started.elapsed().as_secs_f64();
            if el > 0.5 {
                label.push_str(&format!(" @ {:.0} files/s", self.files_done as f64 / el));
            }
        } else if let Some(bps) = self.speed() {
            label.push_str(&format!(" @ {:.2} MB/s", bps / 1_000_000.0));
            if bps > 0.0 {
                label.push_str(&format!(" · ETA {}", human_duration(total_bytes.saturating_sub(self.bytes_done) as f64 / bps)));
            }
        }
        if !self.current.is_empty() {
            label.push_str(&format!(" · {}", self.current));
            if self.xphase == "verifying" {
                label.push_str(" (verifying)");
            }
        }
        let title = match (plan.kind, self.mode) {
            (Kind::Import, Mode::Copy) => "Copying",
            (Kind::Import, Mode::Move) => "Moving",
            _ => "Moving within library",
        };
        let gauge = Gauge::default().block(Self::block(title)).gauge_style(Style::new().fg(ACCENT).bg(Color::Black)).ratio(ratio).label(label);
        f.render_widget(gauge, g);

        let h = log.height.saturating_sub(2) as usize;
        let start = self.lines.len().saturating_sub(h);
        let items: Vec<ListItem> = self.lines[start..].iter().map(|l| ListItem::new(styled_log_line(l))).collect();
        f.render_widget(List::new(items).block(Self::block("Log")), log);
    }

    fn draw_done(&self, f: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        if let Some(s) = &self.summary {
            let headline = if s.cancelled {
                Span::styled(" Cancelled ", Style::new().fg(Color::Black).bg(Color::Yellow).bold())
            } else if s.failed > 0 {
                Span::styled(" Finished with errors ", Style::new().fg(Color::Black).bg(Color::Red).bold())
            } else {
                Span::styled(" Done ", Style::new().fg(Color::Black).bg(Color::Green).bold())
            };
            lines.push(Line::from(headline));
            lines.push(Line::from(""));
            let el = self.xfer_elapsed.unwrap_or_default().as_secs_f64();
            let rate = if s.kind == Kind::Import && el > 1.0 {
                format!(" in {} · avg {:.1} MB/s", human_duration(el), s.bytes as f64 / el / 1e6)
            } else {
                String::new()
            };
            let verb = match (s.kind, s.mode) {
                (Kind::Import, Mode::Copy) => "copied",
                (Kind::Import, Mode::Move) => "moved",
                _ => "moved within the library",
            };
            lines.push(Line::from(format!("  {} file(s) {verb} ({}){rate}", s.transferred, human_bytes(s.bytes))));
            if s.failed > 0 {
                lines.push(Line::from(Span::styled(format!("  {} failed — see log", s.failed), Style::new().fg(Color::Red))));
            }
            if s.warnings > 0 {
                lines.push(Line::from(Span::styled(format!("  {} warning(s) — see log", s.warnings), Style::new().fg(Color::Yellow))));
            }
            if s.scan_errors > 0 {
                lines.push(Line::from(Span::styled(format!("  {} scan error(s) — see log", s.scan_errors), Style::new().fg(Color::Red))));
            }
            if s.second_caught_up > 0 {
                lines.push(Line::from(format!("  {} file(s) already in the library added to the second copy", s.second_caught_up)));
            }
            if let Some(sec) = &s.second {
                let line = if s.second_failed > 0 {
                    Span::styled(format!("  second copy: {} failed — see log ({})", s.second_failed, tilde(sec)), Style::new().fg(Color::Yellow))
                } else {
                    Span::raw(format!("  second copy in {}", tilde(sec)))
                };
                lines.push(Line::from(line));
            }
            let dup_note = if s.kind == Kind::Import { ", left on source" } else { "" };
            let trash = if s.trashed > 0 { format!(", {} moved to Trash", s.trashed) } else { String::new() };
            lines.push(Line::from(format!("  {} duplicate(s){dup_note}{trash}", s.duplicates)));
            lines.push(Line::from(format!("  {} conflict(s) skipped, {} undated left in place", s.conflicts, s.undated)));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::raw("  log         "), Span::styled(tilde(&s.log_path), Style::new().fg(ACCENT))]));
            if let Some(c) = &s.csv_path {
                lines.push(Line::from(vec![Span::raw("  duplicates  "), Span::styled(tilde(c), Style::new().fg(ACCENT))]));
            }
            if let Some(n) = &s.note {
                lines.push(Line::from(Span::styled(format!("  note: {n}"), Style::new().fg(Color::Yellow))));
            }
            if s.kind != Kind::Import && s.transferred + s.trashed > 0 {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("  Changed your mind? Press u to undo this run.", Style::new().dark_gray())));
            }
        }
        f.render_widget(Paragraph::new(lines).block(Self::block("Summary")).wrap(Wrap { trim: false }), area);
    }
}

fn draw_help(f: &mut Frame) {
    const KEYS: &[(&str, &str)] = &[
        ("j / k  ↑ / ↓", "move down / up"),
        ("g / G", "jump to top / bottom"),
        ("h / l  tab", "previous / next list (review)"),
        ("enter", "go / start"),
        ("s", "settings (layout, duplicate rule, copy/move)"),
        ("L", "change library folder (becomes default)"),
        ("i", "refresh the catalog now"),
        ("m", "copy ↔ move for this import (review)"),
        ("d", "suspected ↔ content-verified duplicates (review)"),
        ("t", "Tidy: send loose duplicates to Trash on/off"),
        ("e / u", "eject card / undo tidy (when done)"),
        ("esc", "go back"),
        ("q / ctrl-c", "quit — or cancel the running job safely"),
    ];
    let area = f.area();
    let w = 70.min(area.width);
    let h = (KEYS.len() as u16 + 4).min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    let lines: Vec<Line> =
        KEYS.iter().map(|(k, d)| Line::from(vec![Span::styled(format!("  {k:<16}"), Style::new().fg(ACCENT).bold()), Span::raw(*d)])).collect();
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(ACCENT))
                .title(" Keys ")
                .title_bottom(Line::from(" any key to close ").centered()),
        ),
        r,
    );
}

fn styled_log_line(l: &str) -> Line<'static> {
    let color = if l.starts_with("FAILED") {
        Color::Red
    } else if l.starts_with("WARNING") || l.starts_with("TRASHED") {
        Color::Yellow
    } else {
        Color::Green
    };
    match l.split_once(' ') {
        Some((tag, rest)) => Line::from(vec![Span::styled(tag.to_string(), Style::new().fg(color).bold()), Span::raw(format!(" {rest}"))]),
        None => Line::from(l.to_string()),
    }
}
