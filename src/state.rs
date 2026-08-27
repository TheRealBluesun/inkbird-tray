//! Shared daemon state published to the tray over a watch channel.

use chrono::{DateTime, Local};

use crate::protocol::TempUnit;

/// Connection / lifecycle phase shown in the tray.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Scanning,
    Connecting { address: String },
    ConnectedWaiting { address: String },
    Live { address: String },
    DeviceBusy { detail: String },
    Unreachable { detail: String },
    Error { detail: String },
}

impl ConnectionStatus {
    pub fn short_label(&self) -> String {
        match self {
            ConnectionStatus::Scanning => "scanning".into(),
            ConnectionStatus::Connecting { .. } => "connecting".into(),
            ConnectionStatus::ConnectedWaiting { .. } => "waiting for data".into(),
            ConnectionStatus::Live { .. } => "live".into(),
            ConnectionStatus::DeviceBusy { .. } => "device busy".into(),
            ConnectionStatus::Unreachable { .. } => "not found".into(),
            ConnectionStatus::Error { .. } => "error".into(),
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            ConnectionStatus::Connecting { address }
            | ConnectionStatus::ConnectedWaiting { address }
            | ConnectionStatus::Live { address } => Some(address.as_str()),
            ConnectionStatus::DeviceBusy { detail }
            | ConnectionStatus::Unreachable { detail }
            | ConnectionStatus::Error { detail } => Some(detail.as_str()),
            ConnectionStatus::Scanning => None,
        }
    }
}

/// One persisted / displayed reading.
#[derive(Debug, Clone)]
pub struct Reading {
    pub timestamp: DateTime<Local>,
    pub co2_ppm: u16,
    pub temp: f32,
    pub unit: TempUnit,
    pub unit_inferred: bool,
    pub temp_c: f32,
    pub humidity_pct: f32,
    pub pressure_hpa: u16,
}

impl Reading {
    pub fn format_lines(&self) -> String {
        let inferred = if self.unit_inferred {
            " (unit inferred)"
        } else {
            ""
        };
        format!(
            "CO2 {} ppm\n{:.1} °{} / {:.1} °C{}\nRH {:.1} %\n{} hPa\n{}",
            self.co2_ppm,
            self.temp,
            self.unit.as_str(),
            self.temp_c,
            inferred,
            self.humidity_pct,
            self.pressure_hpa,
            format_age(self.timestamp),
        )
    }

    pub fn format_one_line(&self) -> String {
        format!(
            "CO2 {} ppm | {:.1} °{} / {:.1} °C | RH {:.1} % | {} hPa | {}",
            self.co2_ppm,
            self.temp,
            self.unit.as_str(),
            self.temp_c,
            self.humidity_pct,
            self.pressure_hpa,
            format_age(self.timestamp),
        )
    }
}

/// Snapshot consumed by the tray.
#[derive(Debug, Clone)]
pub struct AppState {
    pub status: ConnectionStatus,
    pub last_reading: Option<Reading>,
    /// Interval between the last two DATA packets, as observed by the BLE
    /// task. Lets the tray judge "stale" against the device's own rhythm
    /// instead of a fixed window.
    pub observed_interval_secs: Option<i64>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            status: ConnectionStatus::Scanning,
            last_reading: None,
            observed_interval_secs: None,
        }
    }
}

/// Stale window = the floor, widened to "missed ~two packets" once the
/// device's sampling rhythm is known.
pub fn stale_window(floor_secs: i64, observed_interval: Option<i64>) -> i64 {
    floor_secs.max(observed_interval.map(|i| 2 * i + 30).unwrap_or(0))
}

pub fn format_age(ts: DateTime<Local>) -> String {
    let secs = (Local::now() - ts).num_seconds().max(0);
    if secs < 10 {
        "just now".into()
    } else if secs < 60 {
        format!("{secs} s ago")
    } else {
        let mins = secs / 60;
        if mins == 1 {
            "1 min ago".into()
        } else {
            format!("{mins} min ago")
        }
    }
}
