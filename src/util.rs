use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn human_duration(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// RFC 4180 quoting: wrap in quotes when needed, double any embedded quote.
pub fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

pub fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// macOS and exFAT may store the same accented name in different Unicode
/// forms; compare and store names in NFC.
pub fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// Full-file BLAKE3.
pub fn hash_file(p: &Path) -> io::Result<[u8; 32]> {
    let mut f = File::open(p)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(*h.finalize().as_bytes())
}

const HEAD: u64 = 64 * 1024;

/// Quick fingerprint: size + first 64 KB + last 64 KB. Two files with
/// different quick hashes are certainly different; equal ones still need the
/// full hash. Reads 128 KB instead of the whole file.
pub fn hash_head(p: &Path) -> io::Result<[u8; 32]> {
    let mut f = File::open(p)?;
    let len = f.metadata()?.len();
    let mut h = blake3::Hasher::new();
    h.update(&len.to_le_bytes());
    let mut buf = vec![0u8; HEAD as usize];
    let n = read_full(&mut f, &mut buf)?;
    h.update(&buf[..n]);
    if len > 2 * HEAD {
        f.seek(SeekFrom::Start(len - HEAD))?;
        let n = read_full(&mut f, &mut buf)?;
        h.update(&buf[..n]);
    } else if len > HEAD {
        let n = read_full(&mut f, &mut buf)?;
        h.update(&buf[..n]);
    }
    Ok(*h.finalize().as_bytes())
}

/// Read until the buffer is full or EOF.
pub fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Turn what the user typed or dragged into the terminal into a path.
/// Finder drag-and-drop yields backslash-escaped or quoted paths, often with
/// a trailing space.
pub fn clean_typed_path(s: &str) -> PathBuf {
    let s = s.trim();
    let s = s
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .or_else(|| s.strip_prefix('"').and_then(|t| t.strip_suffix('"')))
        .unwrap_or(s);

    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }

    if out == "~" {
        return home();
    }
    if let Some(rest) = out.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(out)
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Show a path with ~ for the home folder.
pub fn tilde(p: &Path) -> String {
    match p.strip_prefix(home()) {
        Ok(r) if r.as_os_str().is_empty() => "~".into(),
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}

/// A live SQLite database must not sit in a folder a sync client rewrites.
pub fn is_cloud_synced(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s.contains("/Library/CloudStorage/") || s.contains("/Library/Mobile Documents/") || s.contains("/Dropbox/")
}
