//! Every bit of crawl state in one SQLite file, so a killed crawl carries on.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;
use rusqlite::OptionalExtension as _;
use rusqlite::params;
use serde_json::Value;

use crate::Reading;

/// Seconds after a failure before the page is tried again; past the last, it is given up.
pub const BACKOFF: [i64; 3] = [30, 120, 480];

/// A file the crawl saved to disk.
#[derive(Debug, Clone)]
pub struct File {
    pub url: String,
    pub final_url: String,
    pub content_type: Option<String>,
    pub bytes: u64,
    pub sha256: String,
    pub path: String,
}

#[derive(Debug)]
pub struct Store {
    db: Mutex<Connection>,
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Counts {
    pub queued: u64,
    pub taken: u64,
    pub done: u64,
    pub failed: u64,
    pub skipped: u64,
    /// Queued but waiting out a backoff.
    pub waiting: u64,
    /// Of `done`, how many were files rather than pages.
    pub files: u64,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
}

impl Store {
    /// Opens or creates the crawl, and puts back every page that was open when
    /// it last stopped.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be opened as SQLite.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS frontier (
                 url TEXT PRIMARY KEY,
                 host TEXT NOT NULL,
                 depth INTEGER NOT NULL,
                 state TEXT NOT NULL DEFAULT 'queued',
                 attempts INTEGER NOT NULL DEFAULT 0,
                 not_before INTEGER NOT NULL DEFAULT 0,
                 found_on TEXT,
                 error TEXT
             );
             CREATE INDEX IF NOT EXISTS frontier_next ON frontier (state, depth, not_before);
             CREATE TABLE IF NOT EXISTS pages (
                 url TEXT PRIMARY KEY,
                 final_url TEXT NOT NULL,
                 status INTEGER,
                 title TEXT,
                 lang TEXT,
                 markdown TEXT,
                 links TEXT,
                 jsonld TEXT,
                 meta TEXT,
                 hash TEXT,
                 same_as TEXT,
                 fetched_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS pages_hash ON pages (hash);
             CREATE TABLE IF NOT EXISTS files (
                 url TEXT PRIMARY KEY,
                 final_url TEXT NOT NULL,
                 content_type TEXT,
                 bytes INTEGER NOT NULL,
                 sha256 TEXT NOT NULL,
                 path TEXT NOT NULL,
                 fetched_at INTEGER NOT NULL
             );
             UPDATE frontier SET state = 'queued' WHERE state = 'taken';",
        )?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().expect("store lock")
    }

    /// Adds a URL unless it is already known. Returns whether it was new.
    ///
    /// # Errors
    ///
    /// Fails when SQLite does.
    pub fn add(
        &self,
        url: &str,
        host: &str,
        depth: u32,
        found_on: Option<&str>,
    ) -> rusqlite::Result<bool> {
        self.db()
            .execute(
                "INSERT OR IGNORE INTO frontier (url, host, depth, found_on) VALUES (?1, ?2, ?3, ?4)",
                params![url, host, depth, found_on],
            )
            .map(|n| n > 0)
    }

    /// Records a URL that will never be fetched, and why, so it is not offered again.
    ///
    /// # Errors
    ///
    /// Fails when SQLite does.
    pub fn skip(&self, url: &str, host: &str, depth: u32, why: &str) -> rusqlite::Result<()> {
        self.db().execute(
            "INSERT OR IGNORE INTO frontier (url, host, depth, state, error) VALUES (?1, ?2, ?3, 'skipped', ?4)",
            params![url, host, depth, why],
        )?;
        Ok(())
    }

    /// Gives up a taken URL without counting it a failure.
    ///
    /// # Errors
    ///
    /// Fails when SQLite does.
    pub fn set_skipped(&self, url: &str, why: &str) -> rusqlite::Result<()> {
        self.db().execute(
            "UPDATE frontier SET state = 'skipped', error = ?2 WHERE url = ?1",
            params![url, why],
        )?;
        Ok(())
    }

    /// The hosts of the seeds, for a crawl resumed without them.
    ///
    /// # Errors
    ///
    /// Fails when SQLite does.
    pub fn seed_hosts(&self) -> rusqlite::Result<Vec<String>> {
        let db = self.db();
        let mut query = db.prepare("SELECT DISTINCT host FROM frontier WHERE depth = 0")?;
        query.query_map([], |row| row.get(0))?.collect()
    }

    /// Takes the shallowest queued URL whose host `ready` says may be visited now.
    ///
    /// # Errors
    ///
    /// Fails when SQLite does.
    pub fn take(
        &self,
        ready: impl Fn(&str) -> bool,
    ) -> rusqlite::Result<Option<(String, String, u32)>> {
        let db = self.db();
        let mut query = db.prepare_cached(
            "SELECT url, host, depth FROM frontier WHERE state = 'queued' AND not_before <= ?1 \
             ORDER BY depth, rowid LIMIT 200",
        )?;
        let rows = query
            .query_map(params![now()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Some(next) = rows.into_iter().find(|(_, host, _)| ready(host)) else {
            return Ok(None);
        };
        db.execute(
            "UPDATE frontier SET state = 'taken' WHERE url = ?1",
            params![next.0],
        )?;
        Ok(Some(next))
    }

    /// # Errors
    ///
    /// Fails when SQLite does.
    pub fn fail(&self, url: &str, error: &str, retry: bool) -> rusqlite::Result<()> {
        let db = self.db();
        let attempts: i64 = db.query_row(
            "SELECT attempts FROM frontier WHERE url = ?1",
            params![url],
            |row| row.get(0),
        )?;
        let wait = usize::try_from(attempts)
            .ok()
            .and_then(|n| BACKOFF.get(n))
            .filter(|_| retry);
        match wait {
            Some(wait) => db.execute(
                "UPDATE frontier SET state = 'queued', attempts = attempts + 1, not_before = ?2, error = ?3 WHERE url = ?1",
                params![url, now() + wait, error],
            )?,
            None => db.execute(
                "UPDATE frontier SET state = 'failed', attempts = attempts + 1, error = ?2 WHERE url = ?1",
                params![url, error],
            )?,
        };
        Ok(())
    }

    /// Keeps a reading. A page whose text matches one already kept is stored by
    /// reference, `same_as`, rather than twice.
    ///
    /// # Errors
    ///
    /// Fails when SQLite does.
    pub fn keep(
        &self,
        url: &str,
        reading: &Reading,
        hash: &str,
    ) -> rusqlite::Result<Option<String>> {
        let db = self.db();
        let same_as: Option<String> = db
            .query_row(
                "SELECT url FROM pages WHERE hash = ?1 AND url != ?2 AND same_as IS NULL LIMIT 1",
                params![hash, url],
                |row| row.get(0),
            )
            .optional()?;
        let markdown = same_as.is_none().then_some(reading.markdown.as_str());
        db.execute(
            "INSERT OR REPLACE INTO pages (url, final_url, status, title, lang, markdown, links, jsonld, meta, hash, same_as, fetched_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                url,
                reading.url,
                reading.status,
                reading.title,
                reading.lang,
                markdown,
                to_json(&reading.links),
                to_json(&reading.jsonld),
                to_json(&reading.meta),
                hash,
                same_as,
                now()
            ],
        )?;
        db.execute(
            "UPDATE frontier SET state = 'done', error = NULL WHERE url = ?1",
            params![url],
        )?;
        Ok(same_as)
    }

    /// # Errors
    ///
    /// Fails when SQLite does.
    pub fn counts(&self) -> rusqlite::Result<Counts> {
        let db = self.db();
        let mut counts = Counts::default();
        let mut query = db.prepare_cached(
            "SELECT state, not_before > ?1, count(*) FROM frontier GROUP BY 1, 2",
        )?;
        let rows = query.query_map(params![now()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, u64>(2)?,
            ))
        })?;
        for row in rows {
            let (state, later, n) = row?;
            match (state.as_str(), later) {
                ("queued", true) => counts.waiting += n,
                ("queued", false) => counts.queued += n,
                ("taken", _) => counts.taken += n,
                ("done", _) => counts.done += n,
                ("failed", _) => counts.failed += n,
                _ => counts.skipped += n,
            }
        }
        counts.files = db.query_row("SELECT count(*) FROM files", [], |row| row.get(0))?;
        Ok(counts)
    }

    /// Records a file saved to disk at `path`, and marks its URL done.
    ///
    /// # Errors
    ///
    /// Fails when SQLite does.
    pub fn keep_file(&self, file: &File) -> rusqlite::Result<()> {
        let db = self.db();
        db.execute(
            "INSERT OR REPLACE INTO files (url, final_url, content_type, bytes, sha256, path, fetched_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![file.url, file.final_url, file.content_type, file.bytes, file.sha256, file.path, now()],
        )?;
        db.execute(
            "UPDATE frontier SET state = 'done', error = NULL WHERE url = ?1",
            params![file.url],
        )?;
        Ok(())
    }

    /// Every kept page as one JSON object per line, duplicates included by reference.
    ///
    /// # Errors
    ///
    /// Fails when SQLite or the writer does.
    pub fn export(
        &self,
        out: &mut impl std::io::Write,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let db = self.db();
        let mut query = db.prepare(
            "SELECT p.url, p.final_url, p.status, p.title, p.lang, coalesce(p.markdown, ''), p.links, p.jsonld, p.meta, p.same_as, p.fetched_at, f.depth \
             FROM pages p JOIN frontier f ON f.url = p.url ORDER BY f.depth, f.rowid",
        )?;
        let mut rows = query.query([])?;
        let parse = |text: Option<String>| {
            text.and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .unwrap_or(Value::Null)
        };
        while let Some(row) = rows.next()? {
            let line = serde_json::json!({
                "kind": "page",
                "url": row.get::<_, String>(0)?,
                "final_url": row.get::<_, String>(1)?,
                "status": row.get::<_, Option<u16>>(2)?,
                "title": row.get::<_, Option<String>>(3)?,
                "lang": row.get::<_, Option<String>>(4)?,
                "markdown": row.get::<_, String>(5)?,
                "links": parse(row.get(6)?),
                "jsonld": parse(row.get(7)?),
                "meta": parse(row.get(8)?),
                "same_as": row.get::<_, Option<String>>(9)?,
                "fetched_at": row.get::<_, i64>(10)?,
                "depth": row.get::<_, u32>(11)?,
            });
            writeln!(out, "{line}")?;
        }
        let mut query = db.prepare(
            "SELECT x.url, x.final_url, x.content_type, x.bytes, x.sha256, x.path, x.fetched_at, f.depth, f.found_on \
             FROM files x JOIN frontier f ON f.url = x.url ORDER BY f.depth, f.rowid",
        )?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let line = serde_json::json!({
                "kind": "file",
                "url": row.get::<_, String>(0)?,
                "final_url": row.get::<_, String>(1)?,
                "content_type": row.get::<_, Option<String>>(2)?,
                "bytes": row.get::<_, i64>(3)?,
                "sha256": row.get::<_, String>(4)?,
                "path": row.get::<_, String>(5)?,
                "fetched_at": row.get::<_, i64>(6)?,
                "depth": row.get::<_, u32>(7)?,
                "found_on": row.get::<_, Option<String>>(8)?,
            });
            writeln!(out, "{line}")?;
        }
        Ok(())
    }

    /// The failures, newest reason first, for a report at the end.
    ///
    /// # Errors
    ///
    /// Fails when SQLite does.
    pub fn failures(&self, limit: u32) -> rusqlite::Result<Vec<(String, String)>> {
        let db = self.db();
        let mut query = db.prepare("SELECT url, coalesce(error, '') FROM frontier WHERE state = 'failed' ORDER BY rowid DESC LIMIT ?1")?;
        query
            .query_map(params![limit], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }
}

