# inkbird-tray

Background service that talks to an [InkBird IAM-T1](https://inkbird.com/)
air-quality sensor over Bluetooth LE and shows live CO₂ / temperature /
humidity / pressure in a Linux system tray icon.

Built for Cinnamon on X11 (Linux Mint). The tray uses the StatusNotifierItem
D-Bus protocol via `ksni`.

## Build and run

```bash
cargo build --release
./target/release/inkbird-tray
```

Logs go to stderr. Increase verbosity with:

```bash
RUST_LOG=inkbird_tray=debug ./target/release/inkbird-tray
```

Readings are appended to:

```
~/.local/share/inkbird-tray/readings.csv
```

Header:

```
timestamp,co2_ppm,temp,unit,temp_c,humidity_pct,pressure_hpa
```

Stop with Ctrl-C, SIGTERM, or the tray **Quit** item. The process disconnects
from the sensor before exiting so the phone app can take the (single) BLE slot.

### systemd user unit

A unit file is shipped but **not installed**:

```
inkbird-tray.service
```

To enable it for your user session (optional):

```bash
mkdir -p ~/.config/systemd/user
cp inkbird-tray.service ~/.config/systemd/user/
# edit ExecStart if the binary is not at ~/inkbird-tray/target/release/inkbird-tray
systemctl --user daemon-reload
systemctl --user enable --now inkbird-tray.service
```

`Type=simple`, `Restart=always`, `RestartSec=10`, `After=bluetooth.target`,
`WantedBy=default.target`.

## BLE crate choice

The brief suggested `btleplug` 0.12+. This daemon uses **`bluer` 0.17**
(`bluetoothd` feature) instead:

- Official BlueZ D-Bus bindings, Linux-only (this project is already Linux-only
  because of StatusNotifierItem + the systemd unit).
- `Session::default_adapter()` maps to BlueZ's default adapter (`hci0` when
  present). On this machine `hci0` is the Broadcom BT 4.0 USB stick that can
  reach the sensor; `hci1` is the onboard Qualcomm radio.
- Native GATT notify stream (`Characteristic::notify`) with typed BlueZ error
  kinds (`AlreadyConnected`, `Failed`, …), which makes the "device busy" state
  reliable.
- `DiscoveryFilter { transport: Le }` so we do not waste the scan on classic
  Bluetooth.

## Device protocol

The IAM-T1 is BLE-only. There is no Wi-Fi and no cloud. Data is **not** in
advertisements; it arrives as GATT notifications after you subscribe.

**Advertisement fingerprint** (used to rediscover on every reconnect; the
address is random-static and must never be hard-coded):

- local name contains `iam-t1` (case-insensitive), e.g. `Ink@IAM-T1`
- and/or manufacturer data company ID `12628` (`0x3154`) whose payload starts
  with ASCII `AC-6200`

**GATT**

| | UUID |
| --- | --- |
| service | `0000ffe0-0000-1000-8000-00805f9b34fb` |
| notify characteristic | `0000ffe4-0000-1000-8000-00805f9b34fb` |

No init write is required. Do not poll or `Read Value`. Subscribe and wait.
The device pushes at its configured interval (60 s on the unit this was
verified against).

**16-byte DATA packet** — `bytes[0..3] == 55 aa 01`

| field | layout |
| --- | --- |
| sign | `data[4] & 0xF` (`1` = negative temperature) |
| temp | `(data[5] << 8 \| data[6]) / 10.0`, negated if sign |
| humidity %RH | `(data[7] << 8 \| data[8]) / 10.0` |
| CO₂ ppm | `data[9] << 8 \| data[10]` |
| pressure hPa | `data[11] << 8 \| data[12]` |

Verified capture `55aa01101002df0262030d03da010043` decodes to 73.5 °F,
61.0 %RH, 781 ppm, 986 hPa.

**12-byte STATE packet** — `bytes[0..3] == 55 aa 05`

`data[10] & 0xF`: `1` = Fahrenheit, `0` = Celsius. These arrive only
periodically (minutes). Until one is seen, the unit is inferred from magnitude
(`temp > 65` ⇒ Fahrenheit). The tray and CSV store the raw value with its unit
**and** Celsius.

Packets outside humidity 0–100 %, CO₂ ≤ 5000 ppm, pressure 300–1200 hPa, or
temp −60..150 are dropped.

The sensor accepts **one** central connection. The official phone app and this
daemon mutually exclude each other.

## Tray

StatusNotifierItem via `ksni` 0.3, same approach as voxtype on this Cinnamon
desktop (`assume_sni_available(true)` so a missing watcher is treated as
temporary).

