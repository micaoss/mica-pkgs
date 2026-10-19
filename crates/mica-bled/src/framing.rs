//! Messages on the GATT byte streams (protocol section 2): a 4-byte big-endian
//! length, then that many bytes; chunks cut at arbitrary points.

/// The largest message either side may send.
pub const MAX_MESSAGE: usize = 65536;

/// A length prefix outside `1..=MAX_MESSAGE`.
#[derive(Debug, PartialEq, Eq)]
pub struct BadLength(pub u32);

impl std::fmt::Display for BadLength {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "message length {} is outside 1..={MAX_MESSAGE}", self.0)
    }
}

impl std::error::Error for BadLength {}

/// Reassembles messages from the chunks of one direction.
#[derive(Debug, Default)]
pub struct Reassembler {
    buf: Vec<u8>,
}

impl Reassembler {
    /// Append a chunk and return every message it completes. After an error
    /// the stream is unusable and the connection must close.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, BadLength> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some(header) = self.buf.first_chunk::<4>() else {
                return Ok(out);
            };
            let len = u32::from_be_bytes(*header);
            let size = usize::try_from(len).unwrap_or(usize::MAX);
            if size == 0 || size > MAX_MESSAGE {
                return Err(BadLength(len));
            }
            if self.buf.len() < 4 + size {
                return Ok(out);
            }
            out.push(self.buf[4..4 + size].to_vec());
            self.buf.drain(..4 + size);
        }
    }
}

/// Prefix a message with its length.
pub fn encode(message: &[u8]) -> Result<Vec<u8>, BadLength> {
    let len = u32::try_from(message.len()).unwrap_or(u32::MAX);
    if message.is_empty() || message.len() > MAX_MESSAGE {
        return Err(BadLength(len));
    }
    let mut out = Vec::with_capacity(4 + message.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(message);
    Ok(out)
}

/// Cut an encoded stream into notification chunks of at most `mtu - 3` bytes.
pub fn chunks(stream: &[u8], mtu: u16) -> std::slice::Chunks<'_, u8> {
    stream.chunks(usize::from(mtu.max(23)) - 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_split_anywhere_reassembles() {
        let stream = [encode(b"hello").unwrap(), encode(b"world!").unwrap()].concat();
        for cut in 0..=stream.len() {
            let mut r = Reassembler::default();
            let mut got = r.push(&stream[..cut]).unwrap();
            got.extend(r.push(&stream[cut..]).unwrap());
            assert_eq!(
                got,
                vec![b"hello".to_vec(), b"world!".to_vec()],
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn byte_by_byte_reassembles() {
        let stream = encode(&[7; 300]).unwrap();
        let mut r = Reassembler::default();
        let mut got = Vec::new();
        for b in &stream {
            got.extend(r.push(&[*b]).unwrap());
        }
        assert_eq!(got, vec![vec![7; 300]]);
    }

    #[test]
    fn zero_and_oversized_lengths_are_refused() {
        assert_eq!(
            Reassembler::default().push(&[0, 0, 0, 0]),
            Err(BadLength(0))
        );
        assert_eq!(
            Reassembler::default().push(&[0, 1, 0, 1]),
            Err(BadLength(65537))
        );
        assert!(
            Reassembler::default()
                .push(&[0, 1, 0, 0])
                .unwrap()
                .is_empty()
        );
        assert!(encode(b"").is_err());
        assert!(encode(&vec![0; MAX_MESSAGE + 1]).is_err());
        assert!(encode(&vec![0; MAX_MESSAGE]).is_ok());
    }

    #[test]
    fn chunks_respect_the_mtu() {
        let stream = vec![1; 100];
        assert!(chunks(&stream, 23).all(|c| c.len() <= 20));
        assert_eq!(chunks(&stream, 103).count(), 1);
        assert!(chunks(&stream, 0).all(|c| c.len() <= 20));
    }
}
