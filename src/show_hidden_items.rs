use gpui::Global;

/// The single live application-wide owner of hidden-item visibility.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShowHiddenItems(pub(crate) bool);

impl Global for ShowHiddenItems {}
