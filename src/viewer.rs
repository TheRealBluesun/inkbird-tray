//! History window: `inkbird-tray --viewer`, spawned by clicking the tray icon.
//!
//! A separate process so the GUI toolkit never shares a thread (or a crash)
//! with the BLE loop. Reads the SQLite store read-only and polls it for new
//! rows. The tray sends SIGUSR1 to raise an already-open window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeZone};
use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke, Vec2, Vec2b};
use egui_plot::{Axis, GridInput, GridMark, Line, Plot, PlotPoints, Points, Span, VLine};

use crate::db;

const POLL: Duration = Duration::from_secs(15);
/// Samples further apart than this are drawn as separate line segments
/// (sensor off, BLE held by the phone app, daemon stopped).
const GAP_SECS: f64 = 600.0;
const MIN_SPAN_SECS: f64 = 600.0;
/// Outdoor samples are hourly; break the line only when hours are missing.
const OUTDOOR_GAP_SECS: f64 = 3.0 * 3600.0;
/// Plot grid_spacing range: lines fade in from MIN to fully opaque at MAX px.
const GRID_MIN_PX: f32 = 10.0;
const GRID_MAX_PX: f32 = 30.0;

const PRESETS: [(&str, f64); 7] = [
    ("3h", 3.0 * 3600.0),
    ("12h", 12.0 * 3600.0),
    ("24h", 86400.0),
    ("3d", 3.0 * 86400.0),
    ("7d", 7.0 * 86400.0),
    ("30d", 30.0 * 86400.0),
    ("All", f64::INFINITY),
];
const DEFAULT_PRESET: usize = 2;

mod palette {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(0x0e, 0x11, 0x16);
    pub const PLOT_BG: Color32 = Color32::from_rgb(0x13, 0x17, 0x1e);
    pub const CARD: Color32 = Color32::from_rgb(0x19, 0x1e, 0x27);
    pub const CARD_HOVER: Color32 = Color32::from_rgb(0x22, 0x28, 0x33);
    pub const GRID: Color32 = Color32::from_rgb(0x2c, 0x33, 0x40);
    pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xe9, 0xef);
    pub const MUTED: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa3);
    pub const FAINT: Color32 = Color32::from_rgb(0x5a, 0x62, 0x70);
    pub const ACCENT: Color32 = Color32::from_rgb(0x2d, 0x4f, 0x5e);
    pub const CO2_GOOD: Color32 = Color32::from_rgb(0x34, 0xd3, 0x99);
    pub const CO2_MID: Color32 = Color32::from_rgb(0xfb, 0xbf, 0x24);
    pub const CO2_POOR: Color32 = Color32::from_rgb(0xfb, 0x92, 0x3c);
    pub const CO2_BAD: Color32 = Color32::from_rgb(0xf8, 0x71, 0x71);
    pub const TEMP: Color32 = Color32::from_rgb(0xfb, 0x71, 0x85);
    pub const RH: Color32 = Color32::from_rgb(0x38, 0xbd, 0xf8);
    pub const HPA: Color32 = Color32::from_rgb(0xa7, 0x8b, 0xfa);
    pub const OUTDOOR: Color32 = Color32::from_rgb(0xb4, 0xbe, 0xcd);
}

