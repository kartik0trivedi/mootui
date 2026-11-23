//! Two layers of settings:
//!
//!   ~/.config/mootui/config.toml     this Mac: which library, default mode
//!   <library>/.mootui/library.toml   travels with the library: layout,
//!                                    duplicate rule, excludes
//!
//! Missing keys fall back to defaults, so either file can be partial.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::layout;
use crate::util::{clean_typed_path, home};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DupMode {
    /// Same original name, size and capture time (Lightroom's rule).
    Suspected,
    /// Same content hash.
    Verified,
}

impl DupMode {
    pub fn label(self) -> &'static str {
        match self {
            DupMode::Suspected => "Suspected",
            DupMode::Verified => "Content-verified",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            DupMode::Suspected => "suspected",
            DupMode::Verified => "verified",
        }
    }
    pub fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("verified") {
            DupMode::Verified
        } else {
            DupMode::Suspected
        }
    }
    pub fn toggle(self) -> Self {
        match self {
            DupMode::Suspected => DupMode::Verified,
            DupMode::Verified => DupMode::Suspected,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct AppConfig {
    /// Default library folder.
    pub library: String,
    /// "copy" or "move".
    pub mode: String,
    /// Extra place to keep catalog backups (e.g. a cloud-synced folder).
    /// A finished backup file is safe to sync; the live catalog is not.
    pub backup_dir: String,
    /// Imports also write a verified copy of every file here, in the same
    /// layout (e.g. a folder on a backup drive). Empty for none.
    pub second_copy: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig { library: "~/Pictures".into(), mode: "copy".into(), backup_dir: String::new(), second_copy: String::new() }
    }
}

impl AppConfig {
    pub fn path() -> PathBuf {
        home().join(".config/mootui/config.toml")
    }
    pub fn load() -> Self {
        fs::read_to_string(Self::path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }
    pub fn save(&self) -> std::io::Result<()> {
        let p = Self::path();
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(p, toml::to_string_pretty(self).unwrap_or_default())
    }
    pub fn library_path(&self) -> PathBuf {
        clean_typed_path(&self.library)
    }
    pub fn backup_path(&self) -> Option<PathBuf> {
        (!self.backup_dir.trim().is_empty()).then(|| clean_typed_path(&self.backup_dir))
    }
    pub fn second_copy_path(&self) -> Option<PathBuf> {
        (!self.second_copy.trim().is_empty()).then(|| clean_typed_path(&self.second_copy))
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct LibraryConfig {
    /// Where files go, relative to the library. See layout.rs for fields.
    pub layout: String,
    /// "suspected" or "verified".
    pub duplicates: String,
    /// Tidy: move loose duplicates to the Trash instead of only reporting them.
    pub trash_duplicates: bool,
    /// Folder names (or "*.ext" patterns) mootui never looks inside.
    /// macOS package folders (*.photoslibrary, *.lrdata, ...) are always skipped.
    pub exclude: Vec<String>,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        LibraryConfig {
            layout: layout::DEFAULT_TEMPLATE.into(),
            duplicates: "suspected".into(),
            trash_duplicates: false,
            exclude: ["LOG", "Lightroom", "LightCraft Library", "Photo Booth Library"]
                .map(String::from)
                .to_vec(),
        }
    }
}

impl LibraryConfig {
    pub fn path(root: &Path) -> PathBuf {
        root.join(crate::catalog::DIR).join("library.toml")
    }
    pub fn load(root: &Path) -> Self {
        let mut c: Self = fs::read_to_string(Self::path(root))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        if layout::validate(&c.layout).is_err() {
            c.layout = layout::DEFAULT_TEMPLATE.into();
        }
        c
    }
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let p = Self::path(root);
        fs::create_dir_all(p.parent().unwrap())?;
        let body = format!(
            "# mootui library settings. Edit freely; mootui rereads on start.\n\
             #\n\
             # layout fields: {{year}} {{month}} {{day}} {{month_name}} {{kind}} (RAW/JPG/empty)\n\
             #                {{camera}} {{ext}} {{stem}} {{name}}  -- last segment must be {{name}}\n\
             # duplicates:    \"suspected\" (name+size+capture time) or \"verified\" (content)\n\n{}",
            toml::to_string_pretty(self).unwrap_or_default()
        );
        fs::write(p, body)
    }
    pub fn dup_mode(&self) -> DupMode {
        DupMode::parse(&self.duplicates)
    }
}
