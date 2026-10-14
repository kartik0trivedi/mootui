//! Layout templates: where a file goes inside the library.
//!
//!   "{year}-{month}/{kind}/{name}"   ->  2026-07/RAW/DSCF7319.RAF
//!
//! A segment that renders empty is dropped, so a video (kind "") lands at
//! 2026-07/clip.mov. Sidecars are not routed by template; they follow the
//! file they belong to.

use chrono::NaiveDateTime;

pub const DEFAULT_TEMPLATE: &str = "{year}-{month}/{kind}/{name}";

pub const PRESETS: &[(&str, &str)] = &[
    ("Month / kind", DEFAULT_TEMPLATE),
    ("Month", "{year}-{month}/{name}"),
    ("Year / day (Lightroom style)", "{year}/{year}-{month}-{day}/{name}"),
    ("Year / month", "{year}/{month}/{name}"),
    ("Camera / year / month", "{camera}/{year}/{year}-{month}/{name}"),
];

const TOKENS: &[&str] = &["year", "month", "day", "month_name", "kind", "camera", "ext", "stem", "name"];

pub fn preset_name(template: &str) -> &'static str {
    PRESETS.iter().find(|(_, t)| *t == template).map(|(n, _)| *n).unwrap_or("Custom")
}

pub fn next_preset(template: &str, step: isize) -> &'static str {
    let n = PRESETS.len() as isize;
    let i = PRESETS.iter().position(|(_, t)| *t == template).map(|i| i as isize).unwrap_or(-1);
    PRESETS[((i + step).rem_euclid(n)) as usize].1
}

enum Part<'a> {
    Lit(&'a str),
    Tok(&'a str),
}

fn parse_segment(seg: &str) -> Result<Vec<Part<'_>>, String> {
    let mut out = Vec::new();
    let mut rest = seg;
    while !rest.is_empty() {
        match rest.find('{') {
            Some(0) => {
                let end = rest.find('}').ok_or_else(|| format!("unclosed {{ in \"{seg}\""))?;
                let tok = &rest[1..end];
                if !TOKENS.contains(&tok) {
                    return Err(format!("unknown field {{{tok}}}"));
                }
                out.push(Part::Tok(tok));
                rest = &rest[end + 1..];
            }
            Some(i) => {
                out.push(Part::Lit(&rest[..i]));
                rest = &rest[i..];
            }
            None => {
                out.push(Part::Lit(rest));
                rest = "";
            }
        }
    }
    Ok(out)
}

pub fn validate(template: &str) -> Result<(), String> {
    let segs: Vec<&str> = template.split('/').collect();
    if segs.last() != Some(&"{name}") {
        return Err("the last part of the layout must be {name}".into());
    }
    if segs.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return Err("empty or relative path segment in layout".into());
    }
    for s in segs {
        parse_segment(s)?;
    }
    Ok(())
}

pub struct Fields<'a> {
    /// "YYYY-MM-DD HH:MM:SS"
    pub capture: &'a str,
    pub camera: Option<&'a str>,
    pub name: &'a str,
    pub ext: &'a str,
}

fn clean(s: &str) -> String {
    s.trim().replace(['/', ':'], "-")
}

/// Relative destination path for a file.
pub fn render(template: &str, f: &Fields) -> String {
    let dt = NaiveDateTime::parse_from_str(f.capture, "%Y-%m-%d %H:%M:%S").ok();
    let value = |tok: &str| -> String {
        match tok {
            "year" => f.capture.get(0..4).unwrap_or("0000").to_string(),
            "month" => f.capture.get(5..7).unwrap_or("00").to_string(),
            "day" => f.capture.get(8..10).unwrap_or("00").to_string(),
            "month_name" => dt.map(|d| d.format("%B").to_string()).unwrap_or_default(),
            "kind" => crate::media::kind(f.ext).to_string(),
            "camera" => clean(f.camera.filter(|c| !c.trim().is_empty()).unwrap_or("Unknown camera")),
            "ext" => f.ext.to_string(),
            "stem" => f.name.rsplit_once('.').map(|(s, _)| s).unwrap_or(f.name).to_string(),
            _ => f.name.to_string(),
        }
    };
    let mut segs = Vec::new();
    for seg in template.split('/') {
        let Ok(parts) = parse_segment(seg) else { continue };
        let s: String = parts
            .iter()
            .map(|p| match p {
                Part::Lit(l) => l.to_string(),
                Part::Tok(t) => value(t),
            })
            .collect();
        if !s.trim().is_empty() {
            segs.push(s);
        }
    }
    segs.join("/")
}

/// Does a top-level folder name look like one this template creates?
/// Tidy only moves files that are *not* inside such a folder; files already
/// in a sorted-looking folder are left alone unless reorganizing.
pub fn first_segment_matches(template: &str, folder: &str) -> bool {
    let first = template.split('/').next().unwrap_or("");
    if first == "{name}" {
        return false;
    }
    let Ok(parts) = parse_segment(first) else { return false };
    match_parts(&parts, folder)
}

fn match_parts(parts: &[Part], s: &str) -> bool {
    let Some((head, tail)) = parts.split_first() else {
        return s.is_empty();
    };
    match head {
        Part::Lit(l) => s.strip_prefix(*l).is_some_and(|rest| match_parts(tail, rest)),
        Part::Tok(t) => {
            let fixed = |n: usize| s.len() >= n && s[..n].bytes().all(|b| b.is_ascii_digit()) && match_parts(tail, &s[n..]);
            match *t {
                "year" => fixed(4),
                "month" | "day" => fixed(2),
                "kind" => ["RAW", "JPG"].iter().any(|k| s.strip_prefix(k).is_some_and(|r| match_parts(tail, r))),
                "month_name" => (1..=s.len())
                    .filter(|&i| s.is_char_boundary(i))
                    .any(|i| s[..i].chars().all(char::is_alphabetic) && match_parts(tail, &s[i..])),
                _ => (1..=s.len()).filter(|&i| s.is_char_boundary(i)).any(|i| match_parts(tail, &s[i..])),
            }
        }
    }
}
