//! Shared context-menu construction for table rows and treemap rectangles.
//!
//! Items dispatch payload actions so they run through the same handlers as
//! keyboard shortcuts, always targeting the row or rectangle that was
//! right-clicked.

use gpui_component::menu::{PopupMenu, PopupMenuItem};

use crate::actions::*;
use crate::model::NodeId;

pub fn node_context_menu(
    mut menu: PopupMenu,
    node: NodeId,
    is_dir: bool,
    hardlink: bool,
) -> PopupMenu {
    menu = menu.item(PopupMenuItem::new("Open").action(Box::new(OpenNode(node))));
    menu =
        menu.item(PopupMenuItem::new("Reveal in file manager").action(Box::new(RevealNode(node))));
    menu = menu.item(PopupMenuItem::new("Copy path").action(Box::new(CopyPathNode(node))));
    menu = menu.separator();
    menu = menu.item(PopupMenuItem::new("Move to Trash").action(Box::new(TrashNode(node))));
    menu =
        menu.item(PopupMenuItem::new("Delete permanently...").action(Box::new(DeleteNode(node))));
    if is_dir {
        menu =
            menu.item(PopupMenuItem::new("Clear contents...").action(Box::new(ClearDirNode(node))));
    }
    if hardlink {
        menu = menu.item(PopupMenuItem::label(
            "Hard link: storage counted at another path",
        ));
    }
    menu
}
