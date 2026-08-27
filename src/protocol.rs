//! InkBird IAM-T1 GATT notification packet parser.
//!
//! Byte layout is taken from live captures against the real device. Do not
//! "improve" the offsets without a new capture.

/// Company identifier in advertisement manufacturer data (0x3154).
pub const MFG_ID: u16 = 12628;

/// Manufacturer payload prefix: ASCII "AC-6200" followed by MAC bytes.
pub const MFG_PREFIX: &[u8] = b"AC-6200";

/// Case-insensitive advertisement local-name needle.
pub const NAME_HINT: &str = "iam-t1";

/// GATT service that owns the notify characteristic.
pub const SERVICE_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000_ffe0_0000_1000_8000_0080_5f9b_34fb);

/// Notify characteristic. Subscribe only; never poll or write.
pub const NOTIFY_UUID: uuid::Uuid =
    uuid::Uuid::from_u128(0x0000_ffe4_0000_1000_8000_0080_5f9b_34fb);

/// Temperature unit as reported by the device (or inferred).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempUnit {
    Celsius,
    Fahrenheit,
}

impl TempUnit {
    pub fn as_str(self) -> &'static str {
        match self {
            TempUnit::Celsius => "C",
            TempUnit::Fahrenheit => "F",
        }
    }
}

/// Fields extracted from a 16-byte DATA packet, before unit conversion.
#[derive(Debug, Clone, PartialEq)]
pub struct RawReading {
    pub temp: f32,
    pub humidity_pct: f32,
    pub co2_ppm: u16,
    pub pressure_hpa: u16,
}

/// Outcome of parsing one notification payload.
#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    Data(RawReading),
    State {
        fahrenheit: bool,
    },
    /// Recognized header but values fail the plausibility window.
    Implausible,
    Unknown,
}

/// Parse one GATT notification.
///
/// DATA (16 bytes, `55 aa 01 ...`):
///   sign     = data[4] & 0xF            (1 = negative temperature)
///   temp     = (data[5] << 8 | data[6]) / 10.0, negated if sign
///   humidity = (data[7] << 8 | data[8]) / 10.0  (%RH)
///   co2      =  data[9] << 8 | data[10]         (ppm)
///   pressure =  data[11] << 8 | data[12]        (hPa)
///
/// STATE (12 bytes, `55 aa 05 ...`):
///   data[10] & 0xF == 1 → Fahrenheit, 0 → Celsius
pub fn parse_packet(data: &[u8]) -> Packet {
    if data.len() == 16 && data.starts_with(&[0x55, 0xaa, 0x01]) {
        let sign = data[4] & 0x0F;
        let temp_raw = u16::from(data[5]) << 8 | u16::from(data[6]);
        let mut temp = f32::from(temp_raw) / 10.0;
        if sign == 1 {
            temp = -temp;
        }
        let humidity_pct = f32::from(u16::from(data[7]) << 8 | u16::from(data[8])) / 10.0;
        let co2_ppm = u16::from(data[9]) << 8 | u16::from(data[10]);
        let pressure_hpa = u16::from(data[11]) << 8 | u16::from(data[12]);

        if !(0.0..=100.0).contains(&humidity_pct)
            || co2_ppm > 5000
            || !(300..=1200).contains(&pressure_hpa)
            || !(-60.0..=150.0).contains(&temp)
        {
            return Packet::Implausible;
        }

        return Packet::Data(RawReading {
            temp,
            humidity_pct,
            co2_ppm,
            pressure_hpa,
        });
    }

    if data.len() == 12 && data.starts_with(&[0x55, 0xaa, 0x05]) {
        return Packet::State {
            fahrenheit: (data[10] & 0x0F) != 0,
        };
    }

    Packet::Unknown
}

/// Convert a raw temperature into Celsius given the unit.
pub fn to_celsius(temp: f32, unit: TempUnit) -> f32 {
    match unit {
        TempUnit::Celsius => temp,
        TempUnit::Fahrenheit => (temp - 32.0) * 5.0 / 9.0,
    }
}

