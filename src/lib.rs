//! Standalone native engine. No network or application integration.
pub mod decode;
pub mod output;
#[cfg(windows)]
mod windows;
