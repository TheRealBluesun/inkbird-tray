//! BLE scan / connect / notify / reconnect loop.
//!
//! Rediscover by advertisement fingerprint on every attempt. Never cache the
//! sensor's random-static MAC. Never poll characteristics.
//!
//! Adapter D-Bus paths are *not* stable: a USB dongle unplug/replug moves the
//! working radio from hci0 to hci2 and leaves `/org/bluez/hci0` as a dead
//! object. Re-open the BlueZ session every cycle and pick a live adapter.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bluer::{
    Adapter, AdapterEvent, Address, DiscoveryFilter, DiscoveryTransport, ErrorKind, Session,
};
use chrono::Local;
use futures::{pin_mut, StreamExt};
use tokio::sync::watch;
use tokio::time::{timeout, Instant};

use crate::{csvlog, db};
use crate::protocol::{
    hex_lower, infer_unit, mfg_matches, name_matches, parse_packet, to_celsius, NotifyBuf, Packet,
    TempUnit, NOTIFY_UUID, SERVICE_UUID,
};
use crate::state::{AppState, ConnectionStatus, Reading};

const SCAN_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SERVICES_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_START: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const DEAD_ADAPTER_BACKOFF: Duration = Duration::from_secs(2);

struct LiveAdapter {
    adapter: Adapter,
    label: String,
}

pub async fn run(
    state_tx: watch::Sender<AppState>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let mut backoff = BACKOFF_START;
    let mut preferred_mac: Option<Address> = None;
    let mut unit_from_state: Option<TempUnit> = None;
    let mut last_logged_adapter = String::new();

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        publish(&state_tx, ConnectionStatus::Scanning, None);

        let adapters = match open_adapters(preferred_mac).await {
            Ok(a) => a,
            Err(err) => {
                tracing::warn!("adapter init failed: {err:#}");
                publish(
                    &state_tx,
                    ConnectionStatus::Error {
                        detail: compact_error(&err),
                    },
                    None,
                );
                if wait_backoff(&mut shutdown_rx, backoff).await {
                    break;
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };

        let mut outcome: Option<Result<CycleOk>> = None;
        let mut used_mac: Option<Address> = None;
        for live in &adapters {
            if live.label != last_logged_adapter {
                tracing::info!("using adapter {}", live.label);
                last_logged_adapter = live.label.clone();
            }
            match run_once(
                &live.adapter,
                &state_tx,
                &mut shutdown_rx,
                &mut unit_from_state,
            )
            .await
            {
                Ok(cycle) => {
                    used_mac = live.adapter.address().await.ok();
                    outcome = Some(Ok(cycle));
                    break;
                }
                Err(err) if try_next_adapter(&err) => {
                    tracing::info!("{}: {err:#}; trying next adapter", live.label);
                    outcome = Some(Err(err));
                }
                Err(err) => {
                    outcome = Some(Err(err));
                    break;
                }
            }
        }

        match outcome {
            Some(Ok(CycleOk::Data)) => {
                if used_mac.is_some() {
                    preferred_mac = used_mac;
                }
                backoff = BACKOFF_START;
            }
            Some(Ok(CycleOk::Stop)) => break,
            Some(Err(err)) => {
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
                if is_dead_adapter(&err) {
                    last_logged_adapter.clear();
                    backoff = DEAD_ADAPTER_BACKOFF;
                }
            }
            None => {
                tracing::warn!("BLE cycle failed: no usable Bluetooth adapter");
                publish(
                    &state_tx,
                    ConnectionStatus::Error {
                        detail: "no usable Bluetooth adapter".into(),
                    },
                    None,
                );
            }
        }

        if *shutdown_rx.borrow() {
            break;
        }

        tracing::info!("retrying in {} s", backoff.as_secs());
        if wait_backoff(&mut shutdown_rx, backoff).await {
            break;
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }

    tracing::info!("BLE task stopping");
    Ok(())
}

/// Re-enumerate adapters every cycle. USB BT dongles change hciN on replug,
/// and bluer's `default_adapter()` hard-picks `hci0` whenever that name exists.
async fn open_adapters(preferred_mac: Option<Address>) -> Result<Vec<LiveAdapter>> {
    let session = Session::new()
        .await
        .context("connect to bluetoothd over D-Bus")?;
    let names = session
        .adapter_names()
        .await
        .context("list Bluetooth adapters")?;
    if names.is_empty() {
        return Err(anyhow!("no Bluetooth adapters present"));
    }

    let ordered = order_adapter_names(&session, names, preferred_mac).await;
    let mut out = Vec::new();
    for name in ordered {
        let adapter = match session.adapter(&name) {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!("open {name}: {e}");
                continue;
            }
        };
        if let Err(e) = adapter.set_powered(true).await {
            tracing::warn!("power on {name}: {e}");
            continue;
        }
        let addr = adapter
            .address()
            .await
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into());
        out.push(LiveAdapter {
            adapter,
            label: format!("{name} ({addr})"),
        });
    }
    if out.is_empty() {
        return Err(anyhow!("no powered Bluetooth adapters available"));
    }
    Ok(out)
}

async fn order_adapter_names(
    session: &Session,
    names: Vec<String>,
    preferred_mac: Option<Address>,
) -> Vec<String> {
    let wanted = std::env::var("INKBIRD_TRAY_ADAPTER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut scored: Vec<(u8, String)> = Vec::new();
    for name in names {
        let adapter = match session.adapter(&name) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let mac = adapter.address().await.ok();
        let mut score = 3u8;
        if let Some(ref wanted) = wanted {
            if name.eq_ignore_ascii_case(wanted) {
                score = 0;
            } else if let (Ok(want_mac), Some(mac)) = (wanted.parse::<Address>(), mac) {
                if want_mac == mac {
                    score = 0;
                }
            }
        } else if let (Some(pref), Some(mac)) = (preferred_mac, mac) {
            if pref == mac {
                score = 1;
            }
        }
        if score > 1 && adapter_has_cached_iam(&adapter).await {
            score = 2;
        }
        scored.push((score, name));
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, n)| n).collect()
}

async fn adapter_has_cached_iam(adapter: &Adapter) -> bool {
    let Ok(addrs) = adapter.device_addresses().await else {
        return false;
    };
    for addr in addrs {
        let Ok(dev) = adapter.device(addr) else {
            continue;
        };
        if dev
            .name()
            .await
            .ok()
            .flatten()
            .as_deref()
            .is_some_and(name_matches)
        {
            return true;
        }
    }
    false
}

fn try_next_adapter(err: &anyhow::Error) -> bool {
    is_dead_adapter(err) || err.to_string().contains("sensor not found during scan")
}

fn is_dead_adapter(err: &anyhow::Error) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    s.contains("not present or removed")
        || s.contains("unknown object")
        || s.contains("resource not ready")
        || s.contains("no such adapter")
        || s.contains("does not exist")
}

/// Returns true if shutdown was requested during the wait.
async fn wait_backoff(shutdown_rx: &mut watch::Receiver<bool>, backoff: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(backoff) => false,
        _ = shutdown_rx.changed() => *shutdown_rx.borrow(),
    }
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
    let mut notify_buf = NotifyBuf::default();

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
                        for frame in notify_buf.feed(&payload) {
                            handle_payload(
                                &frame,
                                &address,
                                state_tx,
                                unit_from_state,
                                &mut saw_data,
                            );
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
                tracing::warn!("failed to persist reading to CSV: {e:#}");
            }
            if let Err(e) = db::append(&reading) {
                tracing::warn!("failed to persist reading to SQLite: {e:#}");
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
