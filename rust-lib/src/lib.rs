//! monero_wallet_backend — the Monero wallet family's coordinator.
//!
//! Sits between the two UIs and the engine: keeps the wallet registry, drives the wallet
//! core's tickets, polls sync and balances into events, normalises history, and orchestrates
//! a send as build → review → broadcast. It holds no key material and caches no password —
//! those exist only inside `monero_wallet_core_module`.
//!
//! The pure core ([`model`]) is unit-tested with `cargo test --no-default-features`; the
//! Logos glue is behind the default `logos_module` feature.
pub mod gate;
pub mod model;

#[cfg(feature = "logos_module")]
mod glue;
