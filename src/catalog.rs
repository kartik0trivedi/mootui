//! The library catalog: <library>/.mootui/catalog.db (SQLite).
//!
//! The files on disk are the truth; the catalog is an index over them plus
//! a history of imports and moves. Everything in `files` can be rebuilt by
//! re-scanning the library. Hashes are filled lazily, only when a duplicate
//! check needs them, and kept forever after (keyed by size + mtime).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::util::now;

pub const DIR: &str = ".mootui";
const SCHEMA_VERSION: i64 = 1;
const KEEP_BACKUPS: usize = 5;

pub struct Catalog {
    pub conn: Connection,
    pub root: PathBuf,
}

#[derive(Clone)]
pub struct Row {
    pub id: i64,
    pub rel: String,
    pub size: u64,
    pub mtime: i64,
    #[allow(dead_code)] // kept on the row for reports; matching queries use SQL
    pub name: String,
    pub capture: Option<String>,
    pub date_src: Option<String>,
    pub camera: Option<String>,
    pub head: Option<Vec<u8>>,
    pub full: Option<Vec<u8>>,
    pub first_seen: String,
}

pub struct NewRow<'a> {
    pub rel: &'a str,
    pub size: u64,
    pub mtime: i64,
    pub name: &'a str,
    pub capture: Option<&'a str>,
    pub date_src: Option<&'a str>,
    pub camera: Option<&'a str>,
    pub head: Option<&'a [u8]>,
    pub full: Option<&'a [u8]>,
    pub import_id: Option<i64>,
}

pub struct Move {
    pub from: String,
    pub to: String,
    pub action: String,
    pub trash_path: Option<String>,
}

pub fn db_path(root: &Path) -> PathBuf {
    root.join(DIR).join("catalog.db")
}

pub fn exists(root: &Path) -> bool {
    db_path(root).exists()
}

const ROW_COLS: &str = "id, rel_path, size, mtime, original_name, capture, date_src, camera, hash_head, hash_full, first_seen";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        rel: r.get(1)?,
        size: r.get::<_, i64>(2)? as u64,
        mtime: r.get(3)?,
        name: r.get(4)?,
        capture: r.get(5)?,
        date_src: r.get(6)?,
        camera: r.get(7)?,
        head: r.get(8)?,
        full: r.get(9)?,
        first_seen: r.get(10)?,
    })
}

