//! Ferrotune: a lightweight native Spotify desktop client (unofficial).
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod app;
pub mod auth;
pub mod config;
pub mod desktop;
pub mod error;
pub mod logging;
pub mod mock;
pub mod net;
pub mod store;
pub mod tray;
pub mod ui;
pub mod update;
