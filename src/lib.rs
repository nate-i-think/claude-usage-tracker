//! Claude usage monitor: library core shared by the GUI and CLI binaries.

pub mod config;
pub mod model;
pub mod paths;

/// Install the rustls `ring` crypto provider as the process default.
///
/// Safe to call more than once; an already-installed provider is ignored.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
