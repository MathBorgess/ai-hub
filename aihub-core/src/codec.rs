use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;
use tokio_util::codec::{Decoder, Encoder};
use crate::ipc::IpcMessage;

/// Default maximum allowed frame length (32 MiB), comfortably accommodating 1+ MiB PTY chunks.
pub const DEFAULT_MAX_FRAME_LENGTH: usize = 32 * 1024 * 1024;

/// Errors that can occur during IPC frame encoding/decoding.
#[derive(Debug, Error)]
pub enum IpcCodecError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization/deserialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Frame length {length} exceeds maximum allowed {max}")]
    FrameTooLarge { length: usize, max: usize },
}

/// Tokio codec for length-delimited JSON IPC framing.
///
/// Frames use a 4-byte big-endian unsigned integer length prefix followed by
/// the JSON-serialized payload of `IpcMessage`.
#[derive(Debug, Clone)]
pub struct IpcCodec {
    max_frame_length: usize,
}

impl IpcCodec {
    /// Creates a new `IpcCodec` with custom maximum frame size limit.
    pub fn new(max_frame_length: usize) -> Self {
        Self { max_frame_length }
    }

    /// Maximum frame length supported by this codec instance.
    pub fn max_frame_length(&self) -> usize {
        self.max_frame_length
    }
}

impl Default for IpcCodec {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_LENGTH)
    }
}

impl Encoder<IpcMessage> for IpcCodec {
    type Error = IpcCodecError;

    fn encode(&mut self, item: IpcMessage, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let json_bytes = serde_json::to_vec(&item)?;
        let len = json_bytes.len();
        if len > self.max_frame_length {
            return Err(IpcCodecError::FrameTooLarge {
                length: len,
                max: self.max_frame_length,
            });
        }

        dst.reserve(4 + len);
        dst.put_u32(len as u32);
        dst.extend_from_slice(&json_bytes);
        Ok(())
    }
}

impl Decoder for IpcCodec {
    type Item = IpcMessage;
    type Error = IpcCodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 4 {
            return Ok(None);
        }

        // Peek length prefix
        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&src[..4]);
        let frame_len = u32::from_be_bytes(len_bytes) as usize;

        if frame_len > self.max_frame_length {
            return Err(IpcCodecError::FrameTooLarge {
                length: frame_len,
                max: self.max_frame_length,
            });
        }

        if src.len() < 4 + frame_len {
            // Wait for full frame bytes to arrive
            src.reserve((4 + frame_len) - src.len());
            return Ok(None);
        }

        // Consume header
        src.advance(4);
        // Extract payload
        let payload = src.split_to(frame_len);
        let msg = serde_json::from_slice(&payload)?;
        Ok(Some(msg))
    }
}

/// Standalone synchronous helper to encode an `IpcMessage` into a framed byte vector.
pub fn encode_frame(msg: &IpcMessage) -> Result<Vec<u8>, IpcCodecError> {
    let json_bytes = serde_json::to_vec(msg)?;
    let len = json_bytes.len();
    if len > DEFAULT_MAX_FRAME_LENGTH {
        return Err(IpcCodecError::FrameTooLarge {
            length: len,
            max: DEFAULT_MAX_FRAME_LENGTH,
        });
    }
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    out.extend_from_slice(&json_bytes);
    Ok(out)
}

/// Standalone synchronous helper to decode a framed byte buffer into an `IpcMessage`.
/// Returns the message and number of consumed bytes.
pub fn decode_frame(buf: &[u8]) -> Result<Option<(IpcMessage, usize)>, IpcCodecError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&buf[..4]);
    let frame_len = u32::from_be_bytes(len_bytes) as usize;

    if frame_len > DEFAULT_MAX_FRAME_LENGTH {
        return Err(IpcCodecError::FrameTooLarge {
            length: frame_len,
            max: DEFAULT_MAX_FRAME_LENGTH,
        });
    }

    if buf.len() < 4 + frame_len {
        return Ok(None);
    }

    let payload = &buf[4..4 + frame_len];
    let msg: IpcMessage = serde_json::from_slice(payload)?;
    Ok(Some((msg, 4 + frame_len)))
}
