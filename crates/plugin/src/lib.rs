//! The wrk session-plugin protocol.
//!
//! A *session plugin* is an external process that provides a wrk project's
//! terminal sessions in place of wrk's built-in local agent PTYs — e.g. a plugin
//! that maps a project onto a remote box and its sessions. wrk is the client;
//! the plugin is a server on a Unix domain socket. Each exchange is one line of
//! JSON each way on a fresh connection.
//!
//! The data plane is a *command*: an [`proto::AttachResponse`] names the
//! process wrk spawns in an ordinary terminal pane, and that process is the
//! transport to wherever the session really lives (ssh, a custom client, …).
//!
//! - [`proto`] — the wire types and reply envelope, shared by wrk and by plugins
//!   written in Rust (a plugin in any other language speaks the same JSON).
//! - [`client`] — wrk's side: reachability probe, request/reply with timeouts,
//!   and launching a plugin daemon with fail-fast startup and captured output.
//!
//! Writing a plugin? Start with this crate's `README.md`, *Writing a wrk session
//! plugin*: the contract, how wrk drives a plugin, a complete skeleton, status
//! reporting, and how to test a plugin the way wrk launches it.

pub mod client;
pub mod proto;

/// Compiles the guide's Rust examples as doctests, so they keep matching the
/// real types.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
