//! File-type rules: which files are media, which are raw, which are sidecars,
//! and how files group by basename.

pub const RAW_EXTS: &[&str] = &[
    "raf", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "dng", "orf", "rw2", "raw",
    "pef", "srw", "x3f", "3fr", "erf", "mef", "mos", "iiq", "k25", "kdc", "dcr",
];
const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "jpe", "heic", "heif", "hif", "png", "tif", "tiff", "gif", "webp",
];
const VIDEO_EXTS: &[&str] = &[
    "mov", "mp4", "m4v", "avi", "mts", "m2ts", "mpg", "mpeg", "3gp", "insv", "insp", "lrv",
];
/// Edit/metadata files that belong next to the photo they describe.
const SIDECAR_EXTS: &[&str] = &["xmp", "pp3", "aae", "dop", "thm", "on1"];

pub fn ext_lower(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

pub fn is_raw(ext: &str) -> bool {
    RAW_EXTS.contains(&ext)
}

pub fn is_image(ext: &str) -> bool {
    is_raw(ext) || IMAGE_EXTS.contains(&ext)
}

pub fn is_sidecar(ext: &str) -> bool {
    SIDECAR_EXTS.contains(&ext)
}

pub fn is_media(ext: &str) -> bool {
    is_image(ext) || VIDEO_EXTS.contains(&ext) || is_sidecar(ext)
}

/// {kind} in layout templates. Unknown kinds render empty, which drops the
/// folder level, so videos land at the month root as before.
pub fn kind(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" | "jpe" => "JPG",
        e if is_raw(e) => "RAW",
        _ => "",
    }
}

/// Basename used to group a photo with its jpeg and sidecars. Strips one
/// extension, and a second when the first was a sidecar on a media file, so
/// DSCF0001.RAF.pp3 groups with DSCF0001.RAF.
pub fn group_stem(name: &str) -> String {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return name.to_string();
    };
    if is_sidecar(&ext.to_ascii_lowercase()) {
        if let Some((inner, inner_ext)) = stem.rsplit_once('.') {
            if is_media(&inner_ext.to_ascii_lowercase()) {
                return inner.to_string();
            }
        }
    }
    stem.to_string()
}

/// Which member of a basename group a sidecar belongs to.
/// "DSCF1.RAF.pp3" -> "DSCF1.RAF" exactly; "DSCF1.xmp" -> the raw if any,
/// else any non-sidecar member.
pub fn sidecar_parent<'a>(sidecar: &str, members: &[&'a str]) -> Option<&'a str> {
    if let Some((inner, _)) = sidecar.rsplit_once('.') {
        if let Some(m) = members.iter().find(|m| **m == inner) {
            return Some(m);
        }
    }
    let non_side: Vec<&&str> = members.iter().filter(|m| !is_sidecar(&ext_lower(m))).collect();
    non_side
        .iter()
        .find(|m| is_raw(&ext_lower(m)))
        .or_else(|| non_side.first())
        .map(|m| **m)
}

/// Rank for picking the member whose date speaks for the group.
pub fn date_rank(ext: &str) -> u8 {
    if is_raw(ext) {
        0
    } else if is_sidecar(ext) {
        2
    } else {
        1
    }
}