fn to_json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(text: &str) -> Reading {
        serde_json::from_value(serde_json::json!({
            "url": "https://a.test/", "canonical": null, "status": 200, "title": "t", "lang": "en",
            "markdown": text, "links": [], "jsonld": [], "meta": {}, "actions": []
        }))
        .unwrap()
    }

    #[test]
    fn a_killed_crawl_puts_its_open_pages_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sqlite");
        {
            let store = Store::open(&path).unwrap();
            store.add("https://a.test/1", "a.test", 0, None).unwrap();
            assert!(store.take(|_| true).unwrap().is_some());
            assert_eq!(store.counts().unwrap().taken, 1);
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.counts().unwrap().queued, 1);
    }

    #[test]
    fn failures_back_off_then_give_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c.sqlite")).unwrap();
        store.add("https://a.test/1", "a.test", 0, None).unwrap();
        for _ in 0..BACKOFF.len() {
            store.fail("https://a.test/1", "boom", true).unwrap();
            assert_eq!(store.counts().unwrap().waiting, 1);
        }
        store.fail("https://a.test/1", "boom", true).unwrap();
        assert_eq!(store.counts().unwrap().failed, 1);
    }

    #[test]
    fn the_same_text_is_kept_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c.sqlite")).unwrap();
        for url in ["https://a.test/1", "https://a.test/2"] {
            store.add(url, "a.test", 0, None).unwrap();
        }
        assert_eq!(
            store
                .keep("https://a.test/1", &reading("same"), "h")
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .keep("https://a.test/2", &reading("same"), "h")
                .unwrap()
                .as_deref(),
            Some("https://a.test/1")
        );
        let mut out = Vec::new();
        store.export(&mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 2);
    }

    #[test]
    fn a_busy_host_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("c.sqlite")).unwrap();
        store.add("https://a.test/1", "a.test", 0, None).unwrap();
        store.add("https://b.test/1", "b.test", 0, None).unwrap();
        let taken = store.take(|host| host != "a.test").unwrap().unwrap();
        assert_eq!(taken.1, "b.test");
    }
}
