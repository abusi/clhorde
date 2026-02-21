//! Wire framing and socket path resolution for IPC.

use std::io::{self, Read};
use std::path::PathBuf;

/// Default daemon socket path: `~/.local/share/clhorde/daemon.sock`
pub fn daemon_socket_path() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp/clhorde"))
        .join("daemon.sock")
}

/// Default daemon PID file path: `~/.local/share/clhorde/daemon.pid`
pub fn daemon_pid_path() -> PathBuf {
    crate::config::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp/clhorde"))
        .join("daemon.pid")
}

/// Maximum frame payload size (16 MiB).
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Marker byte that distinguishes binary PTY frames from JSON text frames.
pub const PTY_FRAME_MARKER: u8 = 0x01;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    TooLarge(usize),
    InvalidUtf8,
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "IO error: {e}"),
            FrameError::TooLarge(n) => write!(f, "Frame too large: {n} bytes"),
            FrameError::InvalidUtf8 => write!(f, "Invalid UTF-8 in frame"),
        }
    }
}

/// Encode: 4-byte big-endian length + payload.
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Decode one frame from a reader. Returns payload bytes.
pub fn decode_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, FrameError> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        return Err(FrameError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

/// Encode a PTY binary frame: marker byte + 4-byte prompt_id (big-endian) + raw bytes.
pub fn encode_pty_frame(prompt_id: usize, data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + 4 + data.len());
    payload.push(PTY_FRAME_MARKER);
    payload.extend_from_slice(&(prompt_id as u32).to_be_bytes());
    payload.extend_from_slice(data);
    payload
}

/// Decode a PTY frame payload. Returns (prompt_id, bytes).
///
/// The input should be the raw payload (without the outer length-delimited wrapper).
pub fn decode_pty_frame(payload: &[u8]) -> Result<(usize, Vec<u8>), FrameError> {
    // Minimum: 1 byte marker + 4 bytes prompt_id
    if payload.len() < 5 {
        return Err(FrameError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "PTY frame too short",
        )));
    }
    let prompt_id = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]) as usize;
    let data = payload[5..].to_vec();
    Ok((prompt_id, data))
}

/// Check if a decoded frame payload is a binary PTY frame.
pub fn is_binary_frame(payload: &[u8]) -> bool {
    !payload.is_empty() && payload[0] == PTY_FRAME_MARKER
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_frame() {
        let data = b"hello world";
        let encoded = encode_frame(data);
        let mut cursor = Cursor::new(encoded);
        let decoded = decode_frame(&mut cursor).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn roundtrip_pty_frame() {
        let prompt_id = 42;
        let data = b"raw pty bytes";
        let payload = encode_pty_frame(prompt_id, data);
        assert!(is_binary_frame(&payload));
        let (decoded_id, decoded_data) = decode_pty_frame(&payload).unwrap();
        assert_eq!(decoded_id, prompt_id);
        assert_eq!(decoded_data, data);
    }

    #[test]
    fn empty_frame() {
        let encoded = encode_frame(b"");
        let mut cursor = Cursor::new(encoded);
        let decoded = decode_frame(&mut cursor).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn too_large_frame() {
        let len = (MAX_FRAME_SIZE + 1) as u32;
        let buf = len.to_be_bytes();
        let mut cursor = Cursor::new(buf.to_vec());
        assert!(matches!(
            decode_frame(&mut cursor),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn non_pty_frame_not_binary() {
        let payload = b"{\"type\":\"json\"}";
        assert!(!is_binary_frame(payload));
    }
}
