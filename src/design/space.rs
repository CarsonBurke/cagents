//! The spacing scale: every gap and inset in the shell is one of these steps.
//!
//! The steps are fixed logical pixels rather than COSMIC's density-dependent
//! `theme::spacing()`. A workbench is laid out against its own row heights,
//! icon columns and hit targets, none of which follow the desktop's density
//! setting, so a gap that did would drift out of proportion with everything
//! around it. Reading a constant also costs nothing, where `theme::spacing()`
//! locks and clones the process-global theme on every call.

/// A hairline of air: between a label and its own caption, or around a
/// control inside a grouped toolbar.
pub const XXS: f32 = 2.0;
/// Between the parts of one control: an icon and its label.
pub const XS: f32 = 4.0;
/// Between sibling controls, and the inset of a compact row.
pub const S: f32 = 8.0;
/// Between groups of controls, and the inset of a pane's content.
pub const M: f32 = 12.0;
/// Between the blocks of a form or a card, and the inset of a dialog.
pub const L: f32 = 16.0;
/// Between the sections of a page.
pub const XL: f32 = 24.0;
/// Around the content of an otherwise empty pane.
pub const XXL: f32 = 32.0;
