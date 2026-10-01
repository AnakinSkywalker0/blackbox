//! Compact SQLite storage: insert, window queries, pruning and thinning.

use crate::model::{ProcRow, Sample};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// Samples older than this are thinned to one per `THIN_STEP_SECS`.
pub const FULL_RES_SECS: i64 = 24 * 3600;
pub const THIN_STEP_SECS: i64 = 10;

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub samples: u64,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub best_mhz: u32,
}

/// Default database location for this OS.
pub fn default_db_path() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("blackbox")
        .join("bb.db")
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> rusqlite::Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // journal_mode returns a row, so query it rather than execute it.
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch(
            "PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS samples (
                 ts INTEGER PRIMARY KEY,
                 cpu INTEGER NOT NULL,      -- percent x10
                 mem_used INTEGER NOT NULL, -- MiB
                 mem_total INTEGER NOT NULL,-- MiB
                 swap_used INTEGER NOT NULL,-- MiB
                 mhz INTEGER NOT NULL,
                 disk INTEGER NOT NULL      -- KiB/s
             );
             CREATE TABLE IF NOT EXISTS procs (
                 ts INTEGER NOT NULL,
                 name TEXT NOT NULL,
                 n INTEGER NOT NULL,
                 cpu INTEGER NOT NULL,      -- percent x10
                 disk INTEGER NOT NULL,     -- KiB/s
                 mem INTEGER NOT NULL,      -- MiB
                 PRIMARY KEY (ts, name)
             ) WITHOUT ROWID;",
        )?;
        Ok(Store { conn })
    }

    pub fn insert(&mut self, s: &Sample) -> rusqlite::Result<()> {
        const MIB: u64 = 1024 * 1024;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO samples VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                s.ts,
                (s.cpu_pct * 10.0).round() as i64,
                (s.mem_used / MIB) as i64,
                (s.mem_total / MIB) as i64,
                (s.swap_used / MIB) as i64,
                s.clock_mhz as i64,
                (s.disk_bps / 1024) as i64
            ],
        )?;
        {
            let mut st = tx.prepare_cached(
                "INSERT OR REPLACE INTO procs VALUES (?1,?2,?3,?4,?5,?6)",
            )?;
            for p in &s.procs {
                st.execute(params![
                    s.ts,
                    p.name,
                    p.count as i64,
                    (p.cpu_pct * 10.0).round() as i64,
                    (p.disk_bps / 1024) as i64,
                    (p.mem_bytes / MIB) as i64
                ])?;
            }
        }
        tx.commit()
    }

    /// All samples with `from <= ts <= to`, oldest first, with their programs.
    pub fn window(&self, from: i64, to: i64) -> rusqlite::Result<Vec<Sample>> {
        const MIB: u64 = 1024 * 1024;
        let mut st = self.conn.prepare_cached(
            "SELECT ts,cpu,mem_used,mem_total,swap_used,mhz,disk FROM samples
             WHERE ts BETWEEN ?1 AND ?2 ORDER BY ts",
        )?;
        let mut out: Vec<Sample> = st
            .query_map(params![from, to], |r| {
                Ok(Sample {
                    ts: r.get(0)?,
                    cpu_pct: r.get::<_, i64>(1)? as f32 / 10.0,
                    mem_used: r.get::<_, i64>(2)? as u64 * MIB,
                    mem_total: r.get::<_, i64>(3)? as u64 * MIB,
                    swap_used: r.get::<_, i64>(4)? as u64 * MIB,
                    clock_mhz: r.get::<_, i64>(5)? as u32,
                    disk_bps: r.get::<_, i64>(6)? as u64 * 1024,
                    procs: Vec::new(),
                })
            })?
            .collect::<Result<_, _>>()?;

        let mut ps = self.conn.prepare_cached(
            "SELECT ts,name,n,cpu,disk,mem FROM procs WHERE ts BETWEEN ?1 AND ?2 ORDER BY ts",
        )?;
        let mut rows = ps.query(params![from, to])?;
        let mut i = 0;
        while let Some(r) = rows.next()? {
            let ts: i64 = r.get(0)?;
            while i < out.len() && out[i].ts < ts {
                i += 1;
            }
            if i < out.len() && out[i].ts == ts {
                out[i].procs.push(ProcRow {
                    name: r.get(1)?,
                    count: r.get::<_, i64>(2)? as u32,
                    cpu_pct: r.get::<_, i64>(3)? as f32 / 10.0,
                    disk_bps: r.get::<_, i64>(4)? as u64 * 1024,
                    mem_bytes: r.get::<_, i64>(5)? as u64 * MIB,
                });
            }
        }
        Ok(out)
    }

    /// Highest clock speed ever recorded, the baseline for throttling detection.
    pub fn best_mhz(&self) -> rusqlite::Result<u32> {
        let v: Option<i64> = self
            .conn
            .query_row("SELECT MAX(mhz) FROM samples", [], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(v.unwrap_or(0) as u32)
    }

    pub fn stats(&self) -> rusqlite::Result<Stats> {
        let (n, first, last): (i64, Option<i64>, Option<i64>) = self.conn.query_row(
            "SELECT COUNT(*), MIN(ts), MAX(ts) FROM samples",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(Stats {
            samples: n as u64,
            first_ts: first,
            last_ts: last,
            best_mhz: self.best_mhz()?,
        })
    }

    /// Deletes data older than `retention_days`, and thins data older than 24h.
    /// Returns the number of samples removed.
    pub fn maintain(&mut self, now: i64, retention_days: u32) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        let cutoff = now - retention_days as i64 * 86400;
        let thin_before = now - FULL_RES_SECS;
        let mut removed = 0;
        removed += tx.execute("DELETE FROM samples WHERE ts < ?1", [cutoff])?;
        tx.execute("DELETE FROM procs WHERE ts < ?1", [cutoff])?;
        removed += tx.execute(
            "DELETE FROM samples WHERE ts < ?1 AND ts % ?2 != 0",
            params![thin_before, THIN_STEP_SECS],
        )?;
        tx.execute(
            "DELETE FROM procs WHERE ts < ?1 AND ts % ?2 != 0",
            params![thin_before, THIN_STEP_SECS],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Reclaims free pages and checkpoints the WAL. Call when idle.
    pub fn compact(&self) -> rusqlite::Result<()> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum;")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64) -> Sample {
        Sample {
            ts,
            cpu_pct: 42.5,
            mem_used: 8 << 30,
            mem_total: 16 << 30,
            swap_used: 512 << 20,
            clock_mhz: 3200,
            disk_bps: 5 << 20,
            procs: vec![ProcRow {
                name: "chrome.exe".into(),
                count: 30,
                cpu_pct: 20.0,
                disk_bps: 1 << 20,
                mem_bytes: 2 << 30,
            }],
        }
    }

    #[test]
    fn roundtrip() {
        let mut s = Store::open_in_memory().unwrap();
        s.insert(&sample(1000)).unwrap();
        s.insert(&sample(1001)).unwrap();
        let w = s.window(1000, 1001).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0], sample(1000));
        assert_eq!(s.best_mhz().unwrap(), 3200);
        let st = s.stats().unwrap();
        assert_eq!((st.samples, st.first_ts, st.last_ts), (2, Some(1000), Some(1001)));
    }

    #[test]
    fn window_bounds() {
        let mut s = Store::open_in_memory().unwrap();
        for t in 0..10 {
            s.insert(&sample(t)).unwrap();
        }
        assert_eq!(s.window(3, 5).unwrap().len(), 3);
        assert!(s.window(100, 200).unwrap().is_empty());
    }

    #[test]
    fn prune_and_thin() {
        let mut s = Store::open_in_memory().unwrap();
        let now = 10 * 86400;
        // Day 1 old: 100 consecutive samples. Fresh: 100 samples. Expired: 20.
        for t in 0..100 {
            s.insert(&sample(now - 2 * 86400 + t)).unwrap();
            s.insert(&sample(now - 100 + t)).unwrap();
        }
        for t in 0..20 {
            s.insert(&sample(now - 9 * 86400 + t)).unwrap();
        }
        s.maintain(now, 7).unwrap();
        // Expired gone, old thinned to every 10th (10 left), fresh intact.
        assert_eq!(s.window(now - 9 * 86400, now - 9 * 86400 + 20).unwrap().len(), 0);
        assert_eq!(s.window(now - 2 * 86400, now - 2 * 86400 + 100).unwrap().len(), 10);
        assert_eq!(s.window(now - 100, now).unwrap().len(), 100);
        // Programs are thinned along with their samples.
        let old = s.window(now - 2 * 86400, now - 2 * 86400 + 100).unwrap();
        assert!(old.iter().all(|x| x.procs.len() == 1));
    }
}
