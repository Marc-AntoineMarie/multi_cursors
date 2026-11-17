//! Moteur MultiCursos : configuration, rôle serveur, rôle client.

pub mod client;
pub mod config;

/// Le rôle serveur suppose de capturer les périphériques locaux, ce qui n'existe pour l'instant
/// que sous Linux. Les autres plateformes compilent le rôle client seul (étape 5 : capture Windows
/// et macOS, et n'importe quelle machine pourra tenir le rôle serveur).
#[cfg(target_os = "linux")]
pub mod server;

pub use config::Config;
