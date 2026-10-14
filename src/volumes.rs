use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

pub struct Volume {
    pub name: String,
    pub path: PathBuf,
    /// Camera cards keep their files under DCIM; such volumes are listed first.
    pub has_dcim: bool,
    /// Recognises the same card between imports.
    pub uuid: Option<String>,
}

#[derive(Clone)]
struct Info {
    importable: bool,
    uuid: Option<String>,
}

/// diskutil is slow-ish (~50 ms); the list refreshes every 2 s, so remember
/// each mount by (path, device).
static CACHE: Mutex<Option<HashMap<(PathBuf, u64), Info>>> = Mutex::new(None);

fn plist_value<'a>(plist: &'a str, key: &str) -> Option<&'a str> {
    let k = format!("<key>{key}</key>");
    let rest = plist[plist.find(&k)? + k.len()..].trim_start();
    if let Some(r) = rest.strip_prefix("<string>") {
        return Some(&r[..r.find("</string>")?]);
    }
    if rest.starts_with("<true/>") {
        return Some("true");
    }
    if rest.starts_with("<false/>") {
        return Some("false");
    }
    None
}

/// Cards, USB drives and external disks are importable. Disk images
/// (mounted installers) and internal system volumes (Recovery) are not.
fn info(path: &Path) -> Info {
    let Ok(o) = Command::new("diskutil").args(["info", "-plist"]).arg(path).output() else {
        return Info { importable: true, uuid: None };
    };
    let s = String::from_utf8_lossy(&o.stdout);
    let image = plist_value(&s, "BusProtocol") == Some("Disk Image");
    let internal = plist_value(&s, "Internal") == Some("true");
    let uuid = plist_value(&s, "VolumeUUID").or_else(|| plist_value(&s, "DiskUUID")).map(String::from);
    Info { importable: !image && !internal, uuid }
}

/// Mounted external volumes under /Volumes.
///
/// "Macintosh HD" is a symlink to / and is skipped, as is anything on the
/// same device as /Volumes itself (a leftover directory, not a mount).
pub fn list() -> Vec<Volume> {
    let Ok(vols_meta) = fs::metadata("/Volumes") else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir("/Volumes") else {
        return Vec::new();
    };
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);

    let mut out: Vec<Volume> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let sm = fs::symlink_metadata(&path).ok()?;
            if sm.file_type().is_symlink() || !sm.is_dir() || sm.dev() == vols_meta.dev() {
                return None;
            }
            let i = cache.entry((path.clone(), sm.dev())).or_insert_with(|| info(&path)).clone();
            if !i.importable {
                return None;
            }
            let has_dcim = path.join("DCIM").is_dir();
            Some(Volume { name, path, has_dcim, uuid: i.uuid })
        })
        .collect();

    out.sort_by(|a, b| b.has_dcim.cmp(&a.has_dcim).then(a.name.cmp(&b.name)));
    out
}

pub fn eject(path: &Path) -> Result<(), String> {
    let o = Command::new("diskutil").arg("eject").arg(path).output().map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}
