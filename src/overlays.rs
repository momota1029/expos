//! Shell-wide modal overlay policy.
//!
//! The overlay state itself remains owned by its existing owner (operations,
//! search, menu, or the folder-tree projection).  This module only derives the
//! one visible/interactive top surface from those flags.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayKind {
    Conflict,
    Search,
    Menu,
    FolderTree,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenOverlays {
    pub conflict: bool,
    pub search: bool,
    pub menu: bool,
    pub folder_tree: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OpenPolicy {
    pub allowed: bool,
    pub close: OpenOverlays,
}

pub(crate) fn priority(kind: OverlayKind) -> u8 {
    match kind {
        OverlayKind::Conflict => 4,
        OverlayKind::Search => 3,
        OverlayKind::Menu => 2,
        OverlayKind::FolderTree => 1,
    }
}

pub(crate) fn top_kind(open: OpenOverlays) -> Option<OverlayKind> {
    [
        (open.conflict, OverlayKind::Conflict),
        (open.search, OverlayKind::Search),
        (open.menu, OverlayKind::Menu),
        (open.folder_tree, OverlayKind::FolderTree),
    ]
    .into_iter()
    .filter_map(|(is_open, kind)| is_open.then_some(kind))
    .max_by_key(|kind| priority(*kind))
}

pub(crate) fn open_policy(kind: OverlayKind, open: OpenOverlays) -> OpenPolicy {
    match kind {
        OverlayKind::Conflict => OpenPolicy {
            allowed: true,
            close: OpenOverlays {
                search: open.search,
                menu: open.menu,
                folder_tree: open.folder_tree,
                ..OpenOverlays::default()
            },
        },
        OverlayKind::Search => OpenPolicy {
            allowed: !open.conflict,
            close: OpenOverlays {
                menu: open.menu,
                folder_tree: open.folder_tree,
                ..OpenOverlays::default()
            },
        },
        OverlayKind::Menu => OpenPolicy {
            allowed: !open.conflict && !open.search,
            close: OpenOverlays {
                folder_tree: open.folder_tree,
                ..OpenOverlays::default()
            },
        },
        OverlayKind::FolderTree => OpenPolicy {
            allowed: !open.conflict && !open.search,
            close: OpenOverlays {
                menu: open.menu,
                ..OpenOverlays::default()
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_and_top_kind_are_fixed() {
        assert!(priority(OverlayKind::Conflict) > priority(OverlayKind::Search));
        assert!(priority(OverlayKind::Search) > priority(OverlayKind::Menu));
        assert!(priority(OverlayKind::Menu) > priority(OverlayKind::FolderTree));
        assert_eq!(top_kind(OpenOverlays::default()), None);
        assert_eq!(
            top_kind(OpenOverlays {
                conflict: true,
                search: true,
                menu: true,
                folder_tree: true,
            }),
            Some(OverlayKind::Conflict)
        );
    }

    #[test]
    fn open_policy_enforces_exclusive_surfaces() {
        let all = OpenOverlays {
            conflict: true,
            search: true,
            menu: true,
            folder_tree: true,
        };
        let conflict = open_policy(OverlayKind::Conflict, all);
        assert!(conflict.allowed);
        assert!(conflict.close.search && conflict.close.menu && conflict.close.folder_tree);
        assert!(!open_policy(OverlayKind::Search, all).allowed);
        assert!(!open_policy(OverlayKind::Menu, all).allowed);
        assert!(!open_policy(OverlayKind::FolderTree, all).allowed);

        let search = open_policy(
            OverlayKind::Search,
            OpenOverlays {
                menu: true,
                folder_tree: true,
                ..OpenOverlays::default()
            },
        );
        assert!(search.allowed && search.close.menu && search.close.folder_tree);
        let menu = open_policy(
            OverlayKind::Menu,
            OpenOverlays {
                folder_tree: true,
                ..OpenOverlays::default()
            },
        );
        assert!(menu.allowed && menu.close.folder_tree);
    }

    #[test]
    fn only_the_top_surface_handles_escape() {
        let open = OpenOverlays {
            search: true,
            menu: true,
            folder_tree: true,
            ..OpenOverlays::default()
        };
        assert_eq!(top_kind(open), Some(OverlayKind::Search));
        assert_eq!(
            top_kind(OpenOverlays {
                search: false,
                menu: true,
                folder_tree: true,
                ..open
            }),
            Some(OverlayKind::Menu)
        );
        assert_eq!(
            top_kind(OpenOverlays {
                search: false,
                menu: false,
                folder_tree: true,
                ..open
            }),
            Some(OverlayKind::FolderTree)
        );
    }
}
