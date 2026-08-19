//! A dedicated, raw connection to the daemon for one `AttachTerminal` stream.
//!
//! Deliberately **not** built on `factoryctl::Client::attach_terminal`: that method hands back an
//! iterator wrapping a `BufReader<UnixStream>` with no way to reach the underlying socket from
//! outside it, so there is no way to make a blocked read return early on detach. `Pane`'s
//! daemon-attached backend needs exactly that — "detach without stopping the worker" (design
//! brief) should mean the socket closes *promptly* when the operator leaves AGENT, not
//! "leaks a thread and a file descriptor until the whole client process exits". Connecting
//! directly here keeps a `UnixStream` (or a `try_clone` of one) in `Pane`'s hands so its `detach`
//! can call `shutdown()` on it, which unblocks the reader thread's blocking `read()` immediately.
//!
//! This duplicates a small amount of the newline-delimited-JSON framing `factoryctl::Client`
//! already implements privately. That's an accepted, documented tradeoff (see `pane.rs`) rather
//! than a factoryctl change — this track's scope is `crates/factory-tui/**` only.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use factory_core::local::{LocalRequest, MAX_LOCAL_FRAME_BYTES, RequestEnvelope, ServerFrame};

/// One open `AttachTerminal` connection. Dropping this (or calling [`AttachConnection::shutdown`]
/// explicitly, which is faster) ends the paired reader's blocking read.
pub struct AttachConnection {
    stream: UnixStream,
}

impl AttachConnection {
    /// Connects to `socket`, sends `request` (expected to be an `AttachTerminal`, though this
    /// doesn't enforce that — the daemon will reject anything else with an error response, which
    /// the caller's frame loop surfaces like any other frame), and returns both a handle for
    /// shutting the connection down and a second handle for reading frames from it.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the socket can't be connected to, cloned, or written to.
    pub fn open(socket: &Path, request: LocalRequest) -> io::Result<(Self, UnixStream)> {
        let mut stream = UnixStream::connect(socket)?;
        write_request(&mut stream, &request)?;
        let reader_half = stream.try_clone()?;
        Ok((Self { stream }, reader_half))
    }

    /// Shuts both directions of the socket down, unblocking any in-progress `read()` on the
    /// paired reader handle. Safe to call more than once or after the peer already closed.
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

fn write_request(writer: &mut impl Write, request: &LocalRequest) -> io::Result<()> {
    let envelope = RequestEnvelope::new(request.clone());
    let payload = serde_json::to_vec(&envelope)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    writer.write_all(&payload)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Reads one newline-delimited `ServerFrame` from `reader`, or `Ok(None)` on a clean EOF (the
/// peer closed the connection, or [`AttachConnection::shutdown`] was called). Mirrors
/// `factoryctl::Client`'s private frame-reading loop closely enough to stay wire-compatible, but
/// is independently implemented (and tested) here — see the module doc comment for why.
///
/// # Errors
///
/// Returns an [`io::Error`] on a read failure, an over-size frame, or invalid JSON.
pub fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<ServerFrame>> {
    let mut payload = Vec::new();
    loop {
        let (consumed, complete) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if payload.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "frame ended before newline",
                ));
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            let content_len = newline.unwrap_or(available.len());
            if payload.len() + content_len > MAX_LOCAL_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame exceeds max size",
                ));
            }
            payload.extend_from_slice(&available[..content_len]);
            (consumed, newline.is_some())
        };
        reader.consume(consumed);
        if complete {
            break;
        }
    }
    if payload.last() == Some(&b'\r') {
        payload.pop();
    }
    let frame = serde_json::from_slice(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Some(frame))
}

