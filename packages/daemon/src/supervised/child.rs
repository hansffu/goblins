//! Platform-neutral inherited-control child lifecycle used by both modes.
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
pub(crate) use platform::Child;
