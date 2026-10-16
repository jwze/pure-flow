// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Pure ETW schema helpers, tested on either host OS. No guessed payload offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    Send,
    Receive,
}
pub(super) fn direction(opcode: u8) -> Option<Direction> {
    match opcode {
        10 | 26 => Some(Direction::Send),
        11 | 27 => Some(Direction::Receive),
        _ => None, // Connections, copies, retransmits, failures are not extra transfers.
    }
}
pub(super) fn scalar(bytes: &[u8], expected_size: usize) -> Result<u64, String> {
    match (expected_size, bytes.len()) {
        (4, 4) => Ok(u32::from_le_bytes(bytes.try_into().map_err(|_| "invalid u32")?) as u64),
        (8, 8) => Ok(u64::from_le_bytes(
            bytes.try_into().map_err(|_| "invalid u64")?,
        )),
        _ => Err("ETW scalar property size/type mismatch".into()),
    }
}
pub(super) fn utf16_name(buffer: &[u8], offset: usize) -> Option<String> {
    if !offset.is_multiple_of(2) || offset >= buffer.len() {
        return None;
    }
    let mut words = Vec::new();
    for pair in buffer[offset..].as_chunks::<2>().0 {
        let word = u16::from_le_bytes([pair[0], pair[1]]);
        if word == 0 {
            return String::from_utf16(&words).ok();
        }
        words.push(word);
    }
    None // Missing terminator cannot be passed as a PCWSTR to TDH.
}
pub(super) fn event_in_lifetime(timestamp: i64, birth: u64, exit: u64) -> bool {
    timestamp > 0
        && birth > 0
        && birth <= timestamp as u64
        && (exit == 0 || timestamp as u64 <= exit)
}
