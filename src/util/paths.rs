/// Path helpers shared by UI and actions.
///
/// Lossy display name of a path's final component.
#[allow(dead_code)]
pub fn display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Shorten a user home prefix to `~` for compact display.
pub fn shorten_home(path: &std::path::Path) -> std::path::PathBuf {
    if let Some(home) = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf())
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return std::path::PathBuf::from("~").join(rest);
    }
    path.to_path_buf()
}
