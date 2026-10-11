//! Claude usage monitor: library core shared by the GUI and CLI binaries.

pub mod analytics;
pub mod config;
pub mod cookies;
pub mod debug_report;
pub mod fetch;
pub mod mcp;
pub mod model;
pub mod paths;
pub mod poller;
pub mod storage;
pub mod text_report;
pub mod ui;

/// Install the rustls `ring` crypto provider as the process default.
///
/// Safe to call more than once; an already-installed provider is ignored.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