/// Same thresholds as the tray icon: green < 800, amber ≤ 1000, then worse.
fn co2_color(ppm: f64) -> Color32 {
    fn lerp(a: Color32, b: Color32, t: f64) -> Color32 {
        let t = t.clamp(0.0, 1.0) as f32;
        let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
        Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
    }
    use palette::*;
    match ppm {
        p if p < 700.0 => CO2_GOOD,
        p if p < 900.0 => lerp(CO2_GOOD, CO2_MID, (p - 700.0) / 200.0),
        p if p < 1200.0 => lerp(CO2_MID, CO2_POOR, (p - 900.0) / 300.0),
        p if p < 1600.0 => lerp(CO2_POOR, CO2_BAD, (p - 1200.0) / 400.0),
        _ => CO2_BAD,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Co2,
    Temp,
    Rh,
    Pressure,
}

impl Metric {
    const ALL: [Metric; 4] = [Metric::Co2, Metric::Temp, Metric::Rh, Metric::Pressure];

    fn idx(self) -> usize {
        self as usize
    }
    fn label(self) -> &'static str {
        match self {
            Metric::Co2 => "CO2",
            Metric::Temp => "Temperature",
            Metric::Rh => "Humidity",
            Metric::Pressure => "Pressure",
        }
    }
    fn unit(self, fahrenheit: bool) -> &'static str {
        match self {
            Metric::Co2 => "ppm",
            Metric::Temp if fahrenheit => "°F",
            Metric::Temp => "°C",
            Metric::Rh => "%",
            Metric::Pressure => "hPa",
        }
    }
    fn decimals(self) -> usize {
        match self {
            Metric::Temp => 1,
            _ => 0,
        }
    }
    fn color(self) -> Color32 {
        match self {
            Metric::Co2 => palette::CO2_GOOD,
            Metric::Temp => palette::TEMP,
            Metric::Rh => palette::RH,
            Metric::Pressure => palette::HPA,
        }
    }
    /// Smallest y span shown, so flat data isn't magnified into noise.
    fn min_span(self, fahrenheit: bool) -> f64 {
        match self {
            Metric::Co2 => 100.0,
            Metric::Temp if fahrenheit => 3.0,
            Metric::Temp => 1.5,
            Metric::Rh => 6.0,
            Metric::Pressure => 4.0,
        }
    }
    fn height_weight(self) -> f32 {
        match self {
            Metric::Co2 => 1.8,
            Metric::Temp => 1.3,
            _ => 1.0,
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    let raise = Arc::new(AtomicBool::new(false));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("IAM-T1 · Air quality history")
            .with_app_id("inkbird-tray")
            .with_inner_size([1180.0, 800.0])
            .with_min_inner_size([680.0, 480.0]),
        ..Default::default()
    };
    let raise_for_app = raise.clone();
    eframe::run_native(
        "inkbird-tray-viewer",
        options,
        Box::new(move |cc| {
            install_style(&cc.egui_ctx);
            spawn_raise_listener(cc.egui_ctx.clone(), raise_for_app.clone());
            Ok(Box::new(Viewer::new(raise_for_app)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("viewer window: {e}"))
}

fn spawn_raise_listener(ctx: egui::Context, raise: Arc<AtomicBool>) {
    let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGUSR1])
    else {
        return;
    };
    std::thread::spawn(move || {
        for _ in signals.forever() {
            raise.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        }
    });
}

fn install_style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = palette::BG;
    v.window_fill = palette::CARD;
    v.extreme_bg_color = palette::PLOT_BG;
    v.faint_bg_color = palette::CARD;
    v.override_text_color = Some(palette::TEXT);
    v.selection.bg_fill = palette::ACCENT;
    v.selection.stroke = Stroke::new(1.0, palette::TEXT);
    let r = CornerRadius::same(7);
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.noninteractive,
    ] {
        w.corner_radius = r;
    }
    v.widgets.inactive.weak_bg_fill = palette::CARD;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.hovered.weak_bg_fill = palette::CARD_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.active.weak_bg_fill = palette::ACCENT;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette::GRID);
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 8.0);
        s.spacing.button_padding = Vec2::new(12.0, 5.0);
    });
}

