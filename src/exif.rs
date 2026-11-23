//! Capture dates and camera names.
//!
//! The native reader (exif_native) answers first for JPEG, TIFF-based raws
//! and RAF. Only files it can't date go to exiftool, which is started the
//! first time it is needed (a Perl start-up costs ~0.2 s).
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

/// exiftool, started on first use.
#[derive(Default)]
pub struct Reader {
    tool: Option<ExifTool>,
    tried: bool,
}

impl Reader {
    pub fn new() -> Self {
        Self::default()
    }

    fn tool(&mut self) -> Option<&mut ExifTool> {
        if !self.tried {
            self.tried = true;
            self.tool = ExifTool::start();
        }
        self.tool.as_mut()
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
    Some(format!("{date} {time}"))
}

/// Per-file fallback when exiftool is absent or found nothing.
pub fn fallback(p: &Path) -> Meta {
    fallback_all(&[p]).remove(p).unwrap_or_else(|| birthtime(p))
}

/// The fallback for many files: sips for the images, in batches (one
/// process per 100 files, not per file), then the filesystem birth time.
fn fallback_all(paths: &[&Path]) -> HashMap<PathBuf, Meta> {
    let images: Vec<&Path> = paths.iter().copied().filter(|p| media::is_image(&media::ext_lower(&p.to_string_lossy()))).collect();
    let mut dated = HashMap::new();
    for chunk in images.chunks(100) {
        dated.extend(sips_dates(chunk));
    }
    paths
        .iter()
        .map(|p| {
            let m = match dated.remove(*p) {
                Some(c) => Meta { capture: Some(c), src: "sips".into(), camera: None },
                None => birthtime(p),
            };
            (p.to_path_buf(), m)
        })
        .collect()
}

/// `sips -g creation a b c` prints each readable path as given, then an
/// indented "creation: ..." line; unreadable files only warn on stderr.
fn sips_dates(paths: &[&Path]) -> HashMap<PathBuf, String> {
    let mut out = HashMap::new();
    let Ok(o) = Command::new("sips").args(["-g", "creation"]).args(paths).stderr(Stdio::null()).output() else {
        return out;
    };
    let by_name: HashMap<String, &Path> = paths.iter().map(|p| (p.to_string_lossy().into_owned(), *p)).collect();
    let mut cur: Option<&Path> = None;
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        if let Some(v) = line.trim_start().strip_prefix("creation:") {
            if let (Some(p), Some(c)) = (cur, norm_capture(v)) {
                out.insert(p.to_path_buf(), c);
            }
        } else if !line.starts_with(' ') {
            cur = by_name.get(line).copied();
        }
    }
    out
}

fn birthtime(p: &Path) -> Meta {
    let capture = fs::metadata(p)
        .and_then(|m| m.created())
        .ok()
        .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M:%S").to_string());
    Meta { capture, src: "birthtime".into(), camera: None }
}

/// Read many files: the native reader, then exiftool in batches for what it
/// couldn't date, then the fallback for anything still without a date.
/// Progress counts files settled, so it only moves forward.
pub fn read_all(r: &mut Reader, paths: &[PathBuf], mut tick: impl FnMut(usize), cancel: &dyn Fn() -> bool) -> HashMap<PathBuf, Meta> {
    let mut out: HashMap<PathBuf, Meta> = HashMap::new();
    let mut rest: Vec<&Path> = Vec::new();
    for (k, p) in paths.iter().enumerate() {
        if cancel() {
            return out;
        }
        match crate::exif_native::read(p) {
            Some(m) => {
                out.insert(p.clone(), m);
            }
            None => rest.push(p),
        }
        if k % 50 == 49 {
            tick(out.len());
        }
    }
    let mut done = out.len();
    tick(done);
    if rest.is_empty() {
        return out;
    }
    if let Some(et) = r.tool() {
        for chunk in rest.chunks(100) {
            if cancel() {
                return out;
            }
            out.extend(et.read(chunk));
            done += chunk.len();
            tick(done);
        }
    }
    if cancel() {
        return out;
    }
    let undated: Vec<&Path> = rest.into_iter().filter(|p| out.get(*p).is_none_or(|m| m.capture.is_none())).collect();
    for (p, mut m) in fallback_all(&undated) {
        m.camera = m.camera.or_else(|| out.get(&p).and_then(|e| e.camera.clone()));
        out.insert(p, m);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compare the native reader with exiftool on real files:
    ///   MOOTUI_EXIF_CORPUS=~/Pictures cargo test --release corpus -- --ignored --nocapture
    #[test]
    #[ignore]
    fn corpus_matches_exiftool() {
        let dir = std::env::var("MOOTUI_EXIF_CORPUS").expect("set MOOTUI_EXIF_CORPUS");
        let paths: Vec<PathBuf> = walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file() && !e.file_name().to_string_lossy().starts_with('.'))
            .filter(|e| media::is_image(&media::ext_lower(&e.file_name().to_string_lossy())))
            .map(|e| e.into_path())
            .collect();

        let t = std::time::Instant::now();
        let native: Vec<Option<Meta>> = paths.iter().map(|p| crate::exif_native::read(p)).collect();
        let t_native = t.elapsed();

        let t = std::time::Instant::now();
        let mut et = ExifTool::start().expect("exiftool not installed");
        let mut reference = HashMap::new();
        for chunk in paths.chunks(100) {
            let refs: Vec<&Path> = chunk.iter().map(|p| p.as_path()).collect();
            reference.extend(et.read(&refs));
        }
        let t_exiftool = t.elapsed();

        let (mut hit, mut miss, mut bad) = (0, 0, Vec::new());
        for (p, n) in paths.iter().zip(&native) {
            let Some(n) = n else {
                miss += 1;
                continue;
            };
            hit += 1;
            let e = reference.get(p).cloned().unwrap_or_default();
            if (&n.capture, &n.src, &n.camera) != (&e.capture, &e.src, &e.camera) {
                bad.push(format!("{}: native {:?} {} {:?} / exiftool {:?} {} {:?}", p.display(), n.capture, n.src, n.camera, e.capture, e.src, e.camera));
            }
        }
        println!("{} files: native dated {hit}, left {miss} to exiftool", paths.len());
        println!("native {t_native:.2?} for all, exiftool {t_exiftool:.2?} for all");
        for b in bad.iter().take(20) {
            println!("MISMATCH {b}");
        }
        assert!(bad.is_empty(), "{} mismatches", bad.len());
    }
}