### The icon IS the reading (pixmap digits)

When a reading exists, `icon_name` is blanked and `icon_pixmap` carries the
CO2 ppm rendered as 5x7 bitmap digits (44x44 ARGB32, `src/pixmap.rs`).
Cinnamon's xapp-sn-watcher saves SNI pixmaps to `/dev/shm/xapp-tmp-*.png`
and sets its IconName to that path, which St.Icon renders as a file icon —
so the digits appear in the panel. (Blanking IconName is required: the SNI
spec says visualizers prefer a non-empty IconName over the pixmap.)

- **White digits (250,250,250):** fresh, CO2 ≤ 800 ppm.
- **Pastel yellow (255,224,138):** fresh, CO2 > 800 ppm.
- **Bright red (255,64,64):** fresh, CO2 > 1000 ppm.
- **Grey (128,128,128):** stale ONLY — the reading is older than the stale
  window, i.e. the sensor is probably off, out of range, or its single BLE
  slot is held by the phone. Grey never encodes a CO2 level.
- Before the first reading, falls back to the state icons below.

The stale window adapts to the device's own rhythm: it is the floor
(150 s, override with `INKBIRD_TRAY_STALE_SECS` for testing) widened to
`2 * observed_interval + 30 s` once two packets have been seen — so a
sensor configured to 5-minute sampling in the InkBird app does not flicker
grey between packets.

### Fallback icon names (symbolic variants only)

Cinnamon's xapp-status renderer colors symbolic icons to match the panel
theme; fullcolor Mint-X PNGs render as dim grey silhouettes on a dark panel.
All names below exist in Mint-X (`view-refresh-symbolic` falls back to
Adwaita).

| state | freedesktop icon | CO₂ |
| --- | --- | --- |
| scanning | `view-refresh-symbolic` | — |
| connecting | `bluetooth-active-symbolic` | — |
| connected, no data yet | `bluetooth-active-symbolic` | — |
| live, good | `weather-clear-symbolic` | &lt; 800 ppm |
| live, moderate | `weather-few-clouds-symbolic` | 800–1200 ppm |
| live, poor | `weather-storm-symbolic` | &gt; 1200 ppm |
| device busy | `dialog-warning-symbolic` | — |
| unreachable | `network-offline-symbolic` | — |
| error | `dialog-error-symbolic` | — |

Title / tooltip show CO₂ ppm, temperature in both °F and °C, RH %, pressure
hPa, and reading age (`2 min ago`). Menu: latest readings, **Copy latest
readings** (needs `xclip` or `xsel`), **Quit**.

## Troubleshooting

**Tray is empty / missing, logs say the sensor is live.** The phone app is
fine; Cinnamon's StatusNotifier watcher was not on the session bus when the
process started. The daemon waits and re-registers. Restart Cinnamon
(`Ctrl-Alt-Esc` on Mint) or log out/in. Confirm:

```bash
busctl --user status org.kde.StatusNotifierWatcher
```

**`device busy` / connect timeout, sensor is sitting right there.** Close the
InkBird phone app (force-stop it; leaving it in the background keeps the GATT
link). The IAM-T1 has a single central slot. This daemon keeps retrying with
exponential backoff (5 s … 60 s).

**`not found` / scan never matches.** Pop the battery cap and check the
physical BLE switch. It is easy to knock off when changing cells. Also confirm
the USB BT 4.0 adapter (`hci0`) is the BlueZ default:

```bash
bluetoothctl list
```

`[default]` should be the Broadcom stick, not the onboard Qualcomm radio.

**`Failed to start system tray icon`.** No session bus (`DBUS_SESSION_BUS_ADDRESS`)
or you started the unit outside a graphical login. Run it from the desktop
session, or as a systemd *user* service after login.

**Permission / D-Bus errors talking to BlueZ.** Your user must be in the
`bluetooth` group (or otherwise allowed to use the system Bluetooth daemon).

**Readings stall after hours.** The reconnect loop rediscovers by name /
manufacturer ID on every drop. Check `journalctl --user -u inkbird-tray` (if
installed) or the process stderr. A stuck `device busy` almost always means
something else grabbed the sensor.

## Layout

```
src/main.rs       process, signals, task wiring
src/ble.rs        scan → connect → notify → backoff
src/protocol.rs   packet parser (unit-tested against the live capture)
src/tray.rs       ksni StatusNotifierItem
src/state.rs      watch-channel snapshot
src/csvlog.rs     ~/.local/share/inkbird-tray/readings.csv
inkbird-tray.service
```
