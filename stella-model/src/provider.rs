//! Re-exports the `Provider` port. The trait itself lives in
//! `stella-protocol` so `stella-core` can drive
//! every model call through `&dyn Provider` without depending on any
//! concrete adapter; every adapter in this crate implements it.

pub use stella_protocol::Provider;
