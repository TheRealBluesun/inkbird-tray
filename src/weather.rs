//! Outdoor temperature from Open-Meteo (no API key), stored alongside the
//! sensor readings so the history window can overlay it.
//!
//! Enabled only when INKBIRD_TRAY_LATLON="lat,lon" is set, so no location
//! lives in the repo. Hourly `temperature_2m`; the first fetch after start
//! backfills ~3 months, later fetches refresh the last two days (the model
//! revises recent hours).

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::sync::watch;

use crate::db;

const POLL: Duration = Duration::from_secs(30 * 60);
const RETRY: Duration = Duration::from_secs(5 * 60);
const BACKFILL_DAYS: u32 = 92;
const REFRESH_DAYS: u32 = 2;

pub fn location() -> Option<(f64, f64)> {
    let v = std::env::var("INKBIRD_TRAY_LATLON").ok()?;
    let (lat, lon) = v.split_once(',')?;
    let (lat, lon) = (lat.trim().parse().ok()?, lon.trim().parse().ok()?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

pub async fn run(mut shutdown_rx: watch::Receiver<bool>) {
    let Some((lat, lon)) = location() else {
        tracing::info!("outdoor temperature off (set INKBIRD_TRAY_LATLON=lat,lon to enable)");
        return;
    };
    tracing::info!("outdoor temperature from Open-Meteo every {} min", POLL.as_secs() / 60);

    let mut past_days = BACKFILL_DAYS;
    loop {
        let res = tokio::task::spawn_blocking(move || fetch_and_store(lat, lon, past_days)).await;
        let wait = match res {
            Ok(Ok(n)) => {
                tracing::debug!("stored {n} outdoor samples ({past_days} d)");
                past_days = REFRESH_DAYS;
                POLL
            }
            Ok(Err(e)) => {
                tracing::warn!("outdoor temperature fetch failed: {e:#}");
                RETRY
            }
            Err(e) => {
                tracing::warn!("outdoor temperature task: {e}");
                RETRY
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown_rx.changed() => {}
        }
        if *shutdown_rx.borrow() {
            return;
        }
    }
}

fn fetch_and_store(lat: f64, lon: f64, past_days: u32) -> Result<usize> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}\
         &hourly=temperature_2m&past_days={past_days}&forecast_days=1&timeformat=unixtime"
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let body: serde_json::Value = agent
        .get(&url)
        .call()
        .context("GET open-meteo")?
        .body_mut()
        .read_json()
        .context("decode open-meteo JSON")?;

    let hourly = &body["hourly"];
    let times = hourly["time"].as_array().ok_or_else(|| anyhow!("no hourly.time"))?;
    let temps = hourly["temperature_2m"]
        .as_array()
        .ok_or_else(|| anyhow!("no hourly.temperature_2m"))?;
    let now = chrono::Utc::now().timestamp();
    // Past hours only: the forecast tail would read as a measurement.
    let samples: Vec<(i64, f64)> = times
        .iter()
        .zip(temps)
        .filter_map(|(t, v)| Some((t.as_i64()?, v.as_f64()?)))
        .filter(|&(t, _)| t <= now)
        .collect();
    db::upsert_outdoor(&samples)
}
