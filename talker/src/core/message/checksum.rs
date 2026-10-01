//! Per-message outer checksum (spec §5.5, §7).
//!
//! This is independent of any checksum inside the message protocol itself
//! (e.g. an NMEA sentence's own `*XX`): it wraps the entire wire output.

use serde::{Deserialize, Serialize};

/// Checksum algorithm appended to a message's wire output.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumAlgorithm {
    /// XOR of all bytes (1 byte).
    #[default]
    Xor,
    /// CRC-8/SMBUS (1 byte).
    Crc8,
    /// CRC-16/KERMIT: reflected, initial value 0, check value 0x2189 (2 bytes,
    /// high byte first). Some devices call it "CCITT", a name several different
    /// CRC-16 variants share (ADR-057).
    Crc16Kermit,
    /// CRC-16/MODBUS (2 bytes, low byte first, as MODBUS RTU carries it).
    Crc16Modbus,
    /// CRC-32/ISO-HDLC (4 bytes, big-endian).
    Crc32,
}

/// Configuration for the checksum appended to a message.
///
/// The checksum covers the complete wire output before it — the timestamp (if
/// any) and the payload — and is appended as raw bytes: high byte first, except
/// MODBUS, which its protocol carries low byte first (ADR-057). A message
/// carries a checksum only when [`MessageConfig::checksum`] is `Some`.
///
/// [`MessageConfig::checksum`]: super::MessageConfig
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChecksumConfig {
    #[serde(default)]
    pub algorithm: ChecksumAlgorithm,
    /// When true, the appended value is deliberately corrupted (its last byte
    /// is incremented) so a receiver sees a checksum failure — for negative
    /// testing.
    #[serde(default)]
    pub intentionally_wrong: bool,
}

// Const instances so each algorithm's 256-entry lookup table is built at
// compile time. `Crc::new` inside `compute` rebuilt the table on every call —
// once per send for a checksummed message.
const CRC8_SMBUS: crc::Crc<u8> = crc::Crc::<u8>::new(&crc::CRC_8_SMBUS);
const CRC16_KERMIT: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_KERMIT);
const CRC16_MODBUS: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_MODBUS);
const CRC32_ISO_HDLC: crc::Crc<u32> = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

impl ChecksumConfig {
    /// Compute the checksum of `data`, returned as the raw bytes to append.
    /// Bytes [`Self::compute`] appends, which depends only on the algorithm.
    /// Lets a compiled message state its wire length without rendering.
    pub(crate) const fn wire_len(&self) -> usize {
        match self.algorithm {
            ChecksumAlgorithm::Xor | ChecksumAlgorithm::Crc8 => 1,
            ChecksumAlgorithm::Crc16Kermit | ChecksumAlgorithm::Crc16Modbus => 2,
            ChecksumAlgorithm::Crc32 => 4,
        }
    }

    pub fn compute(&self, data: &[u8]) -> Vec<u8> {
        let mut bytes = match self.algorithm {
            ChecksumAlgorithm::Xor => vec![data.iter().fold(0u8, |acc, &b| acc ^ b)],
            ChecksumAlgorithm::Crc8 => vec![CRC8_SMBUS.checksum(data)],
            ChecksumAlgorithm::Crc16Kermit => CRC16_KERMIT.checksum(data).to_be_bytes().to_vec(),
            ChecksumAlgorithm::Crc16Modbus => CRC16_MODBUS.checksum(data).to_le_bytes().to_vec(),
            ChecksumAlgorithm::Crc32 => CRC32_ISO_HDLC.checksum(data).to_be_bytes().to_vec(),
        };
        if self.intentionally_wrong {
            // Corrupt the last byte so the value always differs from the real
            // checksum, whatever its width.
            if let Some(last) = bytes.last_mut() {
                *last = last.wrapping_add(1);
            }
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The standard catalog "check" value of each algorithm is the checksum
    /// of the ASCII string "123456789".
    const CHECK: &[u8] = b"123456789";

    fn cfg(algorithm: ChecksumAlgorithm) -> ChecksumConfig {
        ChecksumConfig {
            algorithm,
            intentionally_wrong: false,
        }
    }

    #[test]
    fn xor_check_value() {
        assert_eq!(cfg(ChecksumAlgorithm::Xor).compute(CHECK), vec![0x31]);
    }

    #[test]
    fn crc8_check_value() {
        assert_eq!(cfg(ChecksumAlgorithm::Crc8).compute(CHECK), vec![0xF4]);
    }

    #[test]
    fn crc16_kermit_check_value() {
        assert_eq!(
            cfg(ChecksumAlgorithm::Crc16Kermit).compute(CHECK),
            vec![0x21, 0x89]
        );
    }

    /// Profiles store the name of what the algorithm computes (ADR-057); the
    /// old "CCITT" name no longer loads.
    #[test]
    fn kermit_is_stored_by_its_own_name() {
        assert_eq!(
            serde_json::to_string(&ChecksumAlgorithm::Crc16Kermit).unwrap(),
            "\"crc16_kermit\""
        );
        assert!(serde_json::from_str::<ChecksumAlgorithm>("\"crc16_ccitt\"").is_err());
    }

    /// MODBUS RTU carries its CRC low byte first (ADR-057): the catalog check
    /// value 0x4B37 goes on the wire as `37 4B`.
    #[test]
    fn crc16_modbus_check_value_is_appended_low_byte_first() {
        assert_eq!(
            cfg(ChecksumAlgorithm::Crc16Modbus).compute(CHECK),
            vec![0x37, 0x4B]
        );
    }

    #[test]
    fn crc32_check_value() {
        assert_eq!(
            cfg(ChecksumAlgorithm::Crc32).compute(CHECK),
            vec![0xCB, 0xF4, 0x39, 0x26]
        );
    }

    #[test]
    fn xor_of_known_bytes() {
        // 0x01 ^ 0x02 ^ 0x03 = 0x00
        assert_eq!(
            cfg(ChecksumAlgorithm::Xor).compute(&[0x01, 0x02, 0x03]),
            vec![0x00]
        );
    }

    #[test]
    fn intentionally_wrong_differs_but_keeps_width() {
        for algo in [
            ChecksumAlgorithm::Xor,
            ChecksumAlgorithm::Crc8,
            ChecksumAlgorithm::Crc16Kermit,
            ChecksumAlgorithm::Crc16Modbus,
            ChecksumAlgorithm::Crc32,
        ] {
            let correct = cfg(algo).compute(CHECK);
            let wrong = ChecksumConfig {
                algorithm: algo,
                intentionally_wrong: true,
            }
            .compute(CHECK);
            assert_eq!(correct.len(), wrong.len());
            assert_ne!(correct, wrong);
        }
    }

    #[test]
    fn checksum_width_per_algorithm() {
        assert_eq!(cfg(ChecksumAlgorithm::Xor).compute(CHECK).len(), 1);
        assert_eq!(cfg(ChecksumAlgorithm::Crc8).compute(CHECK).len(), 1);
        assert_eq!(cfg(ChecksumAlgorithm::Crc16Kermit).compute(CHECK).len(), 2);
        assert_eq!(cfg(ChecksumAlgorithm::Crc16Modbus).compute(CHECK).len(), 2);
        assert_eq!(cfg(ChecksumAlgorithm::Crc32).compute(CHECK).len(), 4);
    }
}
