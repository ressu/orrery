//! Klippy unix socket framing.
//!
//! Klippy's webhooks server (klippy/webhooks.py in Klipper) reads and writes JSON objects, each
//! followed by a single ETX byte (`0x03`). Requests carry `id`, `method` and `params`. Responses
//! carry the request's `id` and either `result` or `error`. Messages Klippy sends on its own
//! (status updates, gcode output, remote method calls) have `method` and `params` but no `id`.

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Byte that ends every message on the Klippy socket.
pub const TERMINATOR: u8 = 0x03;

/// Serializes `msg` and appends the terminator.
pub fn encode(msg: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec(msg).expect("serde_json::Value always serializes");
    out.push(TERMINATOR);
    out
}

/// Writes one framed message.
pub async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, msg: &Value) -> std::io::Result<()> {
    writer.write_all(&encode(msg)).await?;
    writer.flush().await
}

/// Reads one framed message. Returns `Ok(None)` when the peer closes the socket.
///
/// A frame that isn't valid JSON is returned as an `InvalidData` error; the stream stays usable
/// for the next frame, matching Klippy, which logs and skips undecodable requests.
pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Value>> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(TERMINATOR, &mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        if buf.last() != Some(&TERMINATOR) {
            // EOF in the middle of a frame: treat the partial frame as a closed connection.
            return Ok(None);
        }
        buf.pop();
        if buf.is_empty() {
            continue;
        }
        return serde_json::from_slice(&buf)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn round_trips_several_frames() {
        let mut wire = Vec::new();
        wire.extend(encode(&json!({"id": 1, "method": "info", "params": {}})));
        wire.extend(encode(&json!({"id": 2, "result": {"state": "ready"}})));
        let mut reader = tokio::io::BufReader::new(&wire[..]);
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap()["method"], "info");
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap()["result"]["state"], "ready");
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bad_frame_does_not_poison_stream() {
        let mut wire = b"not json\x03".to_vec();
        wire.extend(encode(&json!({"id": 3})));
        let mut reader = tokio::io::BufReader::new(&wire[..]);
        assert!(read_message(&mut reader).await.is_err());
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap()["id"], 3);
    }
}
