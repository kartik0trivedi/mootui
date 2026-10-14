//! Messages from background work to the UI, and a small handle that every
//! worker uses to report progress and notice cancellation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use crate::plan::Plan;

pub enum JobMsg {
    Phase(String, usize),
    Tick(usize),
    Planned(Result<Plan, String>),
    /// A job that ends with a one-line result (index build, undo).
    Info(Result<String, String>),
}

#[derive(Clone)]
pub struct Progress {
    tx: Option<Sender<JobMsg>>,
    cancel: Arc<AtomicBool>,
}

impl Progress {
    pub fn new(tx: Sender<JobMsg>, cancel: Arc<AtomicBool>) -> Self {
        Progress { tx: Some(tx), cancel }
    }
    /// For work nobody is watching (e.g. the re-index after an undo).
    pub fn silent() -> Self {
        Progress { tx: None, cancel: Arc::new(AtomicBool::new(false)) }
    }
    pub fn phase(&self, s: impl Into<String>, total: usize) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(JobMsg::Phase(s.into(), total));
        }
    }
    pub fn tick(&self, n: usize) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(JobMsg::Tick(n));
        }
    }
    pub fn send(&self, m: JobMsg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(m);
        }
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}
