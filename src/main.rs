//! InkBird IAM-T1 tray daemon.
//!
//! One tokio task owns BLE (scan → connect → notify → reconnect). Another
//! owns the StatusNotifierItem tray. They talk through a watch channel so the
//! tray never blocks the radio.

mod ble;
mod csvlog;
mod db;
mod pixmap;
mod protocol;
mod state;
mod tray;
mod viewer;

/// Floor for the stale window (seconds). A reading older than the effective
/// window (this floor, widened adaptively once the device rhythm is known —
/// see state::stale_window) renders the tray digits in grey.
/// Override with INKBIRD_TRAY_STALE_SECS (useful for testing the grey state).
pub fn stale_floor_secs() -> i64 {
    std::env::var("INKBIRD_TRAY_STALE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&s: &i64| s > 0)
        .unwrap_or(150)
}

use anyhow::Context;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

use crate::state::AppState;
use crate::tray::TrayAction;

fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--viewer") {
        return viewer::run();
    }
    daemon()
}

#[tokio::main]
async fn daemon() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("inkbird_tray=info,warn")),
        )
        .with_target(false)
        .init();

    let csv_path = csvlog::default_path()?;
    tracing::info!("readings CSV: {}", csv_path.display());
    match db::open().and_then(|mut conn| db::import_csv_if_empty(&mut conn, &csv_path)) {
        Ok(0) => {}
        Ok(n) => tracing::info!("seeded SQLite history with {n} rows from CSV"),
        Err(e) => tracing::warn!("SQLite history unavailable: {e:#}"),
    }
    if let Ok(p) = db::default_path() {
        tracing::info!("readings DB: {}", p.display());
    }

    let (state_tx, state_rx) = watch::channel(AppState::default());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let ble_shutdown = shutdown_rx.clone();
    let ble_handle = tokio::spawn(async move {
        if let Err(e) = ble::run(state_tx, ble_shutdown).await {
            tracing::error!("BLE task exited: {e:#}");
        }
    });

    let tray_shutdown = shutdown_rx.clone();
    let mut action_rx = tray::spawn_tray(state_rx, tray_shutdown).await;

    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;
    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;

    tracing::info!("inkbird-tray running; Ctrl-C or tray Quit to stop");

    loop {
        tokio::select! {
            _ = sigint.recv() => {
                tracing::info!("SIGINT");
                break;
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM");
                break;
            }
            action = async {
                match action_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match action {
                    Some(TrayAction::Quit) | None => {
                        tracing::info!("quit requested from tray");
                        break;
                    }
                }
            }
        }
    }

    let _ = shutdown_tx.send(true);
    match tokio::time::timeout(std::time::Duration::from_secs(5), ble_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("BLE task join: {e}"),
        Err(_) => tracing::warn!("BLE task did not stop within 5s"),
    }
    tracing::info!("stopped");
    Ok(())
}
