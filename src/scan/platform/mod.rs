#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "windows")]
pub(crate) mod win_enum;
#[cfg(target_os = "windows")]
pub mod windows;

// The record parser is pure byte handling: compile and test it on every
// platform so malformed-input coverage runs in Linux CI too.
#[cfg(all(not(target_os = "windows"), test))]
pub mod win_enum;
