//! BLE scan / connect / notify / reconnect loop.
//!
//! Rediscover by advertisement fingerprint on every attempt. Never cache the
//! random-static MAC. Never poll characteristics.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bluer::{Adapter, AdapterEvent, DiscoveryFilter, DiscoveryTransport, ErrorKind, Session};
use chrono::Local;
use futures::{pin_mut, StreamExt};
use tokio::sync::watch;
use tokio::time::{timeout, Instant};

use crate::csvlog;
use crate::protocol::{
    hex_lower, infer_unit, mfg_matches, name_matches, parse_packet, to_celsius, Packet, TempUnit,
    NOTIFY_UUID, SERVICE_UUID,
};
use crate::state::{AppState, ConnectionStatus, Reading};

const SCAN_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SERVICES_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_START: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

pub async fn run(
    state_tx: watch::Sender<AppState>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let session = Session::new()
        .await
        .context("connect to bluetoothd over D-Bus")?;
    let adapter = session
        .default_adapter()
        .await
        .context("open BlueZ default adapter (hci0 if present)")?;
    adapter
        .set_powered(true)
        .await
        .context("power on default adapter")?;

    let addr = adapter
        .address()
        .await
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    tracing::info!("using BlueZ default adapter {} ({})", adapter.name(), addr);

    let mut backoff = BACKOFF_START;
    let mut unit_from_state: Option<TempUnit> = None;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        publish(&state_tx, ConnectionStatus::Scanning, None);

        match run_once(&adapter, &state_tx, &mut shutdown_rx, &mut unit_from_state).await {
            Ok(CycleOk::Data) => {
                backoff = BACKOFF_START;
            }
            Ok(CycleOk::Stop) => break,
            Err(err) => {
                tracing::warn!("BLE cycle failed: {err:#}");
                if !matches_busy_or_status(&state_tx) {
                    publish(
                        &state_tx,
                        ConnectionStatus::Error {
                            detail: compact_error(&err),
                        },
                        None,
                    );
                }
            }
        }

        if *shutdown_rx.borrow() {
            break;
        }

        tracing::info!("retrying in {} s", backoff.as_secs());
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }

    tracing::info!("BLE task stopping");
    Ok(())
}

async fn run_once(
    adapter: &Adapter,
    state_tx: &watch::Sender<AppState>,
    shutdown_rx: &mut watch::Receiver<bool>,
    unit_from_state: &mut Option<TempUnit>,
) -> Result<CycleOk> {
    let device = match find_device(adapter, shutdown_rx).await? {
        FindResult::Shutdown => return Ok(CycleOk::Stop),
        FindResult::Miss => {
            publish(
                state_tx,
                ConnectionStatus::Unreachable {
                    detail: "IAM-T1 not advertising (BLE switch under the battery cap? phone app holding the link?)"
                        .into(),
                },
                None,
            );
            return Err(anyhow!("sensor not found during scan"));
        }
        FindResult::Hit(d) => d,
    };

    let address = device.address().to_string();
    publish(
        state_tx,
        ConnectionStatus::Connecting {
            address: address.clone(),
        },
        None,
    );
    tracing::info!("found IAM-T1 at {address}, connecting");

    match timeout(CONNECT_TIMEOUT, ensure_connected(&device)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let detail = classify_connect_error(&err);
            publish(
                state_tx,
                ConnectionStatus::DeviceBusy {
                    detail: detail.clone(),
                },
                None,
            );
            let _ = device.disconnect().await;
            let _ = adapter.remove_device(device.address()).await;
            return Err(anyhow!(detail));
        }
        Err(_) => {
            let detail = format!(
                "connect timed out after {}s — another central (phone app) is probably holding the only slot",
                CONNECT_TIMEOUT.as_secs()
            );
            publish(
                state_tx,
                ConnectionStatus::DeviceBusy {
                    detail: detail.clone(),
                },
                None,
            );
            let _ = device.disconnect().await;
            let _ = adapter.remove_device(device.address()).await;
            return Err(anyhow!(detail));
        }
    }

    publish(
        state_tx,
        ConnectionStatus::ConnectedWaiting {
            address: address.clone(),
        },
        None,
    );
    tracing::info!("connected to {address}, waiting for GATT services");

    if let Err(err) = wait_services_resolved(&device).await {
        let _ = device.disconnect().await;
        let _ = adapter.remove_device(device.address()).await;
        return Err(err);
    }

    let notify_char = match find_notify_char(&device).await {
        Ok(c) => c,
        Err(err) => {
            let _ = device.disconnect().await;
            let _ = adapter.remove_device(device.address()).await;
            return Err(err);
        }
    };

    tracing::info!("subscribing to notifications on {NOTIFY_UUID}");
    let notifications = match notify_char.notify().await {
        Ok(s) => s,
        Err(err) => {
            let _ = device.disconnect().await;
            let _ = adapter.remove_device(device.address()).await;
            return Err(anyhow!("subscribe failed: {err}"));
        }
    };
    pin_mut!(notifications);
    tracing::info!("subscribed; waiting for packets (device interval, do not poll)");

    let mut saw_data = false;

    loop {
        if *shutdown_rx.borrow() {
            let _ = device.disconnect().await;
            return Ok(if saw_data {
                CycleOk::Data
            } else {
                CycleOk::Stop
            });
        }

        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!("shutdown: disconnecting {address}");
                    let _ = device.disconnect().await;
                    return Ok(if saw_data { CycleOk::Data } else { CycleOk::Stop });
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(3)) => {
                match device.is_connected().await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!("device {address} disconnected");
                        let _ = adapter.remove_device(device.address()).await;
                        return if saw_data {
                            Ok(CycleOk::Data)
                        } else {
                            Err(anyhow!("disconnected before first data packet"))
                        };
                    }
                    Err(e) => {
                        tracing::warn!("is_connected check failed: {e}");
                    }
                }
            }
            item = notifications.next() => {
                match item {
                    None => {
                        tracing::warn!("notification stream ended");
                        let _ = device.disconnect().await;
                        let _ = adapter.remove_device(device.address()).await;
                        return if saw_data {
                            Ok(CycleOk::Data)
                        } else {
                            Err(anyhow!("notification stream ended before first data packet"))
                        };
                    }
                    Some(payload) => {
                        if handle_payload(
                            &payload,
                            &address,
                            state_tx,
                            unit_from_state,
                            &mut saw_data,
                        ) {
                            // data persisted; keep listening
                        }
                    }
                }
            }
        }
    }
}