/// Until a STATE packet arrives, infer the unit from magnitude.
/// Values above 65 are treated as Fahrenheit (device default).
pub fn infer_unit(temp: f32) -> TempUnit {
    if temp > 65.0 {
        TempUnit::Fahrenheit
    } else {
        TempUnit::Celsius
    }
}

/// Lowercase hex dump of a payload, used in logs.
pub fn hex_lower(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// True when advertisement local name matches the IAM-T1 fingerprint.
pub fn name_matches(name: &str) -> bool {
    name.to_ascii_lowercase().contains(NAME_HINT)
}

/// True when manufacturer data matches company ID 12628 and the AC-6200 prefix.
pub fn mfg_matches(mfg: &std::collections::HashMap<u16, Vec<u8>>) -> bool {
    mfg.get(&MFG_ID)
        .is_some_and(|payload| payload.starts_with(MFG_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn live_capture_data_packet() {
        // 55aa01101002df0262030d03da010043
        // temp 73.5 F, RH 61.0 %, CO2 781 ppm, 986 hPa
        let data = decode_hex("55aa01101002df0262030d03da010043");
        match parse_packet(&data) {
            Packet::Data(r) => {
                assert!((r.temp - 73.5).abs() < f32::EPSILON);
                assert!((r.humidity_pct - 61.0).abs() < f32::EPSILON);
                assert_eq!(r.co2_ppm, 781);
                assert_eq!(r.pressure_hpa, 986);
            }
            other => panic!("expected Data, got {other:?}"),
        }
        assert_eq!(infer_unit(73.5), TempUnit::Fahrenheit);
        let c = to_celsius(73.5, TempUnit::Fahrenheit);
        assert!((c - 23.055556).abs() < 0.01);
    }

    #[test]
    fn negative_temperature_sign_bit() {
        // sign nibble = 1, temp raw 0x00c8 = 20.0 → -20.0
        let mut data = decode_hex("55aa01101002df0262030d03da010043");
        data[4] = 0x01;
        data[5] = 0x00;
        data[6] = 0xc8;
        match parse_packet(&data) {
            Packet::Data(r) => assert!((r.temp + 20.0).abs() < f32::EPSILON),
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn state_packet_fahrenheit() {
        let mut data = vec![0u8; 12];
        data[0] = 0x55;
        data[1] = 0xaa;
        data[2] = 0x05;
        data[10] = 0x01;
        assert_eq!(parse_packet(&data), Packet::State { fahrenheit: true });
        data[10] = 0x00;
        assert_eq!(parse_packet(&data), Packet::State { fahrenheit: false });
    }

    #[test]
    fn drops_implausible_humidity() {
        let mut data = decode_hex("55aa01101002df0262030d03da010043");
        // humidity = 0x03e8 = 1000 → 100.0 is ok; 0x03e9 = 100.1 is not
        data[7] = 0x03;
        data[8] = 0xe9;
        assert_eq!(parse_packet(&data), Packet::Implausible);
    }

    #[test]
    fn drops_implausible_co2() {
        let mut data = decode_hex("55aa01101002df0262030d03da010043");
        data[9] = 0x13;
        data[10] = 0x89; // 5001
        assert_eq!(parse_packet(&data), Packet::Implausible);
    }

    #[test]
    fn unknown_short_payload() {
        assert_eq!(parse_packet(&[0x00, 0x01]), Packet::Unknown);
    }

    #[test]
    fn name_and_mfg_fingerprint() {
        assert!(name_matches("Ink@IAM-T1"));
        assert!(name_matches("ink@iam-t1 extra"));
        assert!(!name_matches("Ink@IBT-26S"));

        let mut mfg = std::collections::HashMap::new();
        mfg.insert(MFG_ID, b"AC-6200\x62\x00\xa1\x34\x31\x36".to_vec());
        assert!(mfg_matches(&mfg));
        mfg.insert(MFG_ID, b"XX-9999".to_vec());
        assert!(!mfg_matches(&mfg));
    }
}