/// Reads frames from `reader` until EOF or an error, calling `on_frame` for each one. Used by
/// `pane.rs`'s daemon-attach reader thread; split out so the loop itself (as opposed to what it
/// does with each frame) is unit-testable without a real socket.
pub fn read_frames(reader: impl Read, mut on_frame: impl FnMut(ServerFrame) -> bool) {
    let mut buffered = BufReader::new(reader);
    loop {
        match read_frame(&mut buffered) {
            Ok(Some(frame)) => {
                if !on_frame(frame) {
                    return;
                }
            }
            Ok(None) | Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory_core::runner::encode_terminal_bytes;
    use factory_core::{ProjectId, SessionId};

    fn frame_line(session_id: &str, offset: u64, bytes: &[u8]) -> Vec<u8> {
        let frame = ServerFrame::TerminalOutput {
            protocol_version: factory_core::PROTOCOL_VERSION,
            session_id: SessionId::try_from(session_id).unwrap(),
            offset,
            bytes: encode_terminal_bytes(bytes),
        };
        let mut line = serde_json::to_vec(&frame).unwrap();
        line.push(b'\n');
        line
    }

    fn ready_line(session_id: &str) -> Vec<u8> {
        frame_line(session_id, 0, b"")
    }

    #[test]
    fn empty_replay_has_an_explicit_ready_boundary_before_output() {
        let mut bytes = ready_line("sess-1");
        bytes.extend(frame_line("sess-1", 0, b"later"));
        let mut frames = Vec::new();
        read_frames(bytes.as_slice(), |frame| {
            frames.push(frame);
            true
        });
        assert!(matches!(
            &frames[0],
            ServerFrame::TerminalOutput { bytes, .. } if bytes.is_empty()
        ));
        assert!(matches!(frames[1], ServerFrame::TerminalOutput { .. }));
    }

    #[test]
    fn read_frame_decodes_a_terminal_output_frame() {
        let bytes = frame_line("sess-1", 0, b"hello");
        let mut reader = BufReader::new(bytes.as_slice());
        let frame = read_frame(&mut reader).unwrap().unwrap();
        match frame {
            ServerFrame::TerminalOutput {
                session_id,
                offset,
                bytes,
                ..
            } => {
                assert_eq!(session_id.as_str(), "sess-1");
                assert_eq!(offset, 0);
                assert_eq!(
                    factory_core::runner::decode_terminal_bytes(&bytes).unwrap(),
                    b"hello"
                );
            }
            other => panic!("expected TerminalOutput, got {other:?}"),
        }
    }

    #[test]
    fn read_frame_handles_a_frame_split_across_reads() {
        // A `BufRead` whose `fill_buf` naturally chunks small reads exercises the same
        // "wait for more bytes" path the real socket-backed reader hits.
        struct Chunked<'a> {
            chunks: std::collections::VecDeque<&'a [u8]>,
        }
        impl Read for Chunked<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if let Some(chunk) = self.chunks.pop_front() {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    Ok(n)
                } else {
                    Ok(0)
                }
            }
        }
        let bytes = frame_line("sess-1", 5, b"world");
        let mid = bytes.len() / 2;
        let chunks = Chunked {
            chunks: std::collections::VecDeque::from(vec![&bytes[..mid], &bytes[mid..]]),
        };
        let mut reader = BufReader::new(chunks);
        let frame = read_frame(&mut reader).unwrap().unwrap();
        assert!(matches!(
            frame,
            ServerFrame::TerminalOutput { offset: 5, .. }
        ));
    }

    #[test]
    fn read_frame_returns_none_on_clean_eof() {
        let mut reader = BufReader::new(&b""[..]);
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn read_frame_errors_on_a_truncated_frame() {
        let mut bytes = frame_line("sess-1", 0, b"hi");
        bytes.pop(); // drop the trailing newline
        let mid = bytes.len() - 3;
        let mut reader = BufReader::new(&bytes[..mid]);
        assert!(read_frame(&mut reader).is_err());
    }

    #[test]
    fn read_frames_calls_back_for_every_frame_in_order() {
        let mut all = frame_line("sess-1", 0, b"a");
        all.extend(frame_line("sess-1", 1, b"b"));
        let mut seen: Vec<u64> = Vec::new();
        read_frames(all.as_slice(), |frame| {
            if let ServerFrame::TerminalOutput { offset, .. } = frame {
                seen.push(offset);
            }
            true
        });
        assert_eq!(seen, vec![0, 1]);
    }

    #[test]
    fn read_frames_stops_early_when_the_callback_returns_false() {
        let mut all = frame_line("sess-1", 0, b"a");
        all.extend(frame_line("sess-1", 1, b"b"));
        let mut count = 0;
        read_frames(all.as_slice(), |_| {
            count += 1;
            false
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn request_round_trips_through_write_and_read_frame() {
        // Exercises `write_request` against an in-memory pipe-like pair, confirming the
        // envelope this connection sends is exactly what a well-formed `AttachTerminal`
        // request should look like on the wire.
        let mut buf = Vec::new();
        let request = LocalRequest::AttachTerminal {
            project_id: ProjectId::try_from("proj").unwrap(),
            session_id: SessionId::try_from("sess-1").unwrap(),
            since_offset: 0,
            mode: factory_core::runner::TerminalAttachMode::Tail,
        };
        write_request(&mut buf, &request).unwrap();
        assert_eq!(buf.last(), Some(&b'\n'));
        let envelope: RequestEnvelope = serde_json::from_slice(&buf[..buf.len() - 1]).unwrap();
        assert_eq!(envelope.request, request);
    }
}
