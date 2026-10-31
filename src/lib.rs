//! sessift: search AI coding session history on this machine and others, live, over SSH.
//!
//! See docs/plan.md for the design and docs/remote-protocol.md for the wire protocol.

pub mod backend;
pub mod broker;
pub mod cli;
pub mod client;
pub mod codec;
pub mod config;
pub mod fanout;
pub mod herdr;
pub mod index;
pub mod install;
pub mod model;
pub mod output;
pub mod pick;
pub mod proto;
pub mod rpc;
pub mod server;
pub mod skill;
pub mod text;
pub mod timefmt;
pub mod tui;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
