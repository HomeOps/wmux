//! Wire protocol for wmux.
//!
//! Every message is a length-prefixed frame:
//!
//! ```text
//! +--------+------------------+-----------------+
//! | tag u8 | payload_len u32  | payload         |
//! +--------+------------------+-----------------+
//!            little-endian       payload_len bytes
//! ```
//!
//! The framing is hand-rolled on purpose: it keeps the dependency surface
//! small and makes the byte layout obvious when debugging with a pipe sniffer.

use std::io::{self, Read, Write};

/// Largest payload we will accept in a single frame. Guards against a
/// corrupt or hostile length prefix causing a huge allocation.
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024;

mod tag {
    // client -> server
    pub const ATTACH: u8 = 0x01;
    pub const INPUT: u8 = 0x02;
    pub const RESIZE: u8 = 0x03;
    pub const DETACH: u8 = 0x04;
    pub const KILL: u8 = 0x05;
    pub const INFO: u8 = 0x06;
    pub const CAPTURE: u8 = 0x07;
    pub const DETACH_CLIENTS: u8 = 0x08;

    // server -> client
    pub const REPAINT: u8 = 0x81;
    pub const OUTPUT: u8 = 0x82;
    pub const EXITED: u8 = 0x83;
    pub const INFO_REPLY: u8 = 0x84;
    pub const CAPTURE_REPLY: u8 = 0x85;
    pub const DETACHED: u8 = 0x86;
}

/// A message sent from an attaching client to the session server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    /// Attach to the session at the given terminal size.
    Attach { cols: u16, rows: u16 },
    /// Keyboard input destined for the pty.
    Input(Vec<u8>),
    /// The client's terminal changed size.
    Resize { cols: u16, rows: u16 },
    /// Detach this client, leaving the session running.
    Detach,
    /// Terminate the session and its child process.
    Kill,
    /// Request session metadata without attaching.
    Info,
    /// Request the rendered screen as plain text, without attaching.
    ///
    /// This is what makes a session usable from a program that has no console
    /// of its own: send input, then read back what the shell put on screen.
    Capture,
    /// Detach every attached client, from outside the session.
    ///
    /// The escape hatch for when the prefix key is unavailable: bound by the
    /// terminal emulator, remapped, or missing from the keyboard entirely.
    DetachClients,
}

/// A message sent from the session server to an attached client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    /// Full screen redraw, sent once immediately after a successful attach.
    Repaint(Vec<u8>),
    /// Live pty output.
    Output(Vec<u8>),
    /// The child process exited with this code; the session is going away.
    Exited(i32),
    /// Reply to [`ClientMsg::Info`].
    InfoReply {
        cols: u16,
        rows: u16,
        clients: u32,
        command: String,
    },
    /// Reply to [`ClientMsg::Capture`]: the visible screen, newline separated,
    /// with escape sequences already resolved.
    CaptureReply(String),
    /// The server is detaching this client; the session keeps running.
    Detached,
}

fn write_frame<W: Write>(w: &mut W, tag: u8, payload: &[u8]) -> io::Result<()> {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    let mut header = [0u8; 5];
    header[0] = tag;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&header)?;
    w.write_all(payload)?;
    w.flush()
}

fn read_frame<R: Read>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame payload of {len} bytes exceeds the {MAX_PAYLOAD} byte limit"),
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn dims(payload: &[u8]) -> io::Result<(u16, u16)> {
    if payload.len() < 4 {
        return Err(bad("expected 4 bytes of terminal dimensions"));
    }
    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    Ok((cols, rows))
}

fn dims_payload(cols: u16, rows: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(4);
    v.extend_from_slice(&cols.to_le_bytes());
    v.extend_from_slice(&rows.to_le_bytes());
    v
}

impl ClientMsg {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            ClientMsg::Attach { cols, rows } => {
                write_frame(w, tag::ATTACH, &dims_payload(*cols, *rows))
            }
            ClientMsg::Input(bytes) => write_frame(w, tag::INPUT, bytes),
            ClientMsg::Resize { cols, rows } => {
                write_frame(w, tag::RESIZE, &dims_payload(*cols, *rows))
            }
            ClientMsg::Detach => write_frame(w, tag::DETACH, &[]),
            ClientMsg::Kill => write_frame(w, tag::KILL, &[]),
            ClientMsg::Info => write_frame(w, tag::INFO, &[]),
            ClientMsg::Capture => write_frame(w, tag::CAPTURE, &[]),
            ClientMsg::DetachClients => write_frame(w, tag::DETACH_CLIENTS, &[]),
        }
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<ClientMsg> {
        let (tag, payload) = read_frame(r)?;
        match tag {
            tag::ATTACH => {
                let (cols, rows) = dims(&payload)?;
                Ok(ClientMsg::Attach { cols, rows })
            }
            tag::INPUT => Ok(ClientMsg::Input(payload)),
            tag::RESIZE => {
                let (cols, rows) = dims(&payload)?;
                Ok(ClientMsg::Resize { cols, rows })
            }
            tag::DETACH => Ok(ClientMsg::Detach),
            tag::KILL => Ok(ClientMsg::Kill),
            tag::INFO => Ok(ClientMsg::Info),
            tag::CAPTURE => Ok(ClientMsg::Capture),
            tag::DETACH_CLIENTS => Ok(ClientMsg::DetachClients),
            other => Err(bad(format!("unknown client frame tag 0x{other:02x}"))),
        }
    }
}