impl Catalog {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root.join(DIR))?;
        let conn = Connection::open(db_path(root)).context("opening catalog")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let c = Catalog { conn, root: root.to_path_buf() };
        c.migrate()?;
        Ok(c)
    }

    fn migrate(&self) -> Result<()> {
        let v: i64 = self.conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if v < 1 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE IF NOT EXISTS files (
                    id INTEGER PRIMARY KEY,
                    rel_path TEXT NOT NULL UNIQUE,
                    size INTEGER NOT NULL,
                    mtime INTEGER NOT NULL,
                    original_name TEXT NOT NULL,
                    capture TEXT,
                    date_src TEXT,
                    camera TEXT,
                    hash_head BLOB,
                    hash_full BLOB,
                    hash_algo TEXT NOT NULL DEFAULT 'blake3',
                    import_id INTEGER,
                    first_seen TEXT NOT NULL,
                    state TEXT NOT NULL DEFAULT 'present'
                 );
                 CREATE INDEX IF NOT EXISTS files_name_size ON files(original_name, size);
                 CREATE INDEX IF NOT EXISTS files_size ON files(size);
                 CREATE INDEX IF NOT EXISTS files_full ON files(hash_full);
                 CREATE TABLE IF NOT EXISTS imports (
                    id INTEGER PRIMARY KEY,
                    kind TEXT NOT NULL,
                    started TEXT NOT NULL,
                    finished TEXT,
                    source TEXT,
                    volume_name TEXT,
                    volume_uuid TEXT,
                    mode TEXT,
                    layout TEXT,
                    counts TEXT,
                    undone INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE IF NOT EXISTS moves (
                    id INTEGER PRIMARY KEY,
                    import_id INTEGER NOT NULL,
                    from_rel TEXT NOT NULL,
                    to_rel TEXT NOT NULL,
                    action TEXT NOT NULL,
                    trash_path TEXT
                 );",
            )?;
            let id = blake3::hash(format!("{}{:?}", now(), self.root).as_bytes()).to_hex().to_string();
            self.conn.execute("INSERT OR IGNORE INTO meta VALUES ('library_id', ?1)", [&id[..32]])?;
            self.conn.execute("INSERT OR IGNORE INTO meta VALUES ('created', ?1)", [now()])?;
            self.conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    /// Every row, present or missing: path -> (id, size, mtime, present).
    pub fn all_stat(&self) -> Result<Vec<(i64, String, u64, i64, bool)>> {
        let mut st = self.conn.prepare("SELECT id, rel_path, size, mtime, state = 'present' FROM files")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u64, r.get(3)?, r.get(4)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn present(&self) -> Result<Vec<Row>> {
        let mut st = self.conn.prepare(&format!("SELECT {ROW_COLS} FROM files WHERE state = 'present'"))?;
        let rows = st.query_map([], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn by_name_size(&self, name: &str, size: u64) -> Result<Vec<Row>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT {ROW_COLS} FROM files WHERE state = 'present' AND original_name = ?1 AND size = ?2"
        ))?;
        let rows = st.query_map(params![name, size as i64], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn by_size(&self, size: u64) -> Result<Vec<Row>> {
        let mut st = self.conn.prepare_cached(&format!("SELECT {ROW_COLS} FROM files WHERE state = 'present' AND size = ?1"))?;
        let rows = st.query_map(params![size as i64], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn has_rel(&self, rel: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM files WHERE state = 'present' AND rel_path = ?1", [rel], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn capture_of(&self, rel: &str) -> Option<String> {
        self.conn
            .query_row("SELECT capture FROM files WHERE rel_path = ?1", [rel], |r| r.get::<_, Option<String>>(0))
            .ok()
            .flatten()
    }

    pub fn set_state(&self, id: i64, present: bool) -> Result<()> {
        self.conn.execute("UPDATE files SET state = ?2 WHERE id = ?1", params![id, if present { "present" } else { "missing" }])?;
        Ok(())
    }

    pub fn set_hashes(&self, id: i64, head: Option<&[u8]>, full: Option<&[u8]>) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET hash_head = COALESCE(?2, hash_head), hash_full = COALESCE(?3, hash_full) WHERE id = ?1",
            params![id, head, full],
        )?;
        Ok(())
    }

    /// Insert, or replace whatever was recorded at this path (file changed).
    /// The original name survives a replace when the content did not change
    /// (same size), so renamed files keep matching their card originals.
    pub fn upsert(&self, r: &NewRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO files (rel_path, size, mtime, original_name, capture, date_src, camera,
                                hash_head, hash_full, import_id, first_seen, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'present')
             ON CONFLICT(rel_path) DO UPDATE SET
                original_name = CASE WHEN files.size = excluded.size THEN files.original_name ELSE excluded.original_name END,
                size = excluded.size, mtime = excluded.mtime,
                capture = excluded.capture, date_src = excluded.date_src, camera = excluded.camera,
                hash_head = excluded.hash_head, hash_full = excluded.hash_full,
                import_id = COALESCE(excluded.import_id, files.import_id), state = 'present'",
            params![
                r.rel,
                r.size as i64,
                r.mtime,
                r.name,
                r.capture,
                r.date_src,
                r.camera,
                r.head,
                r.full,
                r.import_id,
                now()
            ],
        )?;
        Ok(())
    }

    pub fn rename_rel(&self, from: &str, to: &str) -> Result<()> {
        self.conn.execute("DELETE FROM files WHERE rel_path = ?1 AND state = 'missing'", [to])?;
        self.conn.execute("UPDATE files SET rel_path = ?2 WHERE rel_path = ?1", [from, to])?;
        Ok(())
    }

    pub fn delete_rel(&self, rel: &str) -> Result<()> {
        self.conn.execute("DELETE FROM files WHERE rel_path = ?1", [rel])?;
        Ok(())
    }

    // --- history -------------------------------------------------------------

    pub fn begin_import(&self, kind: &str, source: &str, volume: Option<(&str, &str)>, mode: &str, layout: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO imports (kind, started, source, volume_name, volume_uuid, mode, layout) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![kind, now(), source, volume.map(|v| v.0), volume.map(|v| v.1), mode, layout],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_import(&self, id: i64, counts: &str) -> Result<()> {
        self.conn.execute("UPDATE imports SET finished = ?2, counts = ?3 WHERE id = ?1", params![id, now(), counts])?;
        Ok(())
    }

    pub fn record_move(&self, import_id: i64, from: &str, to: &str, action: &str, trash: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO moves (import_id, from_rel, to_rel, action, trash_path) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![import_id, from, to, action, trash],
        )?;
        Ok(())
    }

    /// Most recent tidy/reorganize that moved something and was not undone.
    pub fn last_undoable(&self) -> Result<Option<(i64, String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT i.id, i.kind, i.started FROM imports i
                 WHERE i.kind IN ('tidy', 'reorganize') AND i.undone = 0
                   AND EXISTS (SELECT 1 FROM moves m WHERE m.import_id = i.id)
                 ORDER BY i.id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    pub fn moves_for(&self, import_id: i64) -> Result<Vec<Move>> {
        let mut st = self.conn.prepare("SELECT from_rel, to_rel, action, trash_path FROM moves WHERE import_id = ?1 ORDER BY id DESC")?;
        let rows = st.query_map([import_id], |r| Ok(Move { from: r.get(0)?, to: r.get(1)?, action: r.get(2)?, trash_path: r.get(3)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark_undone(&self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE imports SET undone = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    /// (when, files copied) of the last import from this card.
    pub fn last_import_from(&self, volume_uuid: &str) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT i.started, (SELECT COUNT(*) FROM files f WHERE f.import_id = i.id)
                 FROM imports i WHERE i.kind = 'import' AND i.volume_uuid = ?1 ORDER BY i.id DESC LIMIT 1",
                [volume_uuid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn count_present(&self) -> i64 {
        self.conn.query_row("SELECT COUNT(*) FROM files WHERE state = 'present'", [], |r| r.get(0)).unwrap_or(0)
    }

    /// Snapshot to .mootui/backups (keeping the newest few), and optionally
    /// copy it to an extra folder. Import history and original names are the
    /// only things a re-scan cannot rebuild; this is what protects them.
    pub fn backup(&self, extra: Option<&Path>) -> Result<PathBuf> {
        let dir = self.root.join(DIR).join("backups");
        fs::create_dir_all(&dir)?;
        let name = format!("catalog-{}.db", chrono::Local::now().format("%Y-%m-%d_%H%M%S"));
        let dest = dir.join(&name);
        let _ = fs::remove_file(&dest);
        self.conn.execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;

        let mut old: Vec<PathBuf> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("catalog-")))
            .collect();
        old.sort();
        while old.len() > KEEP_BACKUPS {
            let _ = fs::remove_file(old.remove(0));
        }

        if let Some(extra) = extra {
            fs::create_dir_all(extra)?;
            fs::copy(&dest, extra.join(format!("mootui-{name}")))?;
        }
        Ok(dest)
    }
}