enum CycleOk {
    Data,
    Stop,
}

enum FindResult {
    Hit(bluer::Device),
    Miss,
    Shutdown,
}

async fn find_device(
    adapter: &Adapter,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<FindResult> {
    if *shutdown_rx.borrow() {
        return Ok(FindResult::Shutdown);
    }

    if let Err(err) = adapter
        .set_discovery_filter(DiscoveryFilter {
            transport: DiscoveryTransport::Le,
            duplicate_data: true,
            ..Default::default()
        })
        .await
    {
        // Another discovery session may be holding the filter. Continue anyway.
        tracing::debug!("set_discovery_filter: {err}");
    }

    tracing::info!("scanning for IAM-T1 (name 'iam-t1' or mfg 0x3154/AC-6200)");
    let discover = adapter
        .discover_devices_with_changes()
        .await
        .context("start LE discovery")?;
    pin_mut!(discover);

    let deadline = Instant::now() + SCAN_TIMEOUT;
    loop {
        if *shutdown_rx.borrow() {
            return Ok(FindResult::Shutdown);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(FindResult::Miss);
        }

        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(FindResult::Shutdown);
                }
            }
            _ = tokio::time::sleep(remaining) => {
                return Ok(FindResult::Miss);
            }
            evt = discover.next() => {
                match evt {
                    Some(AdapterEvent::DeviceAdded(addr)) => {
                        let device = match adapter.device(addr) {
                            Ok(d) => d,
                            Err(e) => {
                                tracing::debug!("adapter.device({addr}): {e}");
                                continue;
                            }
                        };
                        match device_matches(&device).await {
                            Ok(true) => {
                                tracing::info!(
                                    "advertisement fingerprint matched {}",
                                    device.address()
                                );
                                return Ok(FindResult::Hit(device));
                            }
                            Ok(false) => {}
                            Err(e) => tracing::debug!("fingerprint {addr}: {e}"),
                        }
                    }
                    Some(_) => {}
                    None => return Ok(FindResult::Miss),
                }
            }
        }
    }
}

