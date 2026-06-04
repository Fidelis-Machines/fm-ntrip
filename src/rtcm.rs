//! RTCM 3 framing and message decoding, shared by the caster self-test and the
//! rover client.
//!
//! RTCM 3 frame layout:
//!   byte 0      : 0xD3 preamble
//!   byte 1..2   : 6 reserved bits + 10-bit payload length (big-endian)
//!   byte 3..    : payload  (first 12 bits = message number)
//!   last 3 bytes: CRC-24Q over preamble + length + payload

/// CRC-24Q (RTCM 3 / Qualcomm), polynomial 0x1864CFB, init 0.
pub fn crc24q(data: &[u8]) -> u32 {
    const POLY: u32 = 0x0186_4CFB;
    let mut crc: u32 = 0;
    for &b in data {
        crc ^= (b as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x0100_0000 != 0 {
                crc ^= POLY;
            }
        }
    }
    crc & 0x00FF_FFFF
}

/// Minimal big-endian bit reader over an RTCM payload.
struct Bits<'a> {
    d: &'a [u8],
}
impl Bits<'_> {
    /// Unsigned big-endian field of `len` bits starting at bit `start`.
    fn u(&self, start: usize, len: usize) -> u64 {
        let mut v = 0u64;
        for i in 0..len {
            let bit = start + i;
            v = (v << 1) | ((self.d[bit / 8] >> (7 - (bit % 8))) & 1) as u64;
        }
        v
    }
    /// Two's-complement signed field.
    fn i(&self, start: usize, len: usize) -> i64 {
        let v = self.u(start, len);
        if v & (1 << (len - 1)) != 0 {
            v as i64 - (1i64 << len)
        } else {
            v as i64
        }
    }
}

/// WGS84 ECEF (metres) → geodetic latitude/longitude (degrees) and height (m).
pub fn ecef_to_geodetic(x: f64, y: f64, z: f64) -> (f64, f64, f64) {
    const A: f64 = 6_378_137.0;
    const F: f64 = 1.0 / 298.257_223_563;
    let e2 = F * (2.0 - F);
    let lon = y.atan2(x);
    let p = (x * x + y * y).sqrt();
    let mut lat = z.atan2(p * (1.0 - e2));
    let mut h = 0.0;
    for _ in 0..6 {
        let s = lat.sin();
        let n = A / (1.0 - e2 * s * s).sqrt();
        h = p / lat.cos() - n;
        lat = z.atan2(p * (1.0 - e2 * n / (n + h)));
    }
    (lat.to_degrees(), lon.to_degrees(), h)
}

/// The RTCM message number (DF002) — first 12 bits of the payload.
pub fn message_number(payload: &[u8]) -> Option<u16> {
    if payload.len() < 2 {
        None
    } else {
        Some(((payload[0] as u16) << 4) | ((payload[1] as u16) >> 4))
    }
}

/// Human-readable description of one RTCM message payload, as one or more lines
/// (the first line is prefixed with `[<type>]`). Best-effort: only the common
/// base-station messages are decoded in detail; others get a type label.
pub fn describe_message(payload: &[u8]) -> Vec<String> {
    let Some(msg) = message_number(payload) else {
        return Vec::new();
    };
    let b = Bits { d: payload };
    let nbits = payload.len() * 8;
    match msg {
        // Stationary antenna reference position → decode ECEF and convert to lat/lon.
        1005 | 1006 if nbits >= 152 => {
            let id = b.u(12, 12);
            let x = b.i(34, 38) as f64 * 0.0001;
            let y = b.i(74, 38) as f64 * 0.0001;
            let z = b.i(114, 38) as f64 * 0.0001;
            let (lat, lon, h) = ecef_to_geodetic(x, y, z);
            let ns = if lat >= 0.0 { 'N' } else { 'S' };
            let ew = if lon >= 0.0 { 'E' } else { 'W' };
            vec![
                format!("[{msg}] base station #{id} antenna reference position:"),
                format!(
                    "      {:.7}°{ns}  {:.7}°{ew}  height {:.2} m",
                    lat.abs(),
                    lon.abs(),
                    h
                ),
                format!("      ECEF  X={x:.3}  Y={y:.3}  Z={z:.3}  (metres)"),
            ]
        }
        // MSM observation messages → satellite count comes from the 64-bit mask.
        1071..=1077 | 1081..=1087 | 1091..=1097 | 1101..=1107 | 1111..=1117 | 1121..=1127
            if nbits >= 137 =>
        {
            let sys = msm_system(msg).unwrap_or("GNSS");
            let nsat = msm_satellite_count(payload).unwrap_or(0);
            vec![format!(
                "[{msg}] {sys} observations (MSM) — {nsat} satellites tracked"
            )]
        }
        1019 => vec![format!("[{msg}] GPS satellite ephemeris")],
        1020 => vec![format!("[{msg}] GLONASS satellite ephemeris")],
        1042 => vec![format!("[{msg}] BeiDou satellite ephemeris")],
        1046 => vec![format!("[{msg}] Galileo satellite ephemeris")],
        1230 => vec![format!("[{msg}] GLONASS code-phase biases")],
        1007 | 1008 | 1033 => vec![format!("[{msg}] antenna / receiver descriptor")],
        _ => vec![format!("[{msg}] RTCM message")],
    }
}

