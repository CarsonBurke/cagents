#![cfg(feature = "ui")]

//! Exercise named-tree reconciliation through the UI's actual vendored iced.
//! No renderer, native window, timing recorder or application runtime is started.

use cosmic::iced;
use iced::advanced::{
    Layout, Widget, layout, mouse, renderer,
    widget::{
        Id, Tree,
        tree::{NAMED, State, Tag},
    },
};
type Renderer = cosmic::Renderer;

include!("support/iced_tree.rs");
