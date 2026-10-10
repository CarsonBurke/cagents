// Copyright 2026. Licensed under MIT OR Apache-2.0; see the included licenses.
//! The supported string replacement action. Other EditableText methods report
//! refusal until an application clipboard/range-edit contract is available.
use accesskit_atspi_common::PlatformNode;
use zbus::{fdo, interface};

pub(crate) struct EditableTextInterface {
    node: PlatformNode,
}
impl EditableTextInterface {
    pub fn new(node: PlatformNode) -> Self {
        Self { node }
    }
}
#[interface(name = "org.a11y.atspi.EditableText")]
impl EditableTextInterface {
    fn set_text_contents(&self, new_contents: &str) -> fdo::Result<bool> {
        self.node
            .set_text_contents(new_contents)
            .map_err(|error| crate::util::map_error_from_node(&self.node, error))
    }
    fn insert_text(&self, _position: i32, _text: &str, _length: i32) -> bool {
        false
    }
    fn delete_text(&self, _start_pos: i32, _end_pos: i32) -> bool {
        false
    }
    fn copy_text(&self, _start_pos: i32, _end_pos: i32) -> bool {
        false
    }
    fn cut_text(&self, _start_pos: i32, _end_pos: i32) -> bool {
        false
    }
    fn paste_text(&self, _position: i32) -> bool {
        false
    }
    fn set_attributes(&self, _attributes: &str, _start_pos: i32, _end_pos: i32) -> bool {
        false
    }
}
