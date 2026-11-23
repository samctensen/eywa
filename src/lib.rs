//! Eywa: a per-user service that watches local coding agents and publishes
//! which of them need attention. See `docs/design.md`.

pub mod adapter;
pub mod api;
pub mod hook;
pub mod launchd;
pub mod process;
pub mod record;
pub mod registry;
pub mod row;
pub mod server;
pub mod setup;
pub mod top;
pub mod tree;
