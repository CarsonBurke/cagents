//! The shell's own controls, composed from the tokens in [`crate::design`].

#[cfg(test)]
pub(crate) mod accessibility_probe;
pub(crate) mod accessible_bounds;
pub mod button;
mod culled_column;
pub use culled_column::{CulledColumn, culled_column, keep_in_place};
mod display;
mod editable;
pub mod field;
mod find_options;
pub use find_options::{FindOption, find_options};
mod gauge;
pub mod hover_reveal;
mod icon;
mod icon_button;
mod labeled_controls;
mod labelled;
mod list_row;
mod owned;
mod placeholder;
mod popover;
mod pull_request_state;
mod responsive;
mod shrinkable;
mod tabs;

pub use display::{badge, change_counts, comment_card};
#[cfg(test)]
pub(crate) use gauge::activity_animates;
pub use gauge::{
    activity_spinner, animate_activity, budget_bar, budget_radial, budget_radial_toned,
};
pub use icon::{Icon, ceditor_icon, ceditor_window_icon, passive_icon, provider_icon};
pub use icon_button::{
    IconButton, control_style, icon_button, outlined_control_style, segmented_controls,
    toolbar_actions,
};
pub use labeled_controls::labeled_controls;
pub use labelled::labelled;
pub use list_row::{ListRow, list_row, row_label};
pub use owned::owned;
pub use placeholder::{Placeholder, placeholder};
pub use popover::{popover, popover_item};
pub use pull_request_state::{
    ChecksSummary, WorktreePullRequestState, check_state_glyph, check_state_label,
    check_state_look, checks_glyph, pull_request_review_glyph, pull_request_state_glyph,
    review_label, review_look,
};
pub use responsive::responsive;
pub use shrinkable::shrinkable;
pub use tabs::{ViewTab, nested_view_tabs, view_tab, view_tab_height, view_tabs};
