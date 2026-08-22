#[cfg(target_os = "linux")]
pub mod linux;

// The record parsers are pure byte handling with their own unit tests;
// they are compiled on every platform so malformed-input coverage runs in
// both Linux and Windows CI.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod win_enum;

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod mft;

#[cfg(target_os = "windows")]
pub mod windows;
