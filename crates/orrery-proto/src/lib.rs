//! Wire formats shared by the orrery server and its test tools.
//!
//! - [`klippy`]: the Klippy unix socket protocol (JSON messages terminated by `0x03`).
//! - [`jsonrpc`]: JSON-RPC 2.0 envelopes as Moonraker sends and accepts them on its websocket.

pub mod jsonrpc;
pub mod klippy;
