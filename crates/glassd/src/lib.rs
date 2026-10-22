//! glassd: the Glass services daemon (docs/api.md). The binary is `src/main.rs`; the modules
//! are a library so they can be tested on the host.

pub mod buttons;
#[cfg(target_os = "android")]
pub mod compat;
pub mod daemon;
pub mod display;
pub mod log;
pub mod mockcmd;
pub mod peers;
pub mod queue;
pub mod screen;
pub mod settings_store;
pub mod touch;
