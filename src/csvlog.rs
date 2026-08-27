//! Append-only CSV log at ~/.local/share/inkbird-tray/readings.csv

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::state::Reading;

const HEADER: &str = "timestamp,co2_ppm,temp,unit,temp_c,humidity_pct,pressure_hpa\n";

pub fn default_path() -> Result<PathBuf> {
    let dir = dirs::data_local_dir()
        .context("cannot resolve XDG data directory")?
        .join("inkbird-tray");
    Ok(dir.join("readings.csv"))
}

pub fn append(reading: &Reading) -> Result<PathBuf> {
    let path = default_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    let mut file = opts
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;

    if file.metadata().map(|m| m.len()).unwrap_or(1) == 0 {
        file.write_all(HEADER.as_bytes())
            .with_context(|| format!("write header {}", path.display()))?;
    }

    let line = format!(
        "{},{},{:.1},{},{:.2},{:.1},{}\n",
        reading.timestamp.format("%Y-%m-%dT%H:%M:%S"),
        reading.co2_ppm,
        reading.temp,
        reading.unit.as_str(),
        reading.temp_c,
        reading.humidity_pct,
        reading.pressure_hpa,
    );
    file.write_all(line.as_bytes())
        .with_context(|| format!("append {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush {}", path.display()))?;
    Ok(path)
}
