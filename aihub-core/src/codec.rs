use crate::ipc::IpcMessage;
use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;
use tokio_util::codec::{Decoder, Encoder};

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

/// Header size (bytes) of a `PtyBinaryFrame`: 8-byte offset + 4-byte length.
const PTY_BINARY_FRAME_HEADER_LEN: usize = 12;

/// Binary frame for the dedicated PTY data channel (ADR §5, §2.2): `[8-byte
/// big-endian stream_offset][4-byte big-endian length][raw bytes]`.
///
/// Distinct from the JSON/`Base64Bytes` control-channel encoding: this is the "no more
/// Base64 on the PTY wire" format, used only once a `PtyChannelHello` has authenticated
/// the secondary connection. The legacy JSON path (`Base64Bytes` in `ClientMessage`/
/// `DaemonMessage`) is unchanged for the local Unix socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyBinaryFrame {
    pub stream_offset: u64,
    pub data: Vec<u8>,
}

impl PtyBinaryFrame {
    pub fn new(stream_offset: u64, data: impl Into<Vec<u8>>) -> Self {
        Self {
            stream_offset,
            data: data.into(),
        }
    }

    /// Encodes this frame into its wire representation.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PTY_BINARY_FRAME_HEADER_LEN + self.data.len());
        out.extend_from_slice(&self.stream_offset.to_be_bytes());
        out.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// Decodes a frame from the start of `buf`. Returns the frame and the number of
    /// bytes consumed, or `None` if `buf` does not yet contain a complete frame.
    pub fn decode(buf: &[u8]) -> Option<(Self, usize)> {
        if buf.len() < PTY_BINARY_FRAME_HEADER_LEN {
            return None;
        }
        let mut offset_bytes = [0u8; 8];
        offset_bytes.copy_from_slice(&buf[0..8]);
        let stream_offset = u64::from_be_bytes(offset_bytes);

        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&buf[8..12]);
        let data_len = u32::from_be_bytes(len_bytes) as usize;

        let total_len = PTY_BINARY_FRAME_HEADER_LEN + data_len;
        if buf.len() < total_len {
            return None;
        }
        let data = buf[PTY_BINARY_FRAME_HEADER_LEN..total_len].to_vec();
        Some((
            Self {
                stream_offset,
                data,
            },
            total_len,
        ))
    }
}
