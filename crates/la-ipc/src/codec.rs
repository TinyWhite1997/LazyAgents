//! 4-byte big-endian length-prefix codec.
//!
//! Wire format (architecture §3 帧格式):
//!
//! ```text
//! +--------+----------------------------------+
//! | u32 BE | UTF-8 JSON-RPC message (≤ 4 MiB) |
//! +--------+----------------------------------+
//! ```
//!
//! Frames whose declared length exceeds [`crate::MAX_MESSAGE_BYTES`] are
//! rejected without buffering — a malicious or buggy peer cannot make the
//! daemon allocate up to 4 GiB by lying in the length field.
//!
//! Empty frames (`length == 0`) are accepted and decoded as a zero-byte
//! payload; the higher-level [`crate::Connection`] layer treats them as a
//! decode error so the JSON-RPC layer doesn't have to.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::{IpcError, LENGTH_PREFIX_BYTES, MAX_MESSAGE_BYTES};

/// Length-prefix codec.
///
/// Cheap to clone (it is zero-sized) and stateless across frames — backpressure
/// comes from the bounded I/O buffer in `tokio_util::Framed`, not from us.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameCodec {
    _priv: (),
}

impl FrameCodec {
    pub fn new() -> Self {
        Self { _priv: () }
    }
}

impl Decoder for FrameCodec {
    type Item = Bytes;
    type Error = IpcError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < LENGTH_PREFIX_BYTES {
            // Hint to the reader how much we want next.
            src.reserve(LENGTH_PREFIX_BYTES - src.len());
            return Ok(None);
        }
        // Peek the length without consuming yet, so we can refuse oversize
        // frames before allocating.
        let mut len_bytes = [0u8; LENGTH_PREFIX_BYTES];
        len_bytes.copy_from_slice(&src[..LENGTH_PREFIX_BYTES]);
        let frame_len = u32::from_be_bytes(len_bytes) as usize;
        if frame_len > MAX_MESSAGE_BYTES {
            // Don't drain the buffer — surface the error and let the caller
            // close the connection.
            return Err(IpcError::FrameTooLarge {
                actual: frame_len,
                limit: MAX_MESSAGE_BYTES,
            });
        }
        if src.len() < LENGTH_PREFIX_BYTES + frame_len {
            // Reserve enough room for the rest of this frame.
            src.reserve(LENGTH_PREFIX_BYTES + frame_len - src.len());
            return Ok(None);
        }
        // Consume the prefix, then split off the payload.
        src.advance(LENGTH_PREFIX_BYTES);
        let payload = src.split_to(frame_len).freeze();
        Ok(Some(payload))
    }
}

impl Encoder<Bytes> for FrameCodec {
    type Error = IpcError;

    fn encode(&mut self, item: Bytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let n = item.len();
        if n > MAX_MESSAGE_BYTES {
            return Err(IpcError::EncodeTooLarge {
                actual: n,
                limit: MAX_MESSAGE_BYTES,
            });
        }
        dst.reserve(LENGTH_PREFIX_BYTES + n);
        dst.put_u32(n as u32); // BufMut::put_u32 is big-endian.
        dst.put_slice(&item);
        Ok(())
    }
}

/// Also accept owned `Vec<u8>` for ergonomics in tests.
impl Encoder<Vec<u8>> for FrameCodec {
    type Error = IpcError;

    fn encode(&mut self, item: Vec<u8>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        <Self as Encoder<Bytes>>::encode(self, Bytes::from(item), dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;

    fn empty_buf() -> BytesMut {
        BytesMut::new()
    }

    #[test]
    fn encode_zero_length_frame() {
        let mut codec = FrameCodec::new();
        let mut buf = empty_buf();
        codec.encode(Bytes::new(), &mut buf).unwrap();
        assert_eq!(&buf[..], &[0, 0, 0, 0]);
    }

    #[test]
    fn round_trip_small() {
        let mut codec = FrameCodec::new();
        let mut buf = empty_buf();
        codec.encode(Bytes::from_static(b"hello"), &mut buf).unwrap();
        let frame = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(&frame[..], b"hello");
        assert!(buf.is_empty(), "decoder should drain a complete frame");
    }

    #[test]
    fn round_trip_four_mib_boundary_ok() {
        let mut codec = FrameCodec::new();
        let payload = vec![0xAAu8; MAX_MESSAGE_BYTES];
        let mut buf = empty_buf();
        codec.encode(Bytes::from(payload.clone()), &mut buf).unwrap();
        let frame = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(frame.len(), payload.len());
        assert_eq!(&frame[..], &payload[..]);
    }

    #[test]
    fn encode_rejects_above_cap() {
        let mut codec = FrameCodec::new();
        let payload = vec![0u8; MAX_MESSAGE_BYTES + 1];
        let mut buf = empty_buf();
        let err = codec.encode(Bytes::from(payload), &mut buf).unwrap_err();
        match err {
            IpcError::EncodeTooLarge { actual, limit } => {
                assert_eq!(actual, MAX_MESSAGE_BYTES + 1);
                assert_eq!(limit, MAX_MESSAGE_BYTES);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_above_cap_without_allocation() {
        let mut codec = FrameCodec::new();
        let mut buf = empty_buf();
        buf.put_u32((MAX_MESSAGE_BYTES as u32) + 1); // declared length, no body
        let err = codec.decode(&mut buf).unwrap_err();
        match err {
            IpcError::FrameTooLarge { actual, limit } => {
                assert_eq!(actual, MAX_MESSAGE_BYTES + 1);
                assert_eq!(limit, MAX_MESSAGE_BYTES);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn decode_partial_then_complete() {
        let mut codec = FrameCodec::new();
        let mut buf = empty_buf();

        // First feed: only prefix.
        buf.put_u32(5);
        assert!(codec.decode(&mut buf).unwrap().is_none());

        // Then feed the body in two pieces.
        buf.extend_from_slice(b"he");
        assert!(codec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b"llo");
        let frame = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(&frame[..], b"hello");
    }

    #[test]
    fn decode_multiple_frames_back_to_back() {
        let mut codec = FrameCodec::new();
        let mut buf = empty_buf();
        codec.encode(Bytes::from_static(b"a"), &mut buf).unwrap();
        codec.encode(Bytes::from_static(b"bb"), &mut buf).unwrap();
        codec.encode(Bytes::from_static(b"ccc"), &mut buf).unwrap();
        let f1 = codec.decode(&mut buf).unwrap().unwrap();
        let f2 = codec.decode(&mut buf).unwrap().unwrap();
        let f3 = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(&f1[..], b"a");
        assert_eq!(&f2[..], b"bb");
        assert_eq!(&f3[..], b"ccc");
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }
}