struct Viewer {
    conn: Option<rusqlite::Connection>,
    error: Option<String>,
    xs: Vec<f64>,
    /// co2, temp_c, rh, hpa — temperature converted at display time.
    ys: [Vec<f64>; 4],
    /// Outdoor temperature (unix ts, °C), hourly.
    outdoor: Vec<(f64, f64)>,
    fahrenheit: bool,
    last_ts: i64,
    last_poll: Instant,
    /// Current x range (Unix seconds), read back from the plots every frame.
    view: (f64, f64),
    /// Push `view` into the plots on the next frame (preset, keys, follow).
    apply_view: bool,
    /// Keep the right edge pinned to the newest sample as data arrives.
    follow: bool,
    preset: Option<usize>,
    hover_x: Option<f64>,
    raise: Arc<AtomicBool>,
}

impl Viewer {
    fn new(raise: Arc<AtomicBool>) -> Self {
        let mut v = Self {
            conn: None,
            error: None,
            xs: Vec::new(),
            ys: Default::default(),
            outdoor: Vec::new(),
            fahrenheit: false,
            last_ts: i64::MIN,
            last_poll: Instant::now(),
            view: (0.0, 1.0),
            apply_view: true,
            follow: true,
            preset: Some(DEFAULT_PRESET),
            hover_x: None,
            raise,
        };
        v.poll();
        v.set_preset(DEFAULT_PRESET);
        v
    }

