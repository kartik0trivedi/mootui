//! Capture dates and camera names.
//!
//! One exiftool process stays open for the whole run (-stay_open), fed batches
//! (note: -q would suppress the "{ready}" marker that ends each batch)
//! over a pipe, instead of a new process per batch. -fast2 stops it reading
//! past the metadata, which matters on a slow card.
//!
//! Without exiftool, or when it finds no date: sips (images), then the
//! filesystem birth time, which on an imported card is the import date.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::media;

// Shot-time tags in preference order. Filesystem dates are deliberately absent.
const DATE_TAGS: &[&str] = &["DateTimeOriginal", "CreateDate", "MediaCreateDate", "TrackCreateDate", "GPSDateStamp"];

#[derive(Clone, Default)]
pub struct Meta {
    /// "YYYY-MM-DD HH:MM:SS", camera-local time where the format allows.
    pub capture: Option<String>,
    pub src: String,
    pub camera: Option<String>,
}

pub struct ExifTool {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
}

impl ExifTool {
    pub fn start() -> Option<Self> {
        let mut child = Command::new("exiftool")
            .args(["-stay_open", "True", "-@", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let out = BufReader::new(child.stdout.take()?);
        Some(ExifTool { child, stdin, out })
    }

    /// Metadata for each path exiftool could read. Paths with newlines are
    /// skipped (the argument stream is newline-delimited).
    pub fn read(&mut self, paths: &[&Path]) -> HashMap<PathBuf, Meta> {
        let mut out = HashMap::new();
        let usable: Vec<&Path> = paths.iter().copied().filter(|p| !p.to_string_lossy().contains(['\n', '\r'])).collect();
        if usable.is_empty() {
            return out;
        }
        let mut args = String::from("-json\n-m\n-fast2\n-charset\nfilename=utf8\n-d\n%Y-%m-%d %H:%M:%S\n-Model\n");
        for t in DATE_TAGS {
            args.push_str(&format!("-{t}\n"));
        }
        for p in &usable {
            args.push_str(&format!("{}\n", p.display()));
        }
        args.push_str("-execute\n");
        if self.stdin.write_all(args.as_bytes()).and_then(|_| self.stdin.flush()).is_err() {
            return out;
        }

        let mut body = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            match self.out.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line.trim_end() == "{ready}" => break,
                Ok(_) => body.push_str(&line),
            }
        }

        // Only the JSON array; ignore any summary lines around it.
        let json = match (body.find('['), body.rfind(']')) {
            (Some(a), Some(b)) if b > a => &body[a..=b],
            _ => return out,
        };
        let Ok(serde_json::Value::Array(rows)) = serde_json::from_str(json) else { return out };
        for row in rows {
            let Some(sf) = row.get("SourceFile").and_then(|v| v.as_str()) else { continue };
            let camera = row.get("Model").map(|v| v.to_string().trim_matches('"').trim().to_string()).filter(|s| !s.is_empty());
            let mut meta = Meta { capture: None, src: String::new(), camera };
            for t in DATE_TAGS {
                if let Some(c) = row.get(*t).and_then(|v| v.as_str()).and_then(norm_capture) {
                    meta.capture = Some(c);
                    meta.src = format!("exif:{t}");
                    break;
                }
            }
            out.insert(PathBuf::from(sf), meta);
        }
        out
    }
}

impl Drop for ExifTool {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"-stay_open\nFalse\n");
        let _ = self.stdin.flush();
        let _ = self.child.wait();
    }
}

/// Accepts "YYYY-MM-DD HH:MM:SS..." (formatted by -d), "YYYY-MM-DD", or raw
/// "YYYY:MM:DD ..." (tags -d did not touch). Rejects zero dates.
pub fn norm_capture(v: &str) -> Option<String> {
    let v = v.trim();
    if v.len() < 10 {
        return None;
    }
    let date = v[..10].replace(':', "-");
    let time = if v.len() >= 19 { v[11..19].to_string() } else { "00:00:00".into() };
    let b = date.as_bytes();
    let ok = b[4] == b'-' && b[7] == b'-' && date[..4].bytes().all(|c| c.is_ascii_digit());
    let y: u32 = date[..4].parse().ok()?;
    let m: u32 = date[5..7].parse().ok()?;
    let d: u32 = date[8..10].parse().ok()?;
    if !ok || !(1900..=2200).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(format!("{date} {}", time.replace(':', ":")))
}

/// Per-file fallback when exiftool is absent or found nothing.
pub fn fallback(p: &Path) -> Meta {
    if media::is_image(&media::ext_lower(&p.to_string_lossy())) {
        if let Ok(o) = Command::new("sips").args(["-g", "creation"]).arg(p).output() {
            let s = String::from_utf8_lossy(&o.stdout);
            if let Some(c) = s.lines().find_map(|l| l.trim().strip_prefix("creation:")).and_then(norm_capture) {
                return Meta { capture: Some(c), src: "sips".into(), camera: None };
            }
        }
    }
    let capture = fs::metadata(p)
        .and_then(|m| m.created())
        .ok()
        .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M:%S").to_string());
    Meta { capture, src: "birthtime".into(), camera: None }
}

/// Read many files: exiftool in batches (reporting progress), then the
/// fallback for anything left without a date.
pub fn read_all(exif: &mut Option<ExifTool>, paths: &[PathBuf], mut tick: impl FnMut(usize), cancel: &dyn Fn() -> bool) -> HashMap<PathBuf, Meta> {
    let mut out: HashMap<PathBuf, Meta> = HashMap::new();
    let mut done = 0;
    if let Some(et) = exif.as_mut() {
        for chunk in paths.chunks(100) {
            if cancel() {
                return out;
            }
            let refs: Vec<&Path> = chunk.iter().map(|p| p.as_path()).collect();
            out.extend(et.read(&refs));
            done += chunk.len();
            tick(done);
        }
    }
    for p in paths {
        if cancel() {
            break;
        }
        let have = out.get(p).is_some_and(|m| m.capture.is_some());
        if !have {
            let camera = out.get(p).and_then(|m| m.camera.clone());
            let mut m = fallback(p);
            m.camera = m.camera.or(camera);
            out.insert(p.clone(), m);
        }
    }
    out
}
