//! wmux — terminal session persistence for Windows.
//!
//! The crate is split into a library and a thin binary so that the session
//! protocol can be driven directly from integration tests. Attaching for real
//! needs a console; the protocol does not, which is what makes the
//! detach/reattach behaviour testable in CI.
//!
//! # Architecture
//!
//! ```text
//!   wmux new ──spawns──> wmux server (DETACHED_PROCESS, no console)
//!                             │
//!                             ├── ConPTY ──> pwsh.exe
//!                             ├── vt100::Parser  (screen model for repaint)
//!                             └── named pipe ──< wmux attach (any terminal)
//! ```
//!
//! The server keeps the terminal model because ConPTY only ever hands out a
//! *stream*. Once bytes have gone by there is no way to ask Windows what the
//! screen looks like now, so wmux replays the stream into a terminal model and
//! serialises that model back to VT for each new client.

pub mod client;
pub mod console;
pub mod input;
pub mod pipe;
pub mod protocol;
pub mod run;
pub mod server;
pub mod session;
