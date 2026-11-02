//! Fixed 8-byte sample record, little endian. Used for on-disk storage and for
//! `GET /api/v1/plants/{key}/samples?format=bin`.
//!
//! ```text
//! offset  size  field
//! 0       4     t              u32  unix seconds, UTC (valid until 2106)
//! 4       2     moisture_cpct  u16  moisture in 0.01 % (0..=10000)
//! 6       1     rssi           i8   node Wi-Fi RSSI in dBm at poll time (0 = unknown)
//! 7       1     flags          u8   see [`flags`]
//! ```
//!
//! A stream or file may start with the 8-byte [`MAGIC`] header, which decoders skip.

pub const RECORD_LEN: usize = 8;

/// File / stream header. The last byte is the format version.
pub const MAGIC: [u8; RECORD_LEN] = *b"PLNTREC\x01";

pub mod flags {
    /// The node's uptime went backwards before this sample (it rebooted).
    pub const NODE_REBOOT: u8 = 1 << 0;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleRecord {
    pub t: u32,
    pub moisture_cpct: u16,
    pub rssi: i8,
    pub flags: u8,
}

impl SampleRecord {
    pub fn new(t: u32, moisture_pct: f32, rssi: i8, flags: u8) -> Self {
        let cpct = moisture_pct.clamp(0.0, 100.0) * 100.0 + 0.5;
        Self {
            t,
            moisture_cpct: cpct as u16,
            rssi,
            flags,
        }
    }

    pub fn moisture_pct(&self) -> f32 {
        f32::from(self.moisture_cpct) / 100.0
    }

    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut b = [0u8; RECORD_LEN];
        b[0..4].copy_from_slice(&self.t.to_le_bytes());
        b[4..6].copy_from_slice(&self.moisture_cpct.to_le_bytes());
        b[6] = self.rssi as u8;
        b[7] = self.flags;
        b
    }

    pub fn decode(b: &[u8; RECORD_LEN]) -> Self {
        Self {
            t: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            moisture_cpct: u16::from_le_bytes([b[4], b[5]]),
            rssi: b[6] as i8,
            flags: b[7],
        }
    }

    /// Decodes a byte stream, skipping a leading [`MAGIC`] and any trailing partial record.
    pub fn decode_all(bytes: &[u8]) -> impl Iterator<Item = SampleRecord> + '_ {
        let body = bytes.strip_prefix(&MAGIC[..]).unwrap_or(bytes);
        body.as_chunks::<RECORD_LEN>().0.iter().map(Self::decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let r = SampleRecord::new(1_793_000_000, 42.137, -61, flags::NODE_REBOOT);
        assert_eq!(r.moisture_cpct, 4214);
        assert_eq!(SampleRecord::decode(&r.encode()), r);
    }

    #[test]
    fn clamps() {
        assert_eq!(SampleRecord::new(0, -3.0, 0, 0).moisture_cpct, 0);
        assert_eq!(SampleRecord::new(0, 140.0, 0, 0).moisture_cpct, 10000);
    }

    #[test]
    fn decode_all_skips_header_and_tail() {
        let a = SampleRecord::new(10, 50.0, -50, 0);
        let b = SampleRecord::new(20, 49.5, -51, 0);
        let mut buf = [0u8; 8 * 3 + 3];
        buf[0..8].copy_from_slice(&MAGIC);
        buf[8..16].copy_from_slice(&a.encode());
        buf[16..24].copy_from_slice(&b.encode());
        let got: [SampleRecord; 2] = {
            let mut it = SampleRecord::decode_all(&buf);
            [it.next().unwrap(), it.next().unwrap()]
        };
        assert_eq!(got, [a, b]);
        assert_eq!(SampleRecord::decode_all(&buf).count(), 2);
    }
}