/// GNSS constellation name for an MSM message number (1071–1127), if it is one.
pub fn msm_system(msg: u16) -> Option<&'static str> {
    match msg {
        1071..=1077 => Some("GPS"),
        1081..=1087 => Some("GLONASS"),
        1091..=1097 => Some("Galileo"),
        1101..=1107 => Some("SBAS"),
        1111..=1117 => Some("QZSS"),
        1121..=1127 => Some("BeiDou"),
        _ => None,
    }
}

/// Satellite count in an MSM message — popcount of the 64-bit satellite mask.
pub fn msm_satellite_count(payload: &[u8]) -> Option<u32> {
    if payload.len() * 8 < 137 {
        return None;
    }
    Some(Bits { d: payload }.u(73, 64).count_ones())
}

/// Antenna reference position (lat°, lon°, height m) from a 1005/1006 message.
pub fn station_position(payload: &[u8]) -> Option<(f64, f64, f64)> {
    if payload.len() * 8 < 152 {
        return None;
    }
    let b = Bits { d: payload };
    let x = b.i(34, 38) as f64 * 0.0001;
    let y = b.i(74, 38) as f64 * 0.0001;
    let z = b.i(114, 38) as f64 * 0.0001;
    Some(ecef_to_geodetic(x, y, z))
}

/// Reference station ID (DF003) — present in most station messages.
pub fn station_id(payload: &[u8]) -> Option<u16> {
    if payload.len() * 8 < 24 {
        return None;
    }
    Some(Bits { d: payload }.u(12, 12) as u16)
}

/// One deframed RTCM record produced by [`Deframer`].
pub struct Frame {
    /// The message payload (header and CRC stripped). Empty for a CRC failure.
    pub payload: Vec<u8>,
    /// Whether the CRC-24Q over the frame validated.
    pub crc_ok: bool,
}
impl Frame {
    /// The message number, or `None` if the payload is too short / invalid.
    pub fn msg_type(&self) -> Option<u16> {
        message_number(&self.payload)
    }
}