impl ServerMsg {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            ServerMsg::Repaint(bytes) => write_frame(w, tag::REPAINT, bytes),
            ServerMsg::Output(bytes) => write_frame(w, tag::OUTPUT, bytes),
            ServerMsg::Exited(code) => write_frame(w, tag::EXITED, &code.to_le_bytes()),
            ServerMsg::InfoReply {
                cols,
                rows,
                clients,
                command,
            } => {
                let mut payload = dims_payload(*cols, *rows);
                payload.extend_from_slice(&clients.to_le_bytes());
                payload.extend_from_slice(command.as_bytes());
                write_frame(w, tag::INFO_REPLY, &payload)
            }
            ServerMsg::CaptureReply(text) => write_frame(w, tag::CAPTURE_REPLY, text.as_bytes()),
            ServerMsg::Detached => write_frame(w, tag::DETACHED, &[]),
        }
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<ServerMsg> {
        let (tag, payload) = read_frame(r)?;
        match tag {
            tag::REPAINT => Ok(ServerMsg::Repaint(payload)),
            tag::OUTPUT => Ok(ServerMsg::Output(payload)),
            tag::EXITED => {
                if payload.len() < 4 {
                    return Err(bad("expected 4 bytes of exit code"));
                }
                Ok(ServerMsg::Exited(i32::from_le_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ])))
            }
            tag::INFO_REPLY => {
                if payload.len() < 8 {
                    return Err(bad("truncated info reply"));
                }
                let (cols, rows) = dims(&payload)?;
                let clients = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
                let command = String::from_utf8_lossy(&payload[8..]).into_owned();
                Ok(ServerMsg::InfoReply {
                    cols,
                    rows,
                    clients,
                    command,
                })
            }
            tag::CAPTURE_REPLY => Ok(ServerMsg::CaptureReply(
                String::from_utf8_lossy(&payload).into_owned(),
            )),
            tag::DETACHED => Ok(ServerMsg::Detached),
            other => Err(bad(format!("unknown server frame tag 0x{other:02x}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip_client(msg: ClientMsg) {
        let mut buf = Vec::new();
        msg.write_to(&mut buf).unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(ClientMsg::read_from(&mut cursor).unwrap(), msg);
    }

    fn roundtrip_server(msg: ServerMsg) {
        let mut buf = Vec::new();
        msg.write_to(&mut buf).unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(ServerMsg::read_from(&mut cursor).unwrap(), msg);
    }

    #[test]
    fn client_messages_roundtrip() {
        roundtrip_client(ClientMsg::Attach {
            cols: 120,
            rows: 30,
        });
        roundtrip_client(ClientMsg::Input(b"pwsh -NoLogo\r".to_vec()));
        roundtrip_client(ClientMsg::Resize { cols: 80, rows: 24 });
        roundtrip_client(ClientMsg::Detach);
        roundtrip_client(ClientMsg::Kill);
        roundtrip_client(ClientMsg::Info);
        roundtrip_client(ClientMsg::Capture);
    }

    #[test]
    fn server_messages_roundtrip() {
        roundtrip_server(ServerMsg::Repaint(b"\x1b[2J\x1b[H".to_vec()));
        roundtrip_server(ServerMsg::Output(vec![0u8, 1, 2, 255]));
        roundtrip_server(ServerMsg::Exited(-1));
        roundtrip_server(ServerMsg::InfoReply {
            cols: 100,
            rows: 40,
            clients: 2,
            command: "pwsh.exe".into(),
        });
        roundtrip_server(ServerMsg::CaptureReply("PS X:\\> echo hi\nhi\n".into()));
    }

    #[test]
    fn capture_reply_survives_non_ascii() {
        roundtrip_server(ServerMsg::CaptureReply("naïve — ✓ 日本語".into()));
    }

    #[test]
    fn empty_input_payload_roundtrips() {
        roundtrip_client(ClientMsg::Input(Vec::new()));
    }

    #[test]
    fn several_frames_stream_back_to_back() {
        let mut buf = Vec::new();
        ClientMsg::Input(b"a".to_vec()).write_to(&mut buf).unwrap();
        ClientMsg::Resize { cols: 1, rows: 2 }
            .write_to(&mut buf)
            .unwrap();
        ClientMsg::Detach.write_to(&mut buf).unwrap();

        let mut cursor = Cursor::new(buf);
        assert_eq!(
            ClientMsg::read_from(&mut cursor).unwrap(),
            ClientMsg::Input(b"a".to_vec())
        );
        assert_eq!(
            ClientMsg::read_from(&mut cursor).unwrap(),
            ClientMsg::Resize { cols: 1, rows: 2 }
        );
        assert_eq!(
            ClientMsg::read_from(&mut cursor).unwrap(),
            ClientMsg::Detach
        );
    }

    #[test]
    fn unknown_tag_is_rejected() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 0x7f, b"nonsense").unwrap();
        let err = ClientMsg::read_from(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversized_length_prefix_is_rejected_without_allocating() {
        let mut buf = vec![tag::INPUT];
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        let err = ClientMsg::read_from(&mut Cursor::new(buf)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_frame_is_an_error() {
        let mut buf = Vec::new();
        ClientMsg::Input(b"hello".to_vec())
            .write_to(&mut buf)
            .unwrap();
        buf.truncate(buf.len() - 2);
        assert!(ClientMsg::read_from(&mut Cursor::new(buf)).is_err());
    }
}
