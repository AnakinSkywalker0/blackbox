//! Compact SQLite storage: insert, window queries, pruning and thinning.

use crate::model::{ProcRow, Sample, Sensors};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// Samples older than this are thinned to one per `THIN_STEP_SECS`.
pub const FULL_RES_SECS: i64 = 24 * 3600;
pub const THIN_STEP_SECS: i64 = 10;

/// Nullable columns added in schema v1. NULL means the sensor was unavailable.
/// Scaled integers keep rows small: temps, gpu and frequency x10, queue x100.
const SENSOR_COLUMNS: [&str; 11] =
    ["ac", "batt", "saver", "temp", "fpct", "gpu", "gtemp", "gthr", "dq", "dlat", "pgo"];

fn scaled(v: Option<f32>, k: f32) -> Option<i64> {
    v.map(|x| (x * k).round() as i64)
}

fn unscaled(v: Option<i64>, k: f32) -> Option<f32> {
    v.map(|x| x as f32 / k)
}

/// One recorded `bb why` answer and, once the user gives it, the verdict on it.
#[derive(Debug, Clone, PartialEq)]
pub struct WhyLog {
    pub id: i64,
    pub ts: i64,
    pub from_ts: i64,
    pub to_ts: i64,
    /// Finding titles, best first. Empty means "no clear cause".
    pub titles: Vec<String>,
    pub version: String,
    pub verdict: Option<String>,
    pub note: Option<String>,
}

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
        // Must come before the first write: it only takes effect on a brand-new database.
        conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL")?;
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
             ) WITHOUT ROWID;
             -- Every `bb why` result, so the user can say whether it was right.
             CREATE TABLE IF NOT EXISTS why_log (
                 id INTEGER PRIMARY KEY,
                 ts INTEGER NOT NULL,
                 from_ts INTEGER NOT NULL,
                 to_ts INTEGER NOT NULL,
                 titles TEXT NOT NULL,   -- one finding title per line, best first
                 version TEXT NOT NULL,
                 verdict TEXT,           -- right | partly | wrong
                 note TEXT
             );",
        )?;
        Self::migrate(&conn)?;
        Ok(Store { conn })
    }

    /// Adds sensor columns to databases created by older versions.
    fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 2 {
            let have: Vec<String> = conn
                .prepare("SELECT name FROM pragma_table_info('samples')")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            for col in SENSOR_COLUMNS {
                if !have.iter().any(|h| h == col) {
                    conn.execute_batch(&format!("ALTER TABLE samples ADD COLUMN {col} INTEGER"))?;
                }
            }
            conn.execute_batch("PRAGMA user_version = 2")?;
        }
        Ok(())
    }

    pub fn insert(&mut self, s: &Sample) -> rusqlite::Result<()> {
        self.insert_many(std::slice::from_ref(s))
    }

    /// Inserts several samples in one transaction.
    pub fn insert_many(&mut self, samples: &[Sample]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        for s in samples {
            Self::insert_in(&tx, s)?;
        }
        tx.commit()
    }

    fn insert_in(tx: &rusqlite::Transaction, s: &Sample) -> rusqlite::Result<()> {
        const MIB: u64 = 1024 * 1024;
        let x = &s.sensors;
        tx.execute(
            "INSERT OR REPLACE INTO samples
             (ts,cpu,mem_used,mem_total,swap_used,mhz,disk,ac,batt,saver,temp,fpct,gpu,gtemp,gthr,dq,dlat,pgo)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                s.ts,
                (s.cpu_pct * 10.0).round() as i64,
                (s.mem_used / MIB) as i64,
                (s.mem_total / MIB) as i64,
                (s.swap_used / MIB) as i64,
                s.clock_mhz as i64,
                (s.disk_bps / 1024) as i64,
                x.on_ac.map(i64::from),
                x.battery_pct.map(i64::from),
                x.battery_saver.map(i64::from),
                scaled(x.temp_c, 10.0),
                scaled(x.freq_pct, 10.0),
                scaled(x.gpu_pct, 10.0),
                scaled(x.gpu_temp_c, 10.0),
                x.gpu_throttle.map(i64::from),
                scaled(x.disk_queue, 100.0),
                scaled(x.disk_latency_ms, 10.0),
                scaled(x.page_out, 1.0),
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
        Ok(())
    }

    /// All samples with `from <= ts <= to`, oldest first, with their programs.
    pub fn window(&self, from: i64, to: i64) -> rusqlite::Result<Vec<Sample>> {
        const MIB: u64 = 1024 * 1024;
        let mut st = self.conn.prepare_cached(
            "SELECT ts,cpu,mem_used,mem_total,swap_used,mhz,disk,
                    ac,batt,saver,temp,fpct,gpu,gtemp,gthr,dq,dlat,pgo FROM samples
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
                    sensors: Sensors {
                        on_ac: r.get::<_, Option<i64>>(7)?.map(|v| v != 0),
                        battery_pct: r.get::<_, Option<i64>>(8)?.map(|v| v as u8),
                        battery_saver: r.get::<_, Option<i64>>(9)?.map(|v| v != 0),
                        temp_c: unscaled(r.get(10)?, 10.0),
                        freq_pct: unscaled(r.get(11)?, 10.0),
                        gpu_pct: unscaled(r.get(12)?, 10.0),
                        gpu_temp_c: unscaled(r.get(13)?, 10.0),
                        gpu_throttle: r.get::<_, Option<i64>>(14)?.map(|v| v as u8),
                        disk_queue: unscaled(r.get(15)?, 100.0),
                        disk_latency_ms: unscaled(r.get(16)?, 10.0),
                        page_out: unscaled(r.get(17)?, 1.0),
                    },
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

    pub fn log_why(&self, ts: i64, from: i64, to: i64, titles: &[String], version: &str) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO why_log (ts, from_ts, to_ts, titles, version) VALUES (?1,?2,?3,?4,?5)",
            params![ts, from, to, titles.join("\n"), version],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Records a verdict on the most recent `bb why`. Returns it, or None if there is none.
    pub fn set_verdict(&self, verdict: &str, note: Option<&str>) -> rusqlite::Result<Option<WhyLog>> {
        let id: Option<i64> = self.conn.query_row("SELECT MAX(id) FROM why_log", [], |r| r.get(0))?;
        let Some(id) = id else { return Ok(None) };
        self.conn.execute("UPDATE why_log SET verdict=?1, note=?2 WHERE id=?3", params![verdict, note, id])?;
        Ok(self.why_log()?.into_iter().find(|w| w.id == id))
    }

    pub fn why_log(&self) -> rusqlite::Result<Vec<WhyLog>> {
        let mut st = self.conn.prepare("SELECT id,ts,from_ts,to_ts,titles,version,verdict,note FROM why_log ORDER BY id")?;
        let rows = st.query_map([], |r| {
            let titles: String = r.get(4)?;
            Ok(WhyLog {
                id: r.get(0)?,
                ts: r.get(1)?,
                from_ts: r.get(2)?,
                to_ts: r.get(3)?,
                titles: titles.lines().map(str::to_string).collect(),
                version: r.get(5)?,
                verdict: r.get(6)?,
                note: r.get(7)?,
            })
        })?;
        rows.collect()
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
        if removed > 0 {
            // Hand deleted pages back to the OS so the file really shrinks.
            // Each step frees a page, so the rows have to be consumed.
            let mut st = self.conn.prepare("PRAGMA incremental_vacuum")?;
            let mut rows = st.query([])?;
            while rows.next()?.is_some() {}
        }
        Ok(removed)
    }

    /// Checkpoints the WAL into the main file. Call when idle or shutting down.
    pub fn compact(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
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
            sensors: Sensors {
                on_ac: Some(false),
                battery_pct: Some(61),
                battery_saver: Some(true),
                temp_c: Some(71.5),
                freq_pct: Some(63.0),
                gpu_pct: Some(88.5),
                gpu_temp_c: Some(79.0),
                gpu_throttle: Some(3),
                disk_queue: Some(1.25),
                disk_latency_ms: Some(12.5),
                page_out: Some(1200.0),
            },
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

    #[test]
    fn why_log_roundtrip_and_verdict_goes_to_the_latest() {
        let st = Store::open_in_memory().unwrap();
        assert_eq!(st.set_verdict("right", None).unwrap(), None);
        let a = st.log_why(100, 40, 100, &["A was slow".into(), "B too".into()], "0.4.0").unwrap();
        let b = st.log_why(200, 140, 200, &[], "0.4.0").unwrap();
        assert!(b > a);
        let got = st.set_verdict("wrong", Some("it was the browser")).unwrap().unwrap();
        assert_eq!((got.id, got.verdict.as_deref(), got.note.as_deref()), (b, Some("wrong"), Some("it was the browser")));
        let all = st.why_log().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].titles, vec!["A was slow".to_string(), "B too".to_string()]);
        assert_eq!(all[0].verdict, None);
        assert!(all[1].titles.is_empty());
    }

    #[test]
    fn missing_sensors_stay_none() {
        let mut s = Store::open_in_memory().unwrap();
        let mut smp = sample(5);
        smp.sensors = Sensors::default();
        s.insert(&smp).unwrap();
        let got = s.window(5, 5).unwrap().remove(0);
        assert_eq!(got.sensors, Sensors::default());
    }

    #[test]
    fn upgrades_a_v0_database_without_losing_data() {
        let dir = std::env::temp_dir().join(format!("bb-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        {
            // The schema shipped before sensors existed.
            let c = Connection::open(&dir).unwrap();
            c.execute_batch(
                "CREATE TABLE samples (ts INTEGER PRIMARY KEY, cpu INTEGER NOT NULL, mem_used INTEGER NOT NULL,
                    mem_total INTEGER NOT NULL, swap_used INTEGER NOT NULL, mhz INTEGER NOT NULL, disk INTEGER NOT NULL);
                 INSERT INTO samples VALUES (100, 555, 4096, 16384, 0, 3000, 2048);",
            )
            .unwrap();
        }
        let mut s = Store::open(&dir).unwrap();
        let old = s.window(100, 100).unwrap().remove(0);
        assert_eq!(old.cpu_pct, 55.5);
        assert_eq!(old.sensors, Sensors::default());
        s.insert(&sample(101)).unwrap();
        assert_eq!(s.window(101, 101).unwrap()[0], sample(101));
        drop(s);
        // Opening again must not try to re-add columns.
        assert_eq!(Store::open(&dir).unwrap().stats().unwrap().samples, 2);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", dir.display(), ext));
        }
    }

    /// Measures the on-disk footprint with a full day of worst-case synthetic data.
    /// Run: cargo test --release -p bb-core size_of -- --ignored --nocapture
    #[test]
    #[ignore]
    fn size_of_a_day_of_data() {
        let path = std::env::temp_dir().join(format!("bb-size-{}.db", std::process::id()));
        let size = |p: &Path| -> u64 {
            ["", "-wal", "-shm"].iter().filter_map(|e| std::fs::metadata(format!("{}{e}", p.display())).ok()).map(|m| m.len()).sum()
        };
        let mut st = Store::open(&path).unwrap();
        let now = 100 * 86400;
        let day: Vec<Sample> = (0..86400)
            .map(|i| {
                let mut s = sample(now - 86400 + i);
                // 24 distinct program rows with moving numbers: the worst case.
                s.procs = (0..24)
                    .map(|p| ProcRow {
                        name: format!("program{p}.exe"),
                        count: 1 + (i % 7) as u32,
                        cpu_pct: ((i + p) % 90) as f32 / 3.0,
                        disk_bps: ((i * 7 + p) % 5000) as u64 * 1024,
                        mem_bytes: (100 + (i + p) % 900) as u64 * 1024 * 1024,
                    })
                    .collect();
                s.sensors.temp_c = Some(50.0 + (i % 400) as f32 / 10.0);
                s.sensors.gpu_pct = Some((i % 100) as f32);
                s
            })
            .collect();
        for chunk in day.chunks(5000) {
            st.insert_many(chunk).unwrap();
        }
        st.compact().unwrap();
        let full = size(&path) as f64 / 1e6;
        println!("one day at 1 s resolution: {full:.1} MB");
        // Age that day by 24 h so it gets thinned.
        st.maintain(now + 86400 + 60, 7).unwrap();
        st.compact().unwrap();
        let thinned = size(&path) as f64 / 1e6;
        println!("same day after thinning to 1 per {THIN_STEP_SECS} s: {thinned:.1} MB");
        assert!(thinned < 0.3 * full, "file did not shrink after thinning");
        drop(st);
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }
}