async fn device_matches(device: &bluer::Device) -> Result<bool> {
    // Skip stale BlueZ cache entries that are not currently advertising
    // and not already connected to us.
    let connected = device.is_connected().await.unwrap_or(false);
    let rssi = device.rssi().await.ok().flatten();
    if !connected && rssi.is_none() {
        return Ok(false);
    }

    let name = device.name().await.ok().flatten();
    if name.as_deref().is_some_and(name_matches) {
        return Ok(true);
    }
    if let Ok(Some(mfg)) = device.manufacturer_data().await {
        if mfg_matches(&mfg) {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn ensure_connected(device: &bluer::Device) -> Result<()> {
    match device.is_connected().await {
        Ok(true) => {
            tracing::info!("already connected");
            return Ok(());
        }
        Ok(false) => {}
        Err(e) => tracing::debug!("is_connected: {e}"),
    }

    match device.connect().await {
        Ok(()) => Ok(()),
        Err(err) if err.kind == ErrorKind::AlreadyConnected => Ok(()),
        Err(err) => Err(anyhow!("connect: {err}")),
    }
}

fn classify_connect_error(err: &anyhow::Error) -> String {
    let msg = err.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("already connected") {
        return msg;
    }
    format!(
        "cannot connect ({msg}) — the IAM-T1 accepts only one central; close the phone app if it is open"
    )
}

async fn wait_services_resolved(device: &bluer::Device) -> Result<()> {
    let start = Instant::now();
    loop {
        match device.is_services_resolved().await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => tracing::debug!("is_services_resolved: {e}"),
        }
        if start.elapsed() > SERVICES_TIMEOUT {
            // Fall through and try enumerating anyway; some BlueZ versions
            // never flip the flag but still expose services.
            tracing::warn!(
                "services-resolved flag still false after {:?}; enumerating anyway",
                SERVICES_TIMEOUT
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn find_notify_char(device: &bluer::Device) -> Result<bluer::gatt::remote::Characteristic> {
    let services = device.services().await.context("enumerate GATT services")?;
    for service in services {
        let uuid = match service.uuid().await {
            Ok(u) => u,
            Err(e) => {
                tracing::debug!("service uuid: {e}");
                continue;
            }
        };
        if uuid != SERVICE_UUID {
            continue;
        }
        let chars = service
            .characteristics()
            .await
            .context("enumerate characteristics")?;
        for ch in chars {
            match ch.uuid().await {
                Ok(u) if u == NOTIFY_UUID => return Ok(ch),
                Ok(_) => {}
                Err(e) => tracing::debug!("characteristic uuid: {e}"),
            }
        }
        return Err(anyhow!(
            "service {SERVICE_UUID} found but notify characteristic {NOTIFY_UUID} missing"
        ));
    }
    Err(anyhow!(
        "GATT service {SERVICE_UUID} not found on the device"
    ))
}

/// Returns true if a DATA packet was accepted.
fn handle_payload(
    payload: &[u8],
    address: &str,
    state_tx: &watch::Sender<AppState>,
    unit_from_state: &mut Option<TempUnit>,
    saw_data: &mut bool,
) -> bool {
    match parse_packet(payload) {
        Packet::State { fahrenheit } => {
            let unit = if fahrenheit {
                TempUnit::Fahrenheit
            } else {
                TempUnit::Celsius
            };
            *unit_from_state = Some(unit);
            tracing::info!(
                "state packet: temperature unit = {} ({})",
                unit.as_str(),
                hex_lower(payload)
            );
            false
        }
        Packet::Data(raw) => {
            let inferred = unit_from_state.is_none();
            let unit = unit_from_state.unwrap_or_else(|| infer_unit(raw.temp));
            let temp_c = to_celsius(raw.temp, unit);
            let reading = Reading {
                timestamp: Local::now(),
                co2_ppm: raw.co2_ppm,
                temp: raw.temp,
                unit,
                unit_inferred: inferred,
                temp_c,
                humidity_pct: raw.humidity_pct,
                pressure_hpa: raw.pressure_hpa,
            };
            let suffix = if inferred {
                " (unit inferred from magnitude)"
            } else {
                ""
            };
            tracing::info!(
                "DATA {} | CO2 {} ppm | {:.1} °{} | {:.1} °C | RH {:.1} % | {} hPa{}",
                hex_lower(payload),
                reading.co2_ppm,
                reading.temp,
                reading.unit.as_str(),
                reading.temp_c,
                reading.humidity_pct,
                reading.pressure_hpa,
                suffix
            );
            if let Err(e) = csvlog::append(&reading) {
                tracing::warn!("failed to persist reading: {e:#}");
            }
            *saw_data = true;
            let prev = state_tx.borrow().last_reading.clone();
            state_tx.send_modify(|s| {
                s.status = ConnectionStatus::Live {
                    address: address.to_string(),
                };
                if let Some(p) = prev {
                    let gap = (reading.timestamp - p.timestamp).num_seconds();
                    if gap > 0 {
                        s.observed_interval_secs = Some(gap);
                    }
                }
                s.last_reading = Some(reading);
            });
            true
        }
        Packet::Implausible => {
            tracing::warn!("dropped implausible packet: {}", hex_lower(payload));
            false
        }
        Packet::Unknown => {
            tracing::info!(
                "unknown packet ({} B): {}",
                payload.len(),
                hex_lower(payload)
            );
            false
        }
    }
}

fn publish(state_tx: &watch::Sender<AppState>, status: ConnectionStatus, reading: Option<Reading>) {
    state_tx.send_modify(|s| {
        s.status = status;
        if let Some(r) = reading {
            s.last_reading = Some(r);
        }
    });
}

fn compact_error(err: &anyhow::Error) -> String {
    let s = format!("{err:#}");
    if s.len() > 160 {
        format!("{}…", &s[..157])
    } else {
        s
    }
}

fn matches_busy_or_status(state_tx: &watch::Sender<AppState>) -> bool {
    matches!(
        state_tx.borrow().status,
        ConnectionStatus::DeviceBusy { .. } | ConnectionStatus::Unreachable { .. }
    )
}
