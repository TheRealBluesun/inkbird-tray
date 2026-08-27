//! StatusNotifierItem tray (ksni 0.3), modeled on voxtype's tray.rs.
//!
//! Icon names are freedesktop names present in Mint-X / hicolor. See README.

use std::io::Write;
use std::process::{Command, Stdio};

use ksni::TrayMethods;
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::state::{AppState, ConnectionStatus};

/// Menu actions that leave the tray process (Quit).
#[derive(Debug, Clone)]
pub enum TrayAction {
    Quit,
}

/// Freedesktop icon names used by this tray.
///
/// Symbolic variants only: Cinnamon's xapp-status renderer colors symbolic
/// icons to match the panel theme, while fullcolor Mint-X PNGs (e.g.
/// "weather-clear") render as dim grey silhouettes on a dark panel.
/// All names below exist in Mint-X (view-refresh falls back to Adwaita).
pub mod icons {
    pub const SCANNING: &str = "view-refresh-symbolic";
    pub const CONNECTING: &str = "bluetooth-active-symbolic";
    pub const WAITING: &str = "bluetooth-active-symbolic";
    pub const GOOD: &str = "weather-clear-symbolic";
    pub const MODERATE: &str = "weather-few-clouds-symbolic";
    pub const POOR: &str = "weather-storm-symbolic";
    pub const BUSY: &str = "dialog-warning-symbolic";
    pub const UNREACHABLE: &str = "network-offline-symbolic";
    pub const ERROR: &str = "dialog-error-symbolic";
}

struct InkBirdTray {
    app: AppState,
    action_tx: mpsc::UnboundedSender<TrayAction>,
}

impl InkBirdTray {
    fn icon(&self) -> &'static str {
        match &self.app.status {
            ConnectionStatus::Scanning => icons::SCANNING,
            ConnectionStatus::Connecting { .. } => icons::CONNECTING,
            ConnectionStatus::ConnectedWaiting { .. } => icons::WAITING,
            ConnectionStatus::Live { .. } => match self.app.last_reading.as_ref() {
                Some(r) if r.co2_ppm < 800 => icons::GOOD,
                Some(r) if r.co2_ppm <= 1000 => icons::MODERATE,
                Some(_) => icons::POOR,
                None => icons::WAITING,
            },
            ConnectionStatus::DeviceBusy { .. } => icons::BUSY,
            ConnectionStatus::Unreachable { .. } => icons::UNREACHABLE,
            ConnectionStatus::Error { .. } => icons::ERROR,
        }
    }

    fn title_text(&self) -> String {
        match (&self.app.status, &self.app.last_reading) {
            (ConnectionStatus::Live { .. }, Some(r)) => {
                format!("IAM-T1: {} ppm", r.co2_ppm)
            }
            (status, _) => format!("IAM-T1: {}", status.short_label()),
        }
    }

    fn tooltip_body(&self) -> String {
        let mut lines = Vec::new();
        if let Some(detail) = self.app.status.detail() {
            lines.push(detail.to_string());
        }
        if let Some(reading) = &self.app.last_reading {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(reading.format_lines());
        } else {
            lines.push("No reading yet".into());
        }
        lines.join("\n")
    }
}

impl ksni::Tray for InkBirdTray {
    fn id(&self) -> String {
        "inkbird-tray".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    fn icon_name(&self) -> String {
        // When we have a reading, the pixmap carries the digits; per the SNI
        // spec visualizers prefer IconName when non-empty, so blank it to let
        // the pixmap win. No reading yet: fall back to a theme icon.
        if self.app.last_reading.is_some() {
            String::new()
        } else {
            self.icon().into()
        }
    }

    /// The icon itself is the CO2 reading: ppm digits rendered as an ARGB32
    /// pixmap. White when fresh, grey when the last packet is older than
    /// STALE_AFTER (sensor likely off / link held by something else).
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let Some(reading) = &self.app.last_reading else {
            return Vec::new(); // no reading yet: fall back to icon_name
        };
        let fresh = (chrono::Local::now() - reading.timestamp).num_seconds()
            < crate::state::stale_window(
                crate::stale_floor_secs(),
                self.app.observed_interval_secs,
            );
        let rgb = crate::pixmap::digit_rgb(reading.co2_ppm, fresh);
        let p = crate::pixmap::render_digits(reading.co2_ppm, rgb);
        vec![ksni::Icon {
            width: p.width,
            height: p.height,
            data: p.data,
        }]
    }

    fn title(&self) -> String {
        self.title_text()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.title_text(),
            description: self.tooltip_body(),
            ..Default::default()
        }
    }

    fn watcher_online(&self) {
        tracing::info!("StatusNotifierWatcher online, tray icon registered");
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        match reason {
            ksni::OfflineReason::No => {
                tracing::info!("StatusNotifierWatcher went away, waiting for it to return")
            }
            reason => tracing::info!(
                "StatusNotifierWatcher unavailable ({}), waiting for it to appear",
                match reason {
                    ksni::OfflineReason::Error(e) => e.to_string(),
                    _ => "unknown".into(),
                }
            ),
        }
        true
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;

        let status_label = format!("Status: {}", self.app.status.short_label());
        let reading_label = self
            .app
            .last_reading
            .as_ref()
            .map(|r| r.format_one_line())
            .unwrap_or_else(|| "No reading yet".into());
        let has_reading = self.app.last_reading.is_some();
        let copy_text = self
            .app
            .last_reading
            .as_ref()
            .map(|r| r.format_one_line())
            .unwrap_or_default();

        vec![
            StandardItem {
                label: status_label,
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: reading_label,
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Copy latest readings".into(),
                enabled: has_reading,
                activate: Box::new(move |_this: &mut Self| {
                    if let Err(e) = copy_to_clipboard(&copy_text) {
                        tracing::warn!("clipboard copy failed: {e:#}");
                    } else {
                        tracing::info!("copied latest readings to clipboard");
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.action_tx.send(TrayAction::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    if text.is_empty() {
        anyhow::bail!("nothing to copy");
    }
    for (bin, args) in [
        ("xclip", &["-selection", "clipboard"][..]),
        ("xsel", &["--clipboard", "--input"][..]),
    ] {
        match Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(text.as_bytes())?;
                }
                let _ = child.wait();
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("neither xclip nor xsel is installed");
}

/// Spawn the tray. Returns the action receiver, or None if registration failed.
///
/// Failure is non-fatal: BLE logging still runs without a visible icon.
pub async fn spawn_tray(
    mut state_rx: watch::Receiver<AppState>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Option<mpsc::UnboundedReceiver<TrayAction>> {
    let (action_tx, action_rx) = mpsc::unbounded_channel();
    let tray = InkBirdTray {
        app: state_rx.borrow().clone(),
        action_tx,
    };

    // assume_sni_available keeps the service alive when the watcher is not
    // on the bus yet (slow session start / Cinnamon restart). Same pattern
    // as voxtype.
    let handle = match tray.assume_sni_available(true).spawn().await {
        Ok(handle) => {
            tracing::info!("system tray icon started (StatusNotifierItem id=inkbird-tray)");
            handle
        }
        Err(e) => {
            tracing::warn!("failed to start system tray icon: {e}");
            return None;
        }
    };

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                changed = state_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                _ = tick.tick() => {}
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
            if *shutdown_rx.borrow() {
                break;
            }
            let app = state_rx.borrow().clone();
            handle
                .update(move |tray| {
                    tray.app = app;
                })
                .await;
        }
        tracing::info!("tray update task exiting");
    });

    Some(action_rx)
}
