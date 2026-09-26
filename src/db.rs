//! SQLite store at ~/.local/share/inkbird-tray/readings.db
//!
//! WAL mode, so the history viewer (a separate process) can read while the
//! daemon writes. `ts` is Unix seconds and the rowid, which makes time-range
//! scans cheap and `INSERT OR IGNORE` idempotent for the one-time CSV import.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime, TimeZone};
use rusqlite::{params, Connection, OpenFlags};

use crate::state::Reading;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS readings (
    ts           INTEGER PRIMARY KEY,
    co2_ppm      INTEGER NOT NULL,
    temp         REAL    NOT NULL,
    unit         TEXT    NOT NULL,
    temp_c       REAL    NOT NULL,
    humidity_pct REAL    NOT NULL,
    pressure_hpa INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS outdoor (
    ts     INTEGER PRIMARY KEY,
    temp_c REAL    NOT NULL
);
";

/// One row as the viewer consumes it.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub ts: i64,
    pub co2_ppm: f64,
    pub fahrenheit: bool,
    pub temp_c: f64,
    pub humidity_pct: f64,
    pub pressure_hpa: f64,
}

pub fn default_path() -> Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .context("cannot resolve XDG data directory")?
        .join("inkbird-tray");
    Ok(dir.join("readings.db"))
}

/// Open (creating if needed) the database for writing.
pub fn open() -> Result<Connection> {
    let path = default_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let conn = Connection::open(&path).with_context(|| format!("open {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.execute_batch(SCHEMA).context("create schema")?;
    // Older builds inferred °C for anything <= 65, so a 64.9 °F room was
    // logged as 64.9 °C. Relabel those rows; idempotent.
    let fixed = conn.execute(
        "UPDATE readings SET unit = 'F', temp_c = round((temp - 32) * 5.0 / 9.0, 2)
         WHERE unit = 'C' AND temp > ?1",
        [crate::protocol::INFER_F_ABOVE as f64],
    )?;
    if fixed > 0 {
        tracing::info!("relabeled {fixed} mis-inferred °C rows as °F");
    }
    Ok(conn)
}

/// Open read-only for the viewer. Fails if the daemon has never created it.
pub fn open_readonly() -> Result<Connection> {
    let path = default_path()?;
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    Ok(conn)
}

static WRITER: Mutex<Option<Connection>> = Mutex::new(None);

/// Persist one reading. The connection is opened lazily and dropped on
/// error so the next reading retries from scratch.
pub fn append(reading: &Reading) -> Result<()> {
    let mut guard = WRITER.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(open()?);
    }
    let conn = guard.as_ref().expect("opened above");
    let res = conn.execute(
        "INSERT OR REPLACE INTO readings
             (ts, co2_ppm, temp, unit, temp_c, humidity_pct, pressure_hpa)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            reading.timestamp.timestamp(),
            reading.co2_ppm,
            reading.temp as f64,
            reading.unit.as_str(),
            reading.temp_c as f64,
            reading.humidity_pct as f64,
            reading.pressure_hpa,
        ],
    );
    if let Err(e) = res {
        *guard = None;
        return Err(e).context("insert reading");
    }
    Ok(())
}

/// Insert or refresh outdoor temperature samples `(unix_ts, °C)`.
pub fn upsert_outdoor(samples: &[(i64, f64)]) -> Result<usize> {
    let mut guard = WRITER.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(open()?);
    }
    let conn = guard.as_mut().expect("opened above");
    let res = (|| -> rusqlite::Result<usize> {
        let tx = conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt =
                tx.prepare("INSERT OR REPLACE INTO outdoor (ts, temp_c) VALUES (?1, ?2)")?;
            for &(ts, t) in samples {
                n += stmt.execute(params![ts, t])?;
            }
        }
        tx.commit()?;
        Ok(n)
    })();
    if res.is_err() {
        *guard = None;
    }
    res.context("store outdoor temperature")
}

/// All outdoor samples, oldest first. Small (hourly), and recent hours get
/// revised, so the viewer reloads the whole table.
pub fn load_outdoor(conn: &Connection) -> Result<Vec<(i64, f64)>> {
    // Older databases opened read-only may predate the table.
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'outdoor')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare_cached("SELECT ts, temp_c FROM outdoor ORDER BY ts")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Seed an empty database from the legacy CSV log. No-op once any row exists.
pub fn import_csv_if_empty(conn: &mut Connection, csv_path: &Path) -> Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM readings", [], |r| r.get(0))?;
    if count > 0 || !csv_path.exists() {
        return Ok(0);
    }
    let text = std::fs::read_to_string(csv_path)
        .with_context(|| format!("read {}", csv_path.display()))?;

    let tx = conn.transaction()?;
    let mut imported = 0;
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO readings
                 (ts, co2_ppm, temp, unit, temp_c, humidity_pct, pressure_hpa)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split(',').collect();
            if f.len() != 7 {
                continue;
            }
            let Some(ts) = NaiveDateTime::parse_from_str(f[0], "%Y-%m-%dT%H:%M:%S")
                .ok()
                .and_then(|n| Local.from_local_datetime(&n).earliest())
            else {
                continue;
            };
            let (Ok(co2), Ok(temp), Ok(temp_c), Ok(rh), Ok(hpa)) = (
                f[1].parse::<i64>(),
                f[2].parse::<f64>(),
                f[4].parse::<f64>(),
                f[5].parse::<f64>(),
                f[6].parse::<i64>(),
            ) else {
                continue;
            };
            imported += stmt.execute(params![ts.timestamp(), co2, temp, f[3], temp_c, rh, hpa])?;
        }
    }
    tx.commit()?;
    Ok(imported)
}

/// Rows with `ts > after`, oldest first.
pub fn load_after(conn: &Connection, after: i64) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts, co2_ppm, unit, temp_c, humidity_pct, pressure_hpa
         FROM readings WHERE ts > ?1 ORDER BY ts",
    )?;
    let rows = stmt
        .query_map([after], |r| {
            Ok(Row {
                ts: r.get(0)?,
                co2_ppm: r.get::<_, i64>(1)? as f64,
                fahrenheit: r.get::<_, String>(2)? == "F",
                temp_c: r.get(3)?,
                humidity_pct: r.get(4)?,
                pressure_hpa: r.get::<_, i64>(5)? as f64,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