/// Incremental RTCM 3 deframer: feed it bytes from any source (serial, TCP) and
/// it returns complete frames as they become available, buffering partial ones.
/// It resyncs past false preambles and CRC failures.
#[derive(Default)]
pub struct Deframer {
    buf: Vec<u8>,
}
impl Deframer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `data` and return every complete frame now available. Each frame
    /// is reported as valid (`crc_ok == true`, with `payload`) or as a single
    /// CRC failure (`crc_ok == false`, empty `payload`); the caller decides how
    /// to treat failures. Bytes of a partial trailing frame are retained.
    pub fn push(&mut self, data: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut i = 0;
        while self.buf.len() >= i + 3 {
            if self.buf[i] != 0xD3 {
                i += 1; // not a preamble — slide forward to resync
                continue;
            }
            let len = (((self.buf[i + 1] & 0x03) as usize) << 8) | self.buf[i + 2] as usize;
            let frame_len = 3 + len + 3; // header + payload + CRC-24Q
            if self.buf.len() < i + frame_len {
                break; // rest of the frame hasn't arrived yet
            }
            let frame = &self.buf[i..i + frame_len];
            let computed = crc24q(&frame[..3 + len]);
            let received = ((frame[3 + len] as u32) << 16)
                | ((frame[3 + len + 1] as u32) << 8)
                | frame[3 + len + 2] as u32;
            if computed == received {
                out.push(Frame {
                    payload: frame[3..3 + len].to_vec(),
                    crc_ok: true,
                });
                i += frame_len;
            } else {
                out.push(Frame {
                    payload: Vec::new(),
                    crc_ok: false,
                });
                i += 1; // false preamble — slide forward and try again
            }
        }
        self.buf.drain(..i);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc24q_check_vector() {
        // Canonical CRC-24Q check value (RTKLIB / gpsd) for "123456789".
        assert_eq!(crc24q(b"123456789"), 0x00CD_E703);
    }

    /// Big-endian bit writer, mirror of `Bits`, for building test frames.
    struct BitWriter {
        d: Vec<u8>,
        bit: usize,
    }
    impl BitWriter {
        fn new(bits: usize) -> Self {
            Self {
                d: vec![0u8; bits.div_ceil(8)],
                bit: 0,
            }
        }
        fn put(&mut self, val: u64, len: usize) {
            for i in (0..len).rev() {
                let b = ((val >> i) & 1) as u8;
                let pos = self.bit;
                self.d[pos / 8] |= b << (7 - (pos % 8));
                self.bit += 1;
            }
        }
    }

    /// Wrap a payload in a full RTCM 3 frame (preamble + length + CRC-24Q).
    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut f = vec![
            0xD3,
            ((payload.len() >> 8) & 0x03) as u8,
            (payload.len() & 0xFF) as u8,
        ];
        f.extend_from_slice(payload);
        let crc = crc24q(&f);
        f.extend_from_slice(&[(crc >> 16) as u8, (crc >> 8) as u8, crc as u8]);
        f
    }

    /// Build an RTCM 1005 payload encoding the given ECEF coordinates (metres).
    fn build_1005(station_id: u64, x: f64, y: f64, z: f64) -> Vec<u8> {
        let mut w = BitWriter::new(152);
        w.put(1005, 12); // message number
        w.put(station_id, 12); // DF003 reference station ID
        w.put(0, 6); // DF021 ITRF year
        w.put(0, 4); // GPS/GLO/GAL/ref-station indicators
        w.put((x / 0.0001).round() as i64 as u64 & ((1 << 38) - 1), 38); // DF025 ECEF-X
        w.put(0, 2); // oscillator + reserved
        w.put((y / 0.0001).round() as i64 as u64 & ((1 << 38) - 1), 38); // DF026 ECEF-Y
        w.put(0, 2); // quarter-cycle indicator
        w.put((z / 0.0001).round() as i64 as u64 & ((1 << 38) - 1), 38); // DF027 ECEF-Z
        w.d
    }

    #[test]
    fn parses_1005_and_recovers_ecef() {
        // ECEF for a point near 37.0°N, -122.0°E on the WGS84 ellipsoid.
        let (x, y, z) = (-2_702_584.6, -4_325_039.454, 3_817_393.16);
        let bytes = frame(&build_1005(42, x, y, z));

        let mut d = Deframer::new();
        let frames = d.push(&bytes);
        assert_eq!(frames.len(), 1, "one frame expected");
        assert!(frames[0].crc_ok, "CRC should validate");
        assert_eq!(frames[0].msg_type(), Some(1005));

        let (lat, lon, h) = ecef_to_geodetic(x, y, z);
        assert!((lat - 37.0).abs() < 0.5, "lat ~37°, got {lat}");
        assert!((lon + 122.0).abs() < 0.5, "lon ~-122°, got {lon}");
        assert!(h.abs() < 100.0, "height near ellipsoid, got {h}");

        // The decoded description mentions the station and a position line.
        let desc = describe_message(&frames[0].payload);
        assert!(desc[0].contains("[1005]"));
        assert_eq!(desc.len(), 3);
    }

    #[test]
    fn rejects_corrupt_crc_and_resyncs() {
        let good = frame(&build_1005(1, -2_702_584.0, -4_325_039.0, 3_817_393.0));
        let mut corrupt = good.clone();
        let n = corrupt.len();
        corrupt[n - 1] ^= 0xFF; // smash the CRC

        // Garbage byte, then a corrupt frame, then a good frame.
        let mut bytes = vec![0x00];
        bytes.extend_from_slice(&corrupt);
        bytes.extend_from_slice(&good);

        let mut d = Deframer::new();
        let frames = d.push(&bytes);
        let ok = frames.iter().filter(|f| f.crc_ok).count();
        let bad = frames.iter().filter(|f| !f.crc_ok).count();
        assert_eq!(ok, 1, "only the intact frame should pass");
        assert!(bad >= 1, "corrupt frame should register a CRC failure");
    }

    #[test]
    fn retains_partial_trailing_frame() {
        let bytes = frame(&build_1005(7, -2_702_584.0, -4_325_039.0, 3_817_393.0));
        let split = bytes.len() - 4; // cut mid-frame

        let mut d = Deframer::new();
        let first = d.push(&bytes[..split]);
        assert!(first.is_empty(), "incomplete frame not yet decoded");

        let second = d.push(&bytes[split..]); // remainder arrives
        assert_eq!(second.len(), 1, "frame decoded once complete");
        assert!(second[0].crc_ok);
    }
}
