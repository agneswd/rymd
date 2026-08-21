# Rymd

A calm, fast disk usage visualizer for Linux and Windows.

Rymd scans a directory tree in the background and answers the questions
that actually matter: what is using this space, which directories are the
largest, which files are the largest, what is safe to remove, and how much
space deleting something would reclaim.

The interface stays out of the way: one window with a treemap, a virtualized
file table, and nothing else.

![Rymd](assets/rymd.svg)

## Features

- **Background scanning** on a bounded worker pool. The UI never blocks,
  even with millions of entries.
- **Real disk usage** (`st_blocks` on Linux, cluster-rounded sizes on
  Windows), plus apparent size, switchable at any time.
- **Squarified treemap** of the open directory with tooltips, selection
  sync, double-click navigation and context menus.
- **Virtualized table** with sorting, column resizing and filtering.
- **Hard links counted once**, symlinks never followed, mount boundaries
  respected.
- **Duplicates finder**: size grouping, partial hashing, full BLAKE3 only
  where needed; hard-linked copies are never reported as waste.
- **Safe deletion**: inode re-verification before every destructive action,
  confirmation dialogs everywhere, Trash by default, scan-root protection.
- **Keyboard first**: `Ctrl+O` open, `Ctrl+R` rescan current directory,
  `Ctrl+F` filter, `Alt+Left/Right` history, `Backspace` parent,
  `Delete` trash, `Shift+Delete` delete permanently, `Ctrl+=` / `Ctrl+-`
  zoom.

## Building

You need Rust 1.90 or newer.

```sh
cargo build --release
```

On Windows, MSVC or MinGW both work; the platform layer uses
`windows-sys` only.

## Running

```sh
./target/release/rymd            # opens with an empty state
./target/release/rymd ~/Projects # starts scanning immediately
RYMD_TAB=duplicates ./target/release/rymd .  # start on the duplicates tab
```

## Platform notes

| Concern        | Linux                     | Windows                       |
| -------------- | ------------------------- | ----------------------------- |
| Identity       | `st_dev` + `st_ino`       | volume serial + NTFS file id  |
| Allocated size | `st_blocks * 512`         | size rounded to cluster       |
| Free space     | `statvfs`                 | `GetDiskFreeSpaceExW`         |
| Symlinks       | lstat, never followed     | reparse points never followed |
| Trash          | FreeDesktop via `trash`   | Recycle Bin via `trash`       |

## License

MIT
