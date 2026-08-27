# Task: Rust daemon + system tray icon for an InkBird IAM-T1 air-quality sensor

Build a complete Rust project IN THE CURRENT DIRECTORY (~/inkbird-tray): a background
service that connects to an InkBird IAM-T1 sensor over Bluetooth LE, receives live
readings (CO2 ppm, temperature, humidity, air pressure), and presents them as a system
tray icon with tooltip, CO2-level-aware icon, and a small menu.

## Environment (verified facts — do not re-derive)

- Desktop: Cinnamon on X11, Linux. Tray must use the StatusNotifierItem D-Bus protocol.
- Working tray reference on this exact desktop: /opt/d/voxtype/src/tray.rs uses the
  `ksni = "0.3"` crate. It is READ-ONLY reference — do not modify anything under /opt/d.
  Its doc comment confirms ksni works with Cinnamon. It uses icon names from the
  freedesktop icon naming spec that exist in Mint-X ("audio-input-microphone",
  "media-record", "view-refresh"). Follow that pattern.
- Rust 1.97.1 and cargo are installed. crates.io network access works.
- BLE stack: BlueZ is running. Two adapters: hci0 (Broadcom BT4.0 USB) and hci1
  (Qualcomm BT5.3 onboard). The sensor is reachable on hci0. Use the default adapter.
- Working Python prototype for reference: /home/novi/inkbird-t1/read_iam_t1.py — READ
  ONLY. It has been STOPPED so the BLE device is free for you to connect to right now.

## Device protocol (empirically verified against the real device today — trust exactly)

The IAM-T1 is BLE-only (no WiFi, no cloud).

1. Advertisement fingerprint:
   - local name "Ink@IAM-T1" (case-insensitive substring "iam-t1")
   - manufacturer data company ID 12628 (0x3154), payload starts with ASCII "AC-6200"
     followed by the device's MAC bytes.
   - The BLE address is a RANDOM STATIC ADDRESS: XX:XX:XX:XX:XX:XX today, but it may
     change. Always rediscover by name/manufacturer ID; never hardcode the MAC.

2. Data does NOT come from advertisements. It arrives as GATT notifications:
   - service UUID 0000ffe0-0000-1000-8000-00805f9b34fb
   - characteristic UUID 0000ffe4-0000-1000-8000-00805f9b34fb
   - No init write is needed; subscribing to notifications starts the stream.

3. Packet formats (verified live captures):
   - 16-byte DATA packet: bytes[0..3] == 55 aa 01
       sign     = data[4] & 0xF              (1 = negative temperature)
       temp_raw = (data[5] as u16) << 8 | data[6] as u16   -> /10.0, negate if sign
       humidity = ((data[7] as u16) << 8 | data[8] as u16) -> /10.0 (%RH)
       co2_ppm  = (data[9] as u16) << 8 | data[10] as u16
       pressure = (data[11] as u16) << 8 | data[12] as u16 (hPa)
     Example capture: 55aa01101002df0262030d03da010043
       decodes to temp 73.5 F, RH 61.0 %, CO2 781 ppm, 986 hPa.
   - 12-byte STATE packet: bytes[0..3] == 55 aa 05 ... and data[10] & 0xF:
       1 = Fahrenheit, 0 = Celsius.
     State packets arrive only periodically (may take many minutes). Until one arrives,
     infer the unit from magnitude (temp value > 65 implies Fahrenheit). Store/display
     both the raw value with its unit AND Celsius.
   - Drop packets failing plausibility: humidity 0-100 %, CO2 <= 5000 ppm,
     pressure 300-1200 hPa, temp -60..150.

4. The device pushes at its own configured sampling interval (currently 60 s). Never
   poll or read characteristics; subscribe and wait.

5. CRITICAL constraint: the device accepts only ONE central connection at a time. The
   phone app blocks us and we block the phone app. Treat "cannot connect" as a
   first-class state (device busy), keep retrying with backoff, and surface it in the
   tray.

## Requirements

1. `cargo build --release` must succeed. Binary name: `inkbird-tray`.
2. Architecture: tokio-based. One task for BLE (scan -> connect -> subscribe ->
   notification loop -> reconnect with backoff; rediscover by advertisement fingerprint
   on every reconnect) and one for the tray UI. Communicate via tokio mpsc/watch
   channels. The tray must never block the BLE task.
3. Tray (ksni 0.3, modeled on /opt/d/voxtype/src/tray.rs):
   - title/tooltip shows latest readings: CO2 ppm, temperature (both F and C), RH %,
     pressure hPa, and reading age ("2 min ago").
   - icon_name changes by CO2 level: < 800 ppm good, 800-1200 moderate, > 1200 poor.
     Choose freedesktop icon names that exist in Mint-X/hicolor and document them in
     the README.
   - Distinct tray title/icon states: scanning, connecting, connected-but-no-data-yet,
     device busy/unreachable, error.
   - Menu: at minimum "Quit". Optionally "Copy latest readings".
4. Persist every reading to CSV at ~/.local/share/inkbird-tray/readings.csv with header:
   timestamp,co2_ppm,temp,unit,temp_c,humidity_pct,pressure_hpa
5. Graceful shutdown on SIGTERM/SIGINT.
6. Ship (do NOT install) a systemd user unit `inkbird-tray.service`: Type=simple,
   Restart=always, RestartSec=10, After=bluetooth.target, WantedBy=default.target.
7. Write README.md: build/run instructions, protocol notes, troubleshooting (phone app
   holding the connection, BLE switch under the battery cap).

## Constraints

- Create/modify files ONLY inside the current directory.
- Suggested BLE crate: btleplug (recent 0.12+). If you know a better-maintained Linux
  BLE GATT client crate that reliably supports notifications (e.g. bluer), you may use
  it instead — justify the choice in README.md.
- Do NOT modify anything under /opt/d, /home/novi/inkbird-t1, or system config.
- Prefer simple and robust over clever. This must run unattended for weeks.
- No unwrap() on network/BLE paths; use anyhow/thiserror and keep the reconnect loop
  alive through every failure.

## Definition of done

- `cargo build --release` passes.
- Run the binary against the REAL device for at least 2 minutes (it is free and
  advertising right now) and show a decoded data packet in the log output.
- Demonstrate the tray object is created and the StatusNotifier registration path runs
  without panicking on this session's D-Bus session bus (you may not be able to verify
  the icon visually; code-level evidence is acceptable).

Be rigorous. Use EXACTLY the protocol bytes above — do not guess the protocol.
