//! CRC-16/GENIBUS, the frame check used by INTERLNK.
//!
//! Polynomial 0x1021, initial value 0xFFFF, not reflected, output
//! complemented. A frame carries it high byte first after the length and
//! data it covers.

/// CRC-16/GENIBUS of `data`.
pub fn crc16(data: &[u8]) -> u16 {
    !register(0xFFFF, data)
}

/// True if `frame` (length, data, CRC) carries a correct CRC.
pub fn frame_ok(frame: &[u8]) -> bool {
    frame.len() >= 3 && register(0xFFFF, frame) == RESIDUE
}

/// What the register holds after a whole frame, CRC included.
const RESIDUE: u16 = 0x1D0F;

fn register(mut crc: u16, data: &[u8]) -> u16 {
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                crc << 1 ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_value() {
        assert_eq!(crc16(b"123456789"), 0xD64E);
    }

    #[test]
    fn captured_frame() {
        // The client's `00 00` SERVER-INFO announcement.
        let frame = [0x02, 0x00, 0x00, 0x5D, 0x03];
        assert_eq!(crc16(&frame[..3]), 0x5D03);
        assert!(frame_ok(&frame));
        assert!(!frame_ok(&[0x02, 0x00, 0x01, 0x5D, 0x03]));
        assert!(!frame_ok(&[0x5D, 0x03]));
    }
}
