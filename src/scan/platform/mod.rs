#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "linux")]
pub use linux::LinuxFilesystem;
#[cfg(target_os = "windows")]
pub use windows::WindowsFilesystem;