    fn poll(&mut self) {
        self.last_poll = Instant::now();
        if self.conn.is_none() {
            match db::open_readonly() {
                Ok(c) => self.conn = Some(c),
                Err(e) => {
                    self.error = Some(format!("{e:#}"));
                    return;
                }
            }
        }
        let rows = match db::load_after(self.conn.as_ref().expect("opened"), self.last_ts) {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                self.conn = None;
                return;
            }
        };
        self.error = None;
        match db::load_outdoor(self.conn.as_ref().expect("opened")) {
            Ok(o) => self.outdoor = o.into_iter().map(|(t, c)| (t as f64, c)).collect(),
            Err(e) => tracing::warn!("outdoor temperature: {e:#}"),
        }
        let Some(last) = rows.last() else { return };
        let prev_end = self.xs.last().copied();
        self.fahrenheit = last.fahrenheit;
        self.last_ts = last.ts;
        for r in &rows {
            self.xs.push(r.ts as f64);
            self.ys[0].push(r.co2_ppm);
            self.ys[1].push(r.temp_c);
            self.ys[2].push(r.humidity_pct);
            self.ys[3].push(r.pressure_hpa);
        }
        if let (true, Some(prev)) = (self.follow, prev_end) {
            let shift = last.ts as f64 - prev;
            self.view = (self.view.0 + shift, self.view.1 + shift);
            self.apply_view = true;
        }
    }

    fn value(&self, m: Metric, i: usize) -> f64 {
        let y = self.ys[m.idx()][i];
        match m {
            Metric::Temp if self.fahrenheit => y * 9.0 / 5.0 + 32.0,
            _ => y,
        }
    }

    fn to_display_temp(&self, c: f64) -> f64 {
        if self.fahrenheit {
            c * 9.0 / 5.0 + 32.0
        } else {
            c
        }
    }

    /// Outdoor temperature at `x`, linearly interpolated between hourly
    /// samples, in the display unit.
    fn outdoor_at(&self, x: f64) -> Option<f64> {
        let o = &self.outdoor;
        let i = o.partition_point(|p| p.0 < x);
        let v = match (i.checked_sub(1).map(|j| o[j]), o.get(i).copied()) {
            (Some(a), Some(b)) if b.0 - a.0 <= OUTDOOR_GAP_SECS => {
                let t = if b.0 > a.0 { (x - a.0) / (b.0 - a.0) } else { 0.0 };
                a.1 + (b.1 - a.1) * t
            }
            // Past the newest sample (or before the oldest): hold it for an hour.
            (Some(a), _) if x - a.0 <= 3600.0 => a.1,
            (_, Some(b)) if b.0 - x <= 3600.0 => b.1,
            _ => return None,
        };
        Some(self.to_display_temp(v))
    }

    /// Visible outdoor samples as line segments, split at missing hours.
    fn outdoor_segments(&self, lo: f64, hi: f64) -> Vec<Vec<[f64; 2]>> {
        let o = &self.outdoor;
        let start = o.partition_point(|p| p.0 < lo).saturating_sub(1);
        let end = (o.partition_point(|p| p.0 <= hi) + 1).min(o.len());
        let mut segs: Vec<Vec<[f64; 2]>> = Vec::new();
        for i in start..end {
            let p = [o[i].0, self.to_display_temp(o[i].1)];
            match segs.last_mut() {
                Some(seg) if p[0] - seg.last().expect("non-empty")[0] <= OUTDOOR_GAP_SECS => {
                    seg.push(p)
                }
                _ => segs.push(vec![p]),
            }
        }
        segs
    }

    fn data_extent(&self) -> Option<(f64, f64)> {
        Some((*self.xs.first()?, *self.xs.last()?))
    }

    fn set_preset(&mut self, i: usize) {
        let Some((first, last)) = self.data_extent() else {
            return;
        };
        let span = PRESETS[i].1.min(last - first).max(MIN_SPAN_SECS);
        let pad = span * 0.015;
        self.view = (last - span, last + pad);
        self.apply_view = true;
        self.follow = true;
        self.preset = Some(i);
    }

    fn pan(&mut self, frac: f64) {
        let d = (self.view.1 - self.view.0) * frac;
        self.view = (self.view.0 + d, self.view.1 + d);
        self.apply_view = true;
        self.release_follow();
    }

    fn release_follow(&mut self) {
        self.follow = false;
        self.preset = None;
    }

    fn visible_range(&self) -> std::ops::Range<usize> {
        let a = self.xs.partition_point(|&x| x < self.view.0);
        let b = self.xs.partition_point(|&x| x <= self.view.1);
        a..b.max(a)
    }

    fn nearest(&self, x: f64) -> Option<usize> {
        let i = self.xs.partition_point(|&v| v < x);
        let cands = [i.checked_sub(1), (i < self.xs.len()).then_some(i)];
        let best = cands
            .into_iter()
            .flatten()
            .min_by(|&a, &b| (self.xs[a] - x).abs().total_cmp(&(self.xs[b] - x).abs()))?;
        ((self.xs[best] - x).abs() <= GAP_SECS).then_some(best)
    }

    /// Visible samples reduced to a min/max pair per horizontal bucket, split
    /// into separate segments at gaps. Keeps spikes, caps point count.
    fn decimate(&self, m: Metric, lo: f64, hi: f64, buckets: usize) -> Vec<Vec<[f64; 2]>> {
        let start = self.xs.partition_point(|&x| x < lo).saturating_sub(1);
        let end = (self.xs.partition_point(|&x| x <= hi) + 1).min(self.xs.len());
        let mut segs = Vec::new();
        if start >= end {
            return segs;
        }
        let bw = ((hi - lo) / buckets.max(1) as f64).max(1.0);
        let gap = GAP_SECS.max(bw * 2.5);

        let mut seg: Vec<[f64; 2]> = Vec::new();
        let mut bucket: Option<(i64, [f64; 2], [f64; 2])> = None;
        let flush = |seg: &mut Vec<[f64; 2]>, b: &mut Option<(i64, [f64; 2], [f64; 2])>| {
            if let Some((_, mn, mx)) = b.take() {
                if mn[0] == mx[0] {
                    seg.push(mn);
                } else if mn[0] < mx[0] {
                    seg.extend([mn, mx]);
                } else {
                    seg.extend([mx, mn]);
                }
            }
        };
        for i in start..end {
            let p = [self.xs[i], self.value(m, i)];
            if i > start && p[0] - self.xs[i - 1] > gap {
                flush(&mut seg, &mut bucket);
                segs.push(std::mem::take(&mut seg));
            }
            let b = ((p[0] - lo) / bw).floor() as i64;
            match &mut bucket {
                Some((cur, mn, mx)) if *cur == b => {
                    if p[1] < mn[1] {
                        *mn = p;
                    }
                    if p[1] > mx[1] {
                        *mx = p;
                    }
                }
                _ => {
                    flush(&mut seg, &mut bucket);
                    bucket = Some((b, p, p));
                }
            }
        }
        flush(&mut seg, &mut bucket);
        segs.push(seg);
        segs.retain(|s| !s.is_empty());
        segs
    }

    fn stats(&self, m: Metric, range: std::ops::Range<usize>) -> Option<(f64, f64, f64)> {
        if range.is_empty() {
            return None;
        }
        let (mut mn, mut mx, mut sum) = (f64::INFINITY, f64::NEG_INFINITY, 0.0);
        for i in range.clone() {
            let y = self.value(m, i);
            mn = mn.min(y);
            mx = mx.max(y);
            sum += y;
        }
        Some((mn, sum / range.len() as f64, mx))
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Air quality").size(22.0).strong());
            if let Some(&last) = self.xs.last() {
                ui.label(
                    RichText::new(format!(
                        "  InkBird IAM-T1 · {} readings · latest {}",
                        self.xs.len(),
                        fmt_ts(last, "%a %b %-d, %H:%M")
                    ))
                    .color(palette::MUTED),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let live = if self.follow {
                    ui.add_enabled(false, egui::Button::new("Live"))
                } else {
                    ui.button("Jump to now")
                }
                .on_hover_text("Follow new readings");
                if self.follow {
                    dot(ui, palette::CO2_GOOD);
                }
                if live.clicked() {
                    self.set_preset(self.preset.unwrap_or(DEFAULT_PRESET));
                }
                ui.add_space(12.0);
                for i in (0..PRESETS.len()).rev() {
                    if ui
                        .selectable_label(self.preset == Some(i), PRESETS[i].0)
                        .clicked()
                    {
                        self.set_preset(i);
                    }
                }
            });
        });
        ui.add_space(6.0);

        let visible = self.visible_range();
        let focus = self.hover_x.and_then(|x| self.nearest(x));
        let shown = focus.or(self.xs.len().checked_sub(1));
        ui.columns(4, |cols| {
            for (m, ui) in Metric::ALL.into_iter().zip(cols.iter_mut()) {
                self.card(ui, m, shown, focus.is_some(), visible.clone());
            }
        });
    }

    fn card(
        &self,
        ui: &mut egui::Ui,
        m: Metric,
        shown: Option<usize>,
        hovering: bool,
        visible: std::ops::Range<usize>,
    ) {
        egui::Frame::new()
            .fill(palette::CARD)
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let value = shown.map(|i| self.value(m, i));
                let color = match (m, value) {
                    (Metric::Co2, Some(v)) => co2_color(v),
                    _ => m.color(),
                };
                ui.horizontal(|ui| {
                    dot(ui, color);
                    ui.label(RichText::new(m.label()).color(palette::MUTED).size(12.5));
                    if let Some(i) = shown {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let when = if hovering {
                                fmt_ts(self.xs[i], "%a %-d %b %H:%M")
                            } else {
                                "latest".into()
                            };
                            ui.label(RichText::new(when).color(palette::FAINT).size(11.5));
                        });
                    }
                });
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let text = value
                        .map(|v| format!("{:.*}", m.decimals(), v))
                        .unwrap_or_else(|| "–".into());
                    ui.label(RichText::new(text).size(30.0).color(color).strong());
                    ui.label(
                        RichText::new(m.unit(self.fahrenheit))
                            .size(14.0)
                            .color(palette::MUTED),
                    );
                    let outside = match (m, shown) {
                        (Metric::Temp, Some(i)) => self.outdoor_at(self.xs[i]),
                        _ => None,
                    };
                    if let Some(o) = outside {
                        ui.add_space(10.0);
                        dot(ui, palette::OUTDOOR);
                        ui.label(
                            RichText::new(format!("outside {o:.0}{}", m.unit(self.fahrenheit)))
                                .size(13.0)
                                .color(palette::OUTDOOR),
                        );
                    }
                });
                let summary = match self.stats(m, visible) {
                    Some((mn, avg, mx)) => {
                        let d = m.decimals();
                        format!("min {mn:.d$}   avg {avg:.d$}   max {mx:.d$}")
                    }
                    None => "no data in view".into(),
                };
                ui.label(RichText::new(summary).size(11.5).color(palette::FAINT));
            });
    }

    fn plots(&mut self, ui: &mut egui::Ui) {
        let total_weight: f32 = Metric::ALL.iter().map(|m| m.height_weight()).sum();
        let footer = 22.0;
        let gaps = 6.0 * (Metric::ALL.len() as f32 - 1.0);
        let avail = (ui.available_height() - footer - gaps).max(200.0);

        let apply = std::mem::take(&mut self.apply_view).then_some(self.view);
        let mut new_view = None;
        let mut new_hover = None;
        let mut interacted = false;
        let hover_idx = self.hover_x.and_then(|x| self.nearest(x));
        let buckets = (ui.available_width() as usize).max(200);

        for (k, m) in Metric::ALL.into_iter().enumerate() {
            let last = k == Metric::ALL.len() - 1;
            let height = avail * m.height_weight() / total_weight;
            let decimals = m.decimals();
            let unit = m.unit(self.fahrenheit);

            let plot = Plot::new(("iam-t1", k))
                .height(height)
                .link_axis("iam-t1-x", Vec2b::new(true, false))
                .allow_drag(Vec2b::new(true, false))
                .allow_zoom(Vec2b::new(true, false))
                .allow_scroll(false)
                .allow_boxed_zoom(false)
                .allow_double_click_reset(false)
                .show_x(false)
                .show_y(false)
                .show_axes(Vec2b::new(last, true))
                .y_axis_min_width(58.0)
                .y_axis_formatter(|mark, _| {
                    let d = if mark.step_size < 1.0 { 1 } else { 0 };
                    format!("{:.*}", d, mark.value)
                })
                .y_grid_spacer(value_grid)
                .grid_spacing(GRID_MIN_PX..=GRID_MAX_PX)
                .x_grid_spacer(time_grid)
                .x_axis_formatter(|mark, _| fmt_axis_time(mark))
                .grid_color(palette::GRID)
                .set_margin_fraction(Vec2::new(0.0, 0.0));

            let resp = plot.show(ui, |pui| {
                if let Some((lo, hi)) = apply {
                    pui.set_plot_bounds_x(lo..=hi);
                }
                let mut touched = false;
                if pui.response().hovered() {
                    let (scroll, shift) =
                        pui.ctx().input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
                    let (sx, sy) = if shift { (scroll.y, 0.0) } else { (scroll.x, scroll.y) };
                    if sy != 0.0 {
                        let f = (sy * 0.003).exp();
                        pui.zoom_bounds_around_hovered(Vec2::new(f, 1.0));
                        touched = true;
                    }
                    if sx != 0.0 {
                        pui.translate_bounds(Vec2::new(sx, 0.0));
                        touched = true;
                    }
                }

                let b = pui.plot_bounds();
                let (lo, hi) = (b.min()[0], b.max()[0]);
                let segs = self.decimate(m, lo, hi, buckets);
                let outdoor = if m == Metric::Temp {
                    self.outdoor_segments(lo, hi)
                } else {
                    Vec::new()
                };

                let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
                for p in segs.iter().flatten().chain(outdoor.iter().flatten()) {
                    ymin = ymin.min(p[1]);
                    ymax = ymax.max(p[1]);
                }
                if !ymin.is_finite() {
                    (ymin, ymax) = (0.0, 1.0);
                }
                let pad = ((ymax - ymin) * 0.14).max(0.0);
                let (mut ylo, mut yhi) = (ymin - pad, ymax + pad);
                let min_span = m.min_span(self.fahrenheit);
                if yhi - ylo < min_span {
                    let c = (ylo + yhi) / 2.0;
                    (ylo, yhi) = (c - min_span / 2.0, c + min_span / 2.0);
                }
                pui.set_auto_bounds(Vec2b::new(false, false));
                pui.set_plot_bounds_y(ylo..=yhi);

                if m == Metric::Co2 {
                    pui.span(
                        Span::new("", 800.0..=1000.0)
                            .axis(Axis::Y)
                            .fill(palette::CO2_MID.gamma_multiply(0.05))
                            .border_width(0.0),
                    );
                    pui.span(
                        Span::new("", 1000.0..=100_000.0)
                            .axis(Axis::Y)
                            .fill(palette::CO2_BAD.gamma_multiply(0.06))
                            .border_width(0.0),
                    );
                }

                for seg in segs {
                    let mut line = Line::new(m.label(), PlotPoints::new(seg))
                        .color(m.color())
                        .width(1.8)
                        .fill(ylo as f32)
                        .fill_alpha(0.10)
                        .allow_hover(false);
                    if m == Metric::Co2 {
                        line = line.gradient_color(Arc::new(|p| co2_color(p.y)), true);
                    }
                    pui.line(line);
                }

                for seg in outdoor {
                    pui.line(
                        Line::new("outside", PlotPoints::new(seg))
                            .color(palette::OUTDOOR.gamma_multiply(0.85))
                            .width(1.4)
                            .style(egui_plot::LineStyle::Dashed { length: 6.0 })
                            .allow_hover(false),
                    );
                }

                if let Some(i) = hover_idx {
                    let x = self.xs[i];
                    let y = self.value(m, i);
                    pui.vline(
                        VLine::new("", x)
                            .color(palette::MUTED.gamma_multiply(0.6))
                            .width(1.0)
                            .allow_hover(false),
                    );
                    let c = if m == Metric::Co2 { co2_color(y) } else { m.color() };
                    pui.points(
                        Points::new("", vec![[x, y]])
                            .radius(4.5)
                            .color(c)
                            .filled(true)
                            .allow_hover(false),
                    );
                    if let Some(o) = (m == Metric::Temp).then(|| self.outdoor_at(x)).flatten() {
                        pui.points(
                            Points::new("", vec![[x, o]])
                                .radius(3.5)
                                .color(palette::OUTDOOR)
                                .filled(true)
                                .allow_hover(false),
                        );
                    }
                }

                let hover = pui
                    .response()
                    .hovered()
                    .then(|| pui.pointer_coordinate())
                    .flatten()
                    .map(|p| p.x);
                (lo, hi, touched, hover)
            });

            let (lo, hi, touched, hover) = resp.inner;
            if k == 0 {
                new_view = Some((lo, hi));
            }
            if hover.is_some() {
                new_hover = hover;
            }
            if touched || resp.response.dragged() {
                interacted = true;
            }
            if let Some(i) = hover_idx.filter(|_| hover.is_some()) {
                let outside = (m == Metric::Temp)
                    .then(|| self.outdoor_at(self.xs[i]))
                    .flatten()
                    .map(|o| format!("\noutside {o:.1} {unit}"))
                    .unwrap_or_default();
                resp.response.on_hover_text_at_pointer(format!(
                    "{}\n{:.*} {}{}",
                    fmt_ts(self.xs[i], "%a %-d %b  %H:%M"),
                    decimals,
                    self.value(m, i),
                    unit,
                    outside
                ));
            }
            if !last {
                ui.add_space(6.0 - ui.spacing().item_spacing.y);
            }
        }

        if let Some(v) = new_view {
            self.view = v;
        }
        self.hover_x = new_hover;
        if interacted {
            self.release_follow();
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(
                    "scroll to zoom · drag or shift+scroll to pan · arrow keys step · Esc closes",
                )
                .size(11.5)
                .color(palette::FAINT),
            );
        });
    }
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.raise.swap(false, Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if self.last_poll.elapsed() >= POLL {
            self.poll();
        }
        ctx.request_repaint_after(POLL);

        let (left, right, esc) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if esc {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if left {
            self.pan(-0.25);
        }
        if right {
            self.pan(0.25);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(palette::BG).inner_margin(Margin::same(18)))
            .show(ui, |ui| {
                if self.xs.is_empty() {
                    ui.centered_and_justified(|ui| {
                        let msg = match &self.error {
                            Some(e) => format!("Cannot read history database\n\n{e}"),
                            None => "No readings recorded yet".into(),
                        };
                        ui.label(RichText::new(msg).color(palette::MUTED).size(15.0));
                    });
                    return;
                }
                self.header(ui);
                ui.add_space(10.0);
                self.plots(ui);
            });

        if self.apply_view {
            ctx.request_repaint();
        }
    }
}

fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

fn local(ts: f64) -> Option<DateTime<Local>> {
    Local.timestamp_opt(ts as i64, 0).single()
}

fn fmt_ts(ts: f64, fmt: &str) -> String {
    local(ts).map(|t| t.format(fmt).to_string()).unwrap_or_default()
}

/// Steps for the time axis, aligned to local wall-clock boundaries.
const TIME_STEPS: [f64; 15] = [
    60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0, 10800.0, 21600.0, 43200.0,
    86400.0, 172800.0, 604800.0, 1209600.0,
];

fn time_grid(input: GridInput) -> Vec<GridMark> {
    let (lo, hi) = input.bounds;
    // Aim for a label roughly every 90 px.
    let target = input.base_step_size * 90.0 / GRID_MIN_PX as f64;
    let minor = TIME_STEPS
        .iter()
        .copied()
        .find(|&s| s >= target / 4.0)
        .unwrap_or(*TIME_STEPS.last().unwrap());
    let major = TIME_STEPS
        .iter()
        .copied()
        .find(|&s| s >= target && s % minor == 0.0)
        .unwrap_or(minor * 4.0);

    let offset = local(lo).map(|t| t.offset().local_minus_utc() as f64).unwrap_or(0.0);
    let mut marks = Vec::new();
    let mut t = ((lo + offset) / minor).floor() * minor - offset;
    while t <= hi {
        if t >= lo {
            let is_major = ((t + offset) / major).fract().abs() < 1e-9;
            marks.push(GridMark {
                value: t,
                step_size: if is_major { major } else { minor },
            });
        }
        t += minor;
    }
    marks
}

/// One tier of 1/2/5 × 10^k gridlines, ≥ 22 px apart, so every line is labeled.
/// `base_step_size` is GRID_MIN_PX worth of plot units.
fn value_grid(input: GridInput) -> Vec<GridMark> {
    let (lo, hi) = input.bounds;
    let raw = input.base_step_size * 22.0 / GRID_MIN_PX as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0]
        .into_iter()
        .map(|k| k * mag)
        .find(|&s| s >= raw)
        .unwrap_or(10.0 * mag);
    let mut marks = Vec::new();
    let mut v = (lo / step).ceil() * step;
    while v <= hi {
        marks.push(GridMark { value: v, step_size: step });
        v += step;
    }
    marks
}

fn fmt_axis_time(mark: GridMark) -> String {
    let Some(t) = local(mark.value) else {
        return String::new();
    };
    let midnight = t.format("%H:%M").to_string() == "00:00";
    if mark.step_size >= 86400.0 || midnight {
        t.format("%a %-d %b").to_string()
    } else {
        t.format("%H:%M").to_string()
    }
}
