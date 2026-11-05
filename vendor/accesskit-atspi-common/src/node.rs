// Copyright 2022 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

// Derived from Chromium's accessibility abstraction.
// Copyright 2017 The Chromium Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE.chromium file.

use accesskit::{
    Action, ActionData, ActionRequest, Affine, Live, NodeId as LocalNodeId, Orientation, Point,
    Rect, Role, Toggled, TreeId,
};
use accesskit_consumer::{FilterResult, Node, NodeId, Tree, TreeState};
use atspi_common::{
    CoordType, Granularity, Interface, InterfaceSet, Layer, Politeness, RelationType,
    Role as AtspiRole, ScrollType, State, StateSet,
};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    iter::FusedIterator,
    sync::{Arc, RwLock, RwLockReadGuard, Weak},
};

use crate::{
    Action as AtspiAction, Error, ObjectEvent, Property, Rect as AtspiRect, Result,
    adapter::Adapter,
    context::{AppContext, Context},
    filters::filter,
    util::*,
};

pub(crate) struct NodeWrapper<'a>(pub(crate) &'a Node<'a>);

impl NodeWrapper<'_> {
    pub(crate) fn name(&self) -> Option<String> {
        if self.0.label_comes_from_value() {
            self.0.value()
        } else {
            self.0.label()
        }
    }

    pub(crate) fn description(&self) -> Option<String> {
        self.0.description()
    }

    pub(crate) fn parent_id(&self) -> Option<NodeId> {
        self.0.parent_id()
    }

    pub(crate) fn id(&self) -> NodeId {
        self.0.id()
    }

    fn filtered_child_ids(
        &self,
    ) -> impl DoubleEndedIterator<Item = NodeId> + FusedIterator<Item = NodeId> + '_ {
        self.0.filtered_children(&filter).map(|child| child.id())
    }

    pub(crate) fn role(&self) -> AtspiRole {
        if self.0.has_role_description() {
            return AtspiRole::Extended;
        }

        match self.0.role() {
            Role::Alert => AtspiRole::Notification,
            Role::AlertDialog => AtspiRole::Alert,
            Role::Comment | Role::Suggestion => AtspiRole::Section,
            // TODO: See how to represent ARIA role="application"
            Role::Application => AtspiRole::Embedded,
            Role::Article => AtspiRole::Article,
            Role::Audio => AtspiRole::Audio,
            Role::Banner | Role::Header => AtspiRole::Landmark,
            Role::Blockquote => AtspiRole::BlockQuote,
            Role::Caret => AtspiRole::Unknown,
            Role::Button => {
                if self.0.toggled().is_some() {
                    AtspiRole::ToggleButton
                } else {
                    AtspiRole::Button
                }
            }
            Role::DefaultButton => AtspiRole::Button,
            Role::Canvas => AtspiRole::Canvas,
            Role::Caption => AtspiRole::Caption,
            Role::Cell | Role::GridCell => AtspiRole::TableCell,
            Role::CheckBox => AtspiRole::CheckBox,
            Role::Switch => AtspiRole::ToggleButton,
            Role::ColorWell => AtspiRole::Button,
            Role::ColumnHeader => AtspiRole::ColumnHeader,
            Role::ComboBox | Role::EditableComboBox => AtspiRole::ComboBox,
            Role::Complementary => AtspiRole::Landmark,
            Role::ContentDeletion => AtspiRole::ContentDeletion,
            Role::ContentInsertion => AtspiRole::ContentInsertion,
            Role::ContentInfo | Role::Footer => AtspiRole::Landmark,
            Role::Definition => AtspiRole::DescriptionValue,
            Role::DescriptionList => AtspiRole::DescriptionList,
            Role::Details => AtspiRole::Panel,
            Role::Dialog => AtspiRole::Dialog,
            Role::DisclosureTriangle => AtspiRole::ToggleButton,
            Role::DocCover => AtspiRole::Image,
            Role::DocBackLink | Role::DocBiblioRef | Role::DocGlossRef | Role::DocNoteRef => {
                AtspiRole::Link
            }
            Role::DocBiblioEntry | Role::DocEndnote => AtspiRole::ListItem,
            Role::DocNotice | Role::DocTip => AtspiRole::Comment,
            Role::DocFootnote => AtspiRole::Footnote,
            Role::DocPageBreak => AtspiRole::Separator,
            Role::DocPageFooter => AtspiRole::Footer,
            Role::DocPageHeader => AtspiRole::Header,
            Role::DocAcknowledgements
            | Role::DocAfterword
            | Role::DocAppendix
            | Role::DocBibliography
            | Role::DocChapter
            | Role::DocConclusion
            | Role::DocCredits
            | Role::DocEndnotes
            | Role::DocEpilogue
            | Role::DocErrata
            | Role::DocForeword
            | Role::DocGlossary
            | Role::DocIndex
            | Role::DocIntroduction
            | Role::DocPageList
            | Role::DocPart
            | Role::DocPreface
            | Role::DocPrologue
            | Role::DocToc => AtspiRole::Landmark,
            Role::DocAbstract
            | Role::DocColophon
            | Role::DocCredit
            | Role::DocDedication
            | Role::DocEpigraph
            | Role::DocExample
            | Role::DocPullquote
            | Role::DocQna => AtspiRole::Section,
            Role::DocSubtitle => AtspiRole::Heading,
            Role::Document => AtspiRole::DocumentFrame,
            Role::EmbeddedObject => AtspiRole::Embedded,
            // TODO: Forms which lack an accessible name are no longer
            // exposed as forms. Forms which have accessible
            // names should be exposed as `AtspiRole::Landmark` according to Core AAM.
            Role::Form => AtspiRole::Form,
            Role::Figure | Role::Feed => AtspiRole::Panel,
            Role::GenericContainer | Role::Ruby => AtspiRole::Section,
            Role::GraphicsDocument => AtspiRole::DocumentFrame,
            Role::GraphicsObject => AtspiRole::Panel,
            Role::GraphicsSymbol => AtspiRole::Image,
            Role::Grid => AtspiRole::Table,
            Role::Group => AtspiRole::Panel,
            Role::Heading => AtspiRole::Heading,
            Role::Iframe | Role::IframePresentational => AtspiRole::InternalFrame,
            // TODO: If there are unignored children, then it should be AtspiRole::ImageMap.
            Role::Image => AtspiRole::Image,
            Role::TextRun => AtspiRole::Static,
            Role::Legend => AtspiRole::Label,
            // Layout table objects are treated the same as `Role::GenericContainer`.
            Role::LayoutTable => AtspiRole::Section,
            Role::LayoutTableCell => AtspiRole::Section,
            Role::LayoutTableRow => AtspiRole::Section,
            // TODO: Having a separate accessible object for line breaks
            // is inconsistent with other implementations.
            Role::LineBreak => AtspiRole::Static,
            Role::Link => AtspiRole::Link,
            Role::List => AtspiRole::List,
            Role::ListBox => AtspiRole::ListBox,
            // TODO: Use `AtspiRole::MenuItem' inside a combo box.
            Role::ListBoxOption => AtspiRole::ListItem,
            Role::ListGrid => AtspiRole::Table,
            Role::ListItem => AtspiRole::ListItem,
            // Regular list markers only expose their alternative text, but do not
            // expose their descendants, and the descendants should be ignored. This
            // is because the alternative text depends on the counter style and can
            // be different from the actual (visual) marker text, and hence,
            // inconsistent with the descendants. We treat a list marker as non-text
            // only if it still has non-ignored descendants, which happens only when =>
            // - The list marker itself is ignored but the descendants are not
            // - Or the list marker contains images
            Role::ListMarker => AtspiRole::Static,
            Role::Log => AtspiRole::Log,
            Role::Main => AtspiRole::Landmark,
            Role::Mark => AtspiRole::Static,
            Role::Math => AtspiRole::Math,
            Role::Marquee => AtspiRole::Marquee,
            Role::Menu | Role::MenuListPopup => AtspiRole::Menu,
            Role::MenuBar => AtspiRole::MenuBar,
            Role::MenuItem | Role::MenuListOption => AtspiRole::MenuItem,
            Role::MenuItemCheckBox => AtspiRole::CheckMenuItem,
            Role::MenuItemRadio => AtspiRole::RadioMenuItem,
            Role::Meter => AtspiRole::LevelBar,
            Role::Navigation => AtspiRole::Landmark,
            Role::Note => AtspiRole::Comment,
            Role::Pane | Role::ScrollView => AtspiRole::Panel,
            Role::Paragraph => AtspiRole::Paragraph,
            Role::PdfActionableHighlight => AtspiRole::Button,
            Role::PdfRoot => AtspiRole::DocumentFrame,
            Role::PluginObject => AtspiRole::Embedded,
            Role::ProgressIndicator => AtspiRole::ProgressBar,
            Role::RadioButton => AtspiRole::RadioButton,
            Role::RadioGroup => AtspiRole::Panel,
            Role::Region => AtspiRole::Landmark,
            Role::RootWebArea => AtspiRole::DocumentWeb,
            Role::Row => AtspiRole::TableRow,
            Role::RowGroup => AtspiRole::Panel,
            Role::RowHeader => AtspiRole::RowHeader,
            // TODO: Generally exposed as description on <ruby> (`Role::Ruby`) element, not
            // as its own object in the tree.
            // However, it's possible to make a `Role::RubyAnnotation` element show up in the
            // tree, for example by adding tabindex="0" to the source <rp> or <rt>
            // element or making the source element the target of an aria-owns.
            // Therefore, we need to gracefully handle it if it actually
            // shows up in the tree.
            Role::RubyAnnotation => AtspiRole::Static,
            Role::Section => AtspiRole::Section,
            Role::SectionFooter => AtspiRole::Footer,
            Role::SectionHeader => AtspiRole::Header,
            Role::ScrollBar => AtspiRole::ScrollBar,
            Role::Search => AtspiRole::Landmark,
            Role::Slider => AtspiRole::Slider,
            Role::SpinButton => AtspiRole::SpinButton,
            Role::Splitter => AtspiRole::Separator,
            Role::Label => AtspiRole::Label,
            Role::Status => AtspiRole::StatusBar,
            Role::SvgRoot => AtspiRole::DocumentFrame,
            Role::Tab => AtspiRole::PageTab,
            Role::Table => AtspiRole::Table,
            Role::TabList => AtspiRole::PageTabList,
            Role::TabPanel => AtspiRole::ScrollPane,
            // TODO: This mapping should also be applied to the dfn
            // element.
            Role::Term => AtspiRole::DescriptionTerm,
            Role::TitleBar => AtspiRole::TitleBar,
            Role::TextInput
            | Role::MultilineTextInput
            | Role::SearchInput
            | Role::EmailInput
            | Role::NumberInput
            | Role::PhoneNumberInput
            | Role::UrlInput => AtspiRole::Entry,
            Role::DateInput
            | Role::DateTimeInput
            | Role::WeekInput
            | Role::MonthInput
            | Role::TimeInput => AtspiRole::DateEditor,
            Role::PasswordInput => AtspiRole::PasswordText,
            Role::Abbr | Role::Code | Role::Emphasis | Role::Strong | Role::Time => {
                AtspiRole::Static
            }
            Role::Timer => AtspiRole::Timer,
            Role::Toolbar => AtspiRole::ToolBar,
            Role::Tooltip => AtspiRole::ToolTip,
            Role::Tree => AtspiRole::Tree,
            Role::TreeItem => AtspiRole::TreeItem,
            Role::TreeGrid => AtspiRole::TreeTable,
            Role::Video => AtspiRole::Video,
            // In AT-SPI, elements with `AtspiRole::Frame` are windows with titles and
            // buttons, while those with `AtspiRole::Window` are windows without those
            // elements.
            Role::Window => AtspiRole::Frame,
            Role::WebView => AtspiRole::Panel,
            Role::FigureCaption => AtspiRole::Caption,
            // TODO: Are there special cases to consider?
            Role::Unknown => AtspiRole::Unknown,
            Role::ImeCandidate | Role::Keyboard => AtspiRole::RedundantObject,
            Role::Terminal => AtspiRole::Terminal,
        }
    }

    fn is_focused(&self) -> bool {
        self.0.is_focused()
    }

    pub(crate) fn state(&self, is_window_focused: bool) -> StateSet {
        let state = self.0;
        let atspi_role = self.role();
        let mut atspi_state = StateSet::empty();
        if is_window_focused
            && ((state.parent_id().is_none() && state.role() == Role::Window)
                || (state.is_dialog()
                    && state.tree_state.active_dialog().map(|d| d.id()) == Some(state.id())))
        {
            atspi_state.insert(State::Active);
        }
        if state.is_text_input() && !state.is_read_only() {
            atspi_state.insert(State::Editable);
        }
        // TODO: Focus and selection.
        if state.is_focusable(&filter) {
            atspi_state.insert(State::Focusable);
        }
        let filter_result = filter(self.0);
        if filter_result == FilterResult::Include {
            atspi_state.insert(State::Visible | State::Showing);
        }
        if state.is_required() {
            atspi_state.insert(State::Required);
        }
        if state.is_multiselectable() {
            atspi_state.insert(State::Multiselectable);
        }
        if let Some(expanded) = state.data().is_expanded() {
            atspi_state.insert(State::Expandable);
            if expanded {
                atspi_state.insert(State::Expanded);
            }
        }
        if let Some(orientation) = state.orientation() {
            atspi_state.insert(if orientation == Orientation::Horizontal {
                State::Horizontal
            } else {
                State::Vertical
            });
        }
        if atspi_role != AtspiRole::ToggleButton && state.toggled().is_some() {
            atspi_state.insert(State::Checkable);
        }
        if state.is_modal() {
            atspi_state.insert(State::Modal);
        }
        if let Some(selected) = state.is_selected() {
            if !state.is_disabled() {
                atspi_state.insert(State::Selectable);
            }
            if selected {
                atspi_state.insert(State::Selected);
            }
        }
        if state.is_text_input() {
            atspi_state.insert(State::SelectableText);
            atspi_state.insert(match state.is_multiline() {
                true => State::MultiLine,
                false => State::SingleLine,
            });
        }

        // Special case for indeterminate progressbar.
        if state.role() == Role::ProgressIndicator && state.numeric_value().is_none() {
            atspi_state.insert(State::Indeterminate);
        }

        // Toggled state
        match state.toggled() {
            Some(Toggled::Mixed) => atspi_state.insert(State::Indeterminate),
            Some(Toggled::True) if atspi_role == AtspiRole::ToggleButton => {
                atspi_state.insert(State::Pressed)
            }
            Some(Toggled::True) => atspi_state.insert(State::Checked),
            _ => {}
        }

        if state.is_read_only_supported() && state.is_read_only_or_disabled() {
            atspi_state.insert(State::ReadOnly);
        } else if !state.is_disabled() {
            atspi_state.insert(State::Enabled | State::Sensitive);
        }

        if self.is_focused() {
            atspi_state.insert(State::Focused);
        }

        atspi_state
    }

    fn placeholder(&self) -> Option<&str> {
        self.0.placeholder()
    }

    fn position_in_set(&self) -> Option<String> {
        self.0.position_in_set().map(|p| (p + 1).to_string())
    }

    fn size_of_set(&self) -> Option<String> {
        self.0
            .size_of_set_from_container(&filter)
            .map(|s| s.to_string())
    }

    fn braille_label(&self) -> Option<&str> {
        self.0.braille_label()
    }

    fn braille_role_description(&self) -> Option<&str> {
        self.0.braille_role_description()
    }

    fn attributes(&self) -> HashMap<&'static str, String> {
        let mut attributes = HashMap::new();
        if let Some(placeholder) = self.placeholder() {
            attributes.insert("placeholder-text", placeholder.to_string());
        }
        if let Some(position_in_set) = self.position_in_set() {
            attributes.insert("posinset", position_in_set);
        }
        if let Some(size_of_set) = self.size_of_set() {
            attributes.insert("setsize", size_of_set);
        }
        if let Some(label) = self.braille_label() {
            attributes.insert("braillelabel", label.to_string());
        }
        if let Some(role_description) = self.braille_role_description() {
            attributes.insert("brailleroledescription", role_description.to_string());
        }

        attributes
    }

    fn is_root(&self) -> bool {
        self.0.is_root()
    }

    fn supported_actions(&self) -> impl Iterator<Item = Action> + '_ {
        let enabled = !self.0.is_disabled();
        [
            self.0.is_clickable(&filter).then_some(Action::Click),
            self.0
                .data()
                .supports_action(Action::Expand)
                .then_some(Action::Expand),
            self.0
                .data()
                .supports_action(Action::Collapse)
                .then_some(Action::Collapse),
        ]
        .into_iter()
        .flatten()
        .filter(move |_| enabled)
    }

    fn action_at(&self, index: i32) -> Option<Action> {
        self.supported_actions().nth(usize::try_from(index).ok()?)
    }

    fn supports_action(&self) -> bool {
        self.supported_actions().next().is_some()
    }

    fn supports_component(&self) -> bool {
        self.0.raw_bounds().is_some() || self.is_root()
    }

    fn supports_selection(&self) -> bool {
        self.0.is_container_with_selectable_children()
    }

    fn supports_editable_text(&self) -> bool {
        self.0.is_text_input()
            && !self.0.is_read_only_or_disabled()
            && self.0.data().supports_action(Action::SetValue)
    }

    fn supports_text(&self) -> bool {
        self.0.supports_text_ranges()
    }

    fn supports_value(&self) -> bool {
        self.current_value().is_some()
    }

    pub(crate) fn interfaces(&self) -> InterfaceSet {
        let mut interfaces = InterfaceSet::new(Interface::Accessible);
        if self.supports_action() {
            interfaces.insert(Interface::Action);
        }
        if self.supports_component() {
            interfaces.insert(Interface::Component);
        }
        if self.supports_selection() {
            interfaces.insert(Interface::Selection);
        }
        if self.supports_text() {
            interfaces.insert(Interface::Text);
        }
        if self.supports_editable_text() {
            interfaces.insert(Interface::EditableText);
        }
        if self.supports_value() {
            interfaces.insert(Interface::Value);
        }
        interfaces
    }

    pub(crate) fn live(&self) -> Politeness {
        let live = self.0.live();
        match live {
            Live::Off => Politeness::None,
            Live::Polite => Politeness::Polite,
            Live::Assertive => Politeness::Assertive,
        }
    }

    fn n_actions(&self) -> i32 {
        self.supported_actions().count() as i32
    }

    fn get_action_name(&self, index: i32) -> String {
        match self.action_at(index) {
            Some(Action::Click) => "click",
            Some(Action::Expand) => "expand",
            Some(Action::Collapse) => "collapse",
            _ => "",
        }
        .to_owned()
    }

    fn raw_bounds_and_transform(&self) -> (Option<Rect>, Affine) {
        let state = self.0;
        (state.raw_bounds(), state.direct_transform())
    }

    fn extents(&self, window_bounds: &WindowBounds, coord_type: CoordType) -> Option<Rect> {
        let mut bounds = self.0.bounding_box();
        if self.is_root() {
            let window_bounds = window_bounds.inner.with_origin(Point::ZERO);
            if !window_bounds.is_empty() {
                if let Some(bounds) = &mut bounds {
                    bounds.x0 = bounds.x0.min(window_bounds.x1);
                    bounds.y0 = bounds.y0.min(window_bounds.y1);
                    bounds.x1 = bounds.x1.min(window_bounds.x1);
                    bounds.y1 = bounds.y1.min(window_bounds.y1);
                } else {
                    bounds = Some(window_bounds);
                }
            }
        }
        bounds.map(|bounds| {
            let new_origin = window_bounds.accesskit_point_to_atspi_point(
                bounds.origin(),
                self.0.filtered_parent(&filter),
                coord_type,
            );
            bounds.with_origin(new_origin)
        })
    }

    fn current_value(&self) -> Option<f64> {
        self.0.numeric_value()
    }

    fn active_descendant(&self) -> Option<NodeId> {
        fn find(node: Node<'_>, target: LocalNodeId) -> Option<NodeId> {
            // Raw child IDs and consumer children correspond within this
            // tree. A graft points into another tree and must not alias a
            // same-numbered local target there.
            if node.is_graft() {
                return None;
            }
            node.data()
                .children()
                .iter()
                .zip(node.children())
                .find_map(|(id, child)| match filter(&child) {
                    FilterResult::ExcludeSubtree => None,
                    FilterResult::Include if *id == target => Some(child.id()),
                    _ => find(child, target),
                })
        }
        let target = self.0.data().active_descendant()?;
        find(*self.0, target).or_else(|| self.0.controls().find_map(|node| find(node, target)))
    }

    pub(crate) fn notify_changes(
        &self,
        window_bounds: &WindowBounds,
        adapter: &Adapter,
        old: &NodeWrapper<'_>,
    ) {
        self.notify_state_changes(adapter, old);
        if self.0.data().active_descendant() != old.0.data().active_descendant() {
            if let Some(target) = self.active_descendant() {
                adapter.emit_object_event(self.id(), ObjectEvent::ActiveDescendantChanged(target));
            }
            // This event has no null target. Clearing the property must not
            // fabricate a descendant; normal focus/expanded events remain.
        }
        self.notify_property_changes(adapter, old);
        self.notify_bounds_changes(window_bounds, adapter, old);
        self.notify_children_changes(adapter, old);
    }

    fn notify_state_changes(&self, adapter: &Adapter, old: &NodeWrapper<'_>) {
        let old_state = old.state(true);
        let new_state = self.state(true);
        let changed_states = old_state ^ new_state;
        for state in changed_states.iter() {
            if state == State::Focused {
                // This is handled specially in `focus_moved`.
                continue;
            }
            adapter.emit_object_event(
                self.id(),
                ObjectEvent::StateChanged(state, new_state.contains(state)),
            );
        }
    }

    fn notify_property_changes(&self, adapter: &Adapter, old: &NodeWrapper<'_>) {
        let name = self.name();
        if name != old.name() {
            let name = name.unwrap_or_default();
            adapter.emit_object_event(
                self.id(),
                ObjectEvent::PropertyChanged(Property::Name(name.clone())),
            );

            let live = self.live();
            if live != Politeness::None {
                adapter.emit_object_event(self.id(), ObjectEvent::Announcement(name, live));
            }
        }
        let description = self.description();
        if description != old.description() {
            adapter.emit_object_event(
                self.id(),
                ObjectEvent::PropertyChanged(Property::Description(
                    description.unwrap_or_default(),
                )),
            );
        }
        let parent_id = self.parent_id();
        if parent_id != old.parent_id() {
            let parent = self
                .0
                .filtered_parent(&filter)
                .map_or(NodeIdOrRoot::Root, |node| NodeIdOrRoot::Node(node.id()));
            adapter.emit_object_event(
                self.id(),
                ObjectEvent::PropertyChanged(Property::Parent(parent)),
            );
        }
        let role = self.role();
        if role != old.role() {
            adapter.emit_object_event(
                self.id(),
                ObjectEvent::PropertyChanged(Property::Role(role)),
            );
        }
        if let Some(value) = self.current_value() {
            if Some(value) != old.current_value() {
                adapter.emit_object_event(
                    self.id(),
                    ObjectEvent::PropertyChanged(Property::Value(value)),
                );
            }
        }
    }

    fn notify_bounds_changes(
        &self,
        window_bounds: &WindowBounds,
        adapter: &Adapter,
        old: &NodeWrapper<'_>,
    ) {
        if self.raw_bounds_and_transform() != old.raw_bounds_and_transform() {
            if let Some(extents) = self.extents(window_bounds, CoordType::Window) {
                adapter.emit_object_event(self.id(), ObjectEvent::BoundsChanged(extents.into()));
            }
        }
    }

    fn notify_children_changes(&self, adapter: &Adapter, old: &NodeWrapper<'_>) {
        let old_filtered_children = old.filtered_child_ids().collect::<Vec<NodeId>>();
        let new_filtered_children = self.filtered_child_ids().collect::<Vec<NodeId>>();
        for (index, child) in new_filtered_children.iter().enumerate() {
            if !old_filtered_children.contains(child) {
                adapter.emit_object_event(self.id(), ObjectEvent::ChildAdded(index, *child));
            }
        }
        for child in old_filtered_children.into_iter() {
            if !new_filtered_children.contains(&child) {
                adapter.emit_object_event(self.id(), ObjectEvent::ChildRemoved(child));
            }
        }
    }
}

#[cfg(test)]
mod disabled_state_tests {
    use super::*;

    fn states(role: Role, disabled: bool, read_only: bool) -> StateSet {
        let mut node = accesskit::Node::new(role);
        if disabled {
            node.set_disabled();
        }
        if read_only {
            node.set_read_only();
        }
        let tree = Tree::new(
            accesskit::TreeUpdate {
                nodes: vec![(LocalNodeId(0), node)],
                tree: Some(accesskit::Tree::new(LocalNodeId(0))),
                tree_id: TreeId::ROOT,
                focus: LocalNodeId(0),
            },
            false,
        );
        NodeWrapper(&tree.state().root()).state(false)
    }

    #[test]
    fn disabled_action_controls_do_not_advertise_enabled_or_sensitive() {
        for role in [Role::Button, Role::DefaultButton, Role::Link] {
            for disabled in [false, true] {
                let states = states(role, disabled, false);
                assert_eq!(states.contains(State::Enabled), !disabled, "{role:?}");
                assert_eq!(states.contains(State::Sensitive), !disabled, "{role:?}");
            }
        }
    }

    #[test]
    fn read_only_text_state_behavior_is_preserved() {
        for read_only in [false, true] {
            for disabled in [false, true] {
                let states = states(Role::TextInput, disabled, read_only);
                assert_eq!(states.contains(State::ReadOnly), read_only || disabled);
                assert_eq!(states.contains(State::Enabled), !read_only && !disabled);
                assert_eq!(states.contains(State::Sensitive), !read_only && !disabled);
            }
        }
    }
}

#[derive(Clone)]
pub struct PlatformNode {
    context: Weak<Context>,
    adapter_id: usize,
    id: NodeId,
}

impl PlatformNode {
    pub(crate) fn new(context: &Arc<Context>, adapter_id: usize, id: NodeId) -> Self {
        Self {
            context: Arc::downgrade(context),
            adapter_id,
            id,
        }
    }

    fn from_adapter_root(adapter_id_and_context: &(usize, Arc<Context>)) -> Self {
        let (adapter_id, context) = adapter_id_and_context;
        Self::new(context, *adapter_id, context.read_tree().state().root_id())
    }

    fn upgrade_context(&self) -> Result<Arc<Context>> {
        if let Some(context) = self.context.upgrade() {
            Ok(context)
        } else {
            Err(Error::Defunct)
        }
    }

    fn with_tree_state<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&TreeState) -> Result<T>,
    {
        let context = self.upgrade_context()?;
        let tree = context.read_tree();
        f(tree.state())
    }

    fn with_tree_and_context<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Tree, &Context) -> Result<T>,
    {
        let context = self.upgrade_context()?;
        let tree = context.read_tree();
        f(&tree, &context)
    }

    fn resolve_with_context<F, T>(&self, f: F) -> Result<T>
    where
        for<'a> F: FnOnce(Node<'a>, &'a Tree, &Context) -> Result<T>,
    {
        self.with_tree_and_context(|tree, context| {
            if let Some(node) = tree.state().node_by_id(self.id) {
                f(node, tree, context)
            } else {
                Err(Error::Defunct)
            }
        })
    }

    fn resolve_for_selection_with_context<F, T>(&self, f: F) -> Result<T>
    where
        for<'a> F: FnOnce(Node<'a>, &'a Tree, &Context) -> Result<T>,
    {
        self.resolve_with_context(|node, tree, context| {
            let wrapper = NodeWrapper(&node);
            if wrapper.supports_selection() {
                f(node, tree, context)
            } else {
                Err(Error::UnsupportedInterface)
            }
        })
    }

    fn resolve_for_text_with_context<F, T>(&self, f: F) -> Result<T>
    where
        for<'a> F: FnOnce(Node<'a>, &'a Tree, &Context) -> Result<T>,
    {
        self.resolve_with_context(|node, tree, context| {
            let wrapper = NodeWrapper(&node);
            if wrapper.supports_text() {
                f(node, tree, context)
            } else {
                Err(Error::UnsupportedInterface)
            }
        })
    }

    fn resolve<F, T>(&self, f: F) -> Result<T>
    where
        for<'a> F: FnOnce(Node<'a>) -> Result<T>,
    {
        self.resolve_with_context(|node, _, _| f(node))
    }

    fn resolve_for_selection<F, T>(&self, f: F) -> Result<T>
    where
        for<'a> F: FnOnce(Node<'a>) -> Result<T>,
    {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            if wrapper.supports_selection() {
                f(node)
            } else {
                Err(Error::UnsupportedInterface)
            }
        })
    }

    fn resolve_for_text<F, T>(&self, f: F) -> Result<T>
    where
        for<'a> F: FnOnce(Node<'a>) -> Result<T>,
    {
        self.resolve_for_text_with_context(|node, _, _| f(node))
    }

    fn do_action_internal<F>(&self, target: NodeId, f: F) -> Result<()>
    where
        F: FnOnce(&TreeState, &Context, LocalNodeId, TreeId) -> ActionRequest,
    {
        let context = self.upgrade_context()?;
        let tree = context.read_tree();
        if let Some((target_node, target_tree)) = tree.locate_node(target) {
            let request = f(tree.state(), &context, target_node, target_tree);
            drop(tree);
            context.do_action(request);
            Ok(())
        } else {
            Err(Error::Defunct)
        }
    }

    pub fn name(&self) -> Result<String> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.name().unwrap_or_default())
        })
    }

    pub fn description(&self) -> Result<String> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.description().unwrap_or_default())
        })
    }

    pub fn relative(&self, id: NodeId) -> Self {
        Self {
            context: self.context.clone(),
            adapter_id: self.adapter_id,
            id,
        }
    }

    pub fn root(&self) -> Result<PlatformRoot> {
        let context = self.upgrade_context()?;
        Ok(PlatformRoot::new(&context.app_context))
    }

    pub fn toolkit_name(&self) -> Result<String> {
        self.with_tree_state(|state| Ok(state.toolkit_name().unwrap_or_default().to_string()))
    }

    pub fn toolkit_version(&self) -> Result<String> {
        self.with_tree_state(|state| Ok(state.toolkit_version().unwrap_or_default().to_string()))
    }

    pub fn parent(&self) -> Result<NodeIdOrRoot> {
        self.resolve(|node| {
            let parent = node
                .filtered_parent(&filter)
                .map_or(NodeIdOrRoot::Root, |node| NodeIdOrRoot::Node(node.id()));
            Ok(parent)
        })
    }

    pub fn child_count(&self) -> Result<i32> {
        self.resolve(|node| {
            i32::try_from(node.filtered_children(&filter).count())
                .map_err(|_| Error::TooManyChildren)
        })
    }

    pub fn adapter_id(&self) -> usize {
        self.adapter_id
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn accessible_id(&self) -> Result<String> {
        self.resolve(|node| {
            if let Some(author_id) = node.author_id() {
                Ok(author_id.to_string())
            } else {
                Ok(String::new())
            }
        })
    }

    pub fn child_at_index(&self, index: usize) -> Result<Option<NodeId>> {
        self.resolve(|node| {
            let child = node
                .filtered_children(&filter)
                .nth(index)
                .map(|child| child.id());
            Ok(child)
        })
    }

    pub fn map_children<T, I>(&self, f: impl Fn(NodeId) -> I) -> Result<T>
    where
        T: FromIterator<I>,
    {
        self.resolve(|node| {
            let children = node
                .filtered_children(&filter)
                .map(|child| child.id())
                .map(f)
                .collect();
            Ok(children)
        })
    }

    pub fn index_in_parent(&self) -> Result<i32> {
        self.resolve(|node| {
            i32::try_from(node.preceding_filtered_siblings(&filter).count())
                .map_err(|_| Error::IndexOutOfRange)
        })
    }

    pub fn relation_set<T>(
        &self,
        f: impl Fn(NodeId) -> T,
    ) -> Result<HashMap<RelationType, Vec<T>>> {
        self.resolve(|node| {
            let mut relations = HashMap::new();
            let controls: Vec<_> = node
                .controls()
                .filter(|controlled| filter(controlled) == FilterResult::Include)
                .map(|controlled| controlled.id())
                .map(f)
                .collect();
            if !controls.is_empty() {
                relations.insert(RelationType::ControllerFor, controls);
            }
            Ok(relations)
        })
    }

    pub fn role(&self) -> Result<AtspiRole> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.role())
        })
    }

    pub fn localized_role_name(&self) -> Result<String> {
        self.resolve(|node| Ok(node.role_description().unwrap_or_default().to_string()))
    }

    pub fn state(&self) -> StateSet {
        self.resolve_with_context(|node, tree, _| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.state(tree.state().focus_id().is_some()))
        })
        .unwrap_or(State::Defunct.into())
    }

    pub fn attributes(&self) -> Result<HashMap<&'static str, String>> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.attributes())
        })
    }

    pub fn supports_action(&self) -> Result<bool> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.supports_action())
        })
    }

    pub fn supports_component(&self) -> Result<bool> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.supports_component())
        })
    }

    pub fn supports_selection(&self) -> Result<bool> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.supports_selection())
        })
    }

    pub fn supports_text(&self) -> Result<bool> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.supports_text())
        })
    }

    pub fn supports_value(&self) -> Result<bool> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.supports_value())
        })
    }

    pub fn interfaces(&self) -> Result<InterfaceSet> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.interfaces())
        })
    }

    pub fn n_actions(&self) -> Result<i32> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.n_actions())
        })
    }

    pub fn action_name(&self, index: i32) -> Result<String> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.get_action_name(index))
        })
    }

    pub fn actions(&self) -> Result<Vec<AtspiAction>> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            let n_actions = wrapper.n_actions() as usize;
            let mut actions = Vec::with_capacity(n_actions);
            for i in 0..n_actions {
                actions.push(AtspiAction {
                    localized_name: wrapper.get_action_name(i as i32),
                    description: "".into(),
                    key_binding: "".into(),
                });
            }
            Ok(actions)
        })
    }

    pub fn do_action(&self, index: i32) -> Result<bool> {
        let context = self.upgrade_context()?;
        let request = {
            let tree = context.read_tree();
            let node = tree.state().node_by_id(self.id).ok_or(Error::Defunct)?;
            let Some(action) = NodeWrapper(&node).action_at(index) else {
                return Ok(false);
            };
            let (target_node, target_tree) = tree.locate_node(self.id).ok_or(Error::Defunct)?;
            ActionRequest {
                action,
                target_tree,
                target_node,
                data: None,
            }
        };
        // Release the tree read lock before calling application code.
        context.do_action(request);
        Ok(true)
    }

    pub fn contains(&self, x: i32, y: i32, coord_type: CoordType) -> Result<bool> {
        self.resolve_with_context(|node, _, context| {
            let window_bounds = context.read_root_window_bounds();
            let wrapper = NodeWrapper(&node);
            if let Some(extents) = wrapper.extents(&window_bounds, coord_type) {
                Ok(extents.contains(Point::new(x.into(), y.into())))
            } else {
                Ok(false)
            }
        })
    }

    pub fn accessible_at_point(
        &self,
        x: i32,
        y: i32,
        coord_type: CoordType,
    ) -> Result<Option<NodeId>> {
        self.resolve_with_context(|node, _, context| {
            let window_bounds = context.read_root_window_bounds();
            let point = window_bounds.atspi_point_to_accesskit_point(
                Point::new(x.into(), y.into()),
                Some(node),
                coord_type,
            );
            let point = node.transform().inverse() * point;
            Ok(node.node_at_point(point, &filter).map(|node| node.id()))
        })
    }

    pub fn extents(&self, coord_type: CoordType) -> Result<AtspiRect> {
        self.resolve_with_context(|node, _, context| {
            let window_bounds = context.read_root_window_bounds();
            let wrapper = NodeWrapper(&node);
            Ok(wrapper
                .extents(&window_bounds, coord_type)
                .map_or(AtspiRect::INVALID, AtspiRect::from))
        })
    }

    pub fn layer(&self) -> Result<Layer> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            if wrapper.role() == AtspiRole::Window {
                Ok(Layer::Window)
            } else {
                Ok(Layer::Widget)
            }
        })
    }

    pub fn grab_focus(&self) -> Result<bool> {
        self.do_action_internal(self.id, |_, _, target_node, target_tree| ActionRequest {
            action: Action::Focus,
            target_tree,
            target_node,
            data: None,
        })?;
        Ok(true)
    }

    pub fn scroll_to(&self, scroll_type: ScrollType) -> Result<bool> {
        self.do_action_internal(self.id, |_, _, target_node, target_tree| ActionRequest {
            action: Action::ScrollIntoView,
            target_tree,
            target_node,
            data: atspi_scroll_type_to_scroll_hint(scroll_type).map(ActionData::ScrollHint),
        })?;
        Ok(true)
    }

    pub fn scroll_to_point(&self, coord_type: CoordType, x: i32, y: i32) -> Result<bool> {
        self.resolve_with_context(|node, tree, context| {
            let window_bounds = context.read_root_window_bounds();
            let point = window_bounds.atspi_point_to_accesskit_point(
                Point::new(x.into(), y.into()),
                node.filtered_parent(&filter),
                coord_type,
            );
            let (target_node, target_tree) = tree.locate_node(self.id).ok_or(Error::Defunct)?;
            context.do_action(ActionRequest {
                action: Action::ScrollToPoint,
                target_tree,
                target_node,
                data: Some(ActionData::ScrollToPoint(point)),
            });
            Ok(())
        })?;
        Ok(true)
    }

    pub fn n_selected_children(&self) -> Result<i32> {
        self.resolve_for_selection(|node| {
            node.items(filter)
                .filter(|item| item.is_selected() == Some(true))
                .count()
                .try_into()
                .map_err(|_| Error::TooManyChildren)
        })
    }

    pub fn selected_child(&self, selected_child_index: usize) -> Result<Option<NodeId>> {
        self.resolve_for_selection(|node| {
            Ok(node
                .items(filter)
                .filter(|item| item.is_selected() == Some(true))
                .nth(selected_child_index)
                .map(|node| node.id()))
        })
    }

    pub fn select_child(&self, child_index: usize) -> Result<bool> {
        self.resolve_for_selection_with_context(|node, tree, context| {
            if let Some(child) = node.filtered_children(filter).nth(child_index) {
                if let Some(true) = child.is_selected() {
                    Ok(true)
                } else if child.is_selectable() && child.is_clickable(&filter) {
                    let (target_node, target_tree) =
                        tree.locate_node(child.id()).ok_or(Error::Defunct)?;
                    context.do_action(ActionRequest {
                        action: Action::Click,
                        target_tree,
                        target_node,
                        data: None,
                    });
                    Ok(true)
                } else {
                    Ok(false)
                }
            } else {
                Err(Error::Defunct)
            }
        })
    }

    pub fn deselect_selected_child(&self, selected_child_index: usize) -> Result<bool> {
        self.resolve_for_selection_with_context(|node, tree, context| {
            if let Some(child) = node
                .items(filter)
                .filter(|c| c.is_selected() == Some(true))
                .nth(selected_child_index)
            {
                if child.is_clickable(&filter) {
                    let (target_node, target_tree) =
                        tree.locate_node(child.id()).ok_or(Error::Defunct)?;
                    context.do_action(ActionRequest {
                        action: Action::Click,
                        target_tree,
                        target_node,
                        data: None,
                    });
                    Ok(true)
                } else {
                    Ok(false)
                }
            } else {
                Err(Error::Defunct)
            }
        })
    }

    pub fn is_child_selected(&self, child_index: usize) -> Result<bool> {
        self.resolve_for_selection(|node| {
            node.filtered_children(filter)
                .nth(child_index)
                .map(|child| child.is_item_like() && child.is_selected() == Some(true))
                .ok_or(Error::Defunct)
        })
    }

    pub fn select_all(&self) -> Result<bool> {
        // We don't support selecting all children at once.
        Ok(false)
    }

    pub fn clear_selection(&self) -> Result<bool> {
        // We don't support deselecting all children at once.
        Ok(false)
    }

    pub fn deselect_child(&self, child_index: usize) -> Result<bool> {
        self.resolve_for_selection_with_context(|node, tree, context| {
            if let Some(child) = node.filtered_children(filter).nth(child_index) {
                if let Some(false) = child.is_selected() {
                    Ok(true)
                } else if child.is_selectable() && child.is_clickable(&filter) {
                    let (target_node, target_tree) =
                        tree.locate_node(child.id()).ok_or(Error::Defunct)?;
                    context.do_action(ActionRequest {
                        action: Action::Click,
                        target_tree,
                        target_node,
                        data: None,
                    });
                    Ok(true)
                } else {
                    Ok(false)
                }
            } else {
                Err(Error::Defunct)
            }
        })
    }

    pub fn character_count(&self) -> Result<i32> {
        self.resolve_for_text(|node| {
            node.document_range()
                .end()
                .to_global_usv_index()
                .try_into()
                .map_err(|_| Error::TooManyCharacters)
        })
    }

    pub fn caret_offset(&self) -> Result<i32> {
        self.resolve_for_text(|node| {
            node.text_selection_focus().map_or(Ok(-1), |focus| {
                focus
                    .to_global_usv_index()
                    .try_into()
                    .map_err(|_| Error::TooManyCharacters)
            })
        })
    }

    pub fn string_at_offset(
        &self,
        offset: i32,
        granularity: Granularity,
    ) -> Result<(String, i32, i32)> {
        self.resolve_for_text(|node| {
            let range = text_range_from_offset(&node, offset, granularity)?;
            let text = range.text();
            let start = range
                .start()
                .to_global_usv_index()
                .try_into()
                .map_err(|_| Error::TooManyCharacters)?;
            let end = range
                .end()
                .to_global_usv_index()
                .try_into()
                .map_err(|_| Error::TooManyCharacters)?;

            Ok((text, start, end))
        })
    }

    pub fn text(&self, start_offset: i32, end_offset: i32) -> Result<String> {
        self.resolve_for_text(|node| {
            let range = text_range_from_offsets(&node, start_offset, end_offset)
                .ok_or(Error::IndexOutOfRange)?;
            Ok(range.text())
        })
    }

    pub fn set_caret_offset(&self, offset: i32) -> Result<bool> {
        self.resolve_for_text_with_context(|node, tree, context| {
            if !node.data().supports_action(Action::SetTextSelection) {
                return Ok(false);
            }
            let offset = text_position_from_offset(&node, offset).ok_or(Error::IndexOutOfRange)?;
            let (target_node, target_tree) = tree.locate_node(node.id()).ok_or(Error::Defunct)?;
            context.do_action(ActionRequest {
                action: Action::SetTextSelection,
                target_tree,
                target_node,
                data: Some(ActionData::SetTextSelection(
                    offset.to_degenerate_range().to_text_selection(),
                )),
            });
            Ok(true)
        })
    }

    pub fn text_attribute_value(&self, _offset: i32, _attribute_name: &str) -> Result<String> {
        // TODO: Implement rich text.
        Err(Error::UnsupportedInterface)
    }

    pub fn text_attributes(&self, _offset: i32) -> Result<(HashMap<String, String>, i32, i32)> {
        // TODO: Implement rich text.
        Err(Error::UnsupportedInterface)
    }

    pub fn default_text_attributes(&self) -> Result<HashMap<String, String>> {
        // TODO: Implement rich text.
        Err(Error::UnsupportedInterface)
    }

    pub fn character_extents(&self, offset: i32, coord_type: CoordType) -> Result<AtspiRect> {
        self.resolve_for_text_with_context(|node, _, context| {
            let range = text_range_from_offset(&node, offset, Granularity::Char)?;
            if let Some(bounds) = range.bounding_boxes().first() {
                let window_bounds = context.read_root_window_bounds();
                let new_origin = window_bounds.accesskit_point_to_atspi_point(
                    bounds.origin(),
                    Some(node),
                    coord_type,
                );
                Ok(bounds.with_origin(new_origin).into())
            } else {
                Ok(AtspiRect::INVALID)
            }
        })
    }

    pub fn offset_at_point(&self, x: i32, y: i32, coord_type: CoordType) -> Result<i32> {
        self.resolve_for_text_with_context(|node, _, context| {
            let window_bounds = context.read_root_window_bounds();
            let point = window_bounds.atspi_point_to_accesskit_point(
                Point::new(x.into(), y.into()),
                Some(node),
                coord_type,
            );
            let point = node.transform().inverse() * point;
            node.text_position_at_point(point)
                .to_global_usv_index()
                .try_into()
                .map_err(|_| Error::TooManyCharacters)
        })
    }

    pub fn n_selections(&self) -> Result<i32> {
        self.resolve_for_text(|node| {
            match node.text_selection().filter(|range| !range.is_degenerate()) {
                Some(_) => Ok(1),
                None => Ok(0),
            }
        })
    }

    pub fn selection(&self, selection_num: i32) -> Result<(i32, i32)> {
        if selection_num != 0 {
            return Ok((-1, -1));
        }

        self.resolve_for_text(|node| {
            node.text_selection()
                .filter(|range| !range.is_degenerate())
                .map_or(Ok((-1, -1)), |range| {
                    let start = range
                        .start()
                        .to_global_usv_index()
                        .try_into()
                        .map_err(|_| Error::TooManyCharacters)?;
                    let end = range
                        .end()
                        .to_global_usv_index()
                        .try_into()
                        .map_err(|_| Error::TooManyCharacters)?;

                    Ok((start, end))
                })
        })
    }

    pub fn add_selection(&self, start_offset: i32, end_offset: i32) -> Result<bool> {
        // We only support one selection.
        self.set_selection(0, start_offset, end_offset)
    }

    pub fn remove_selection(&self, selection_num: i32) -> Result<bool> {
        if selection_num != 0 {
            return Ok(false);
        }

        self.resolve_for_text_with_context(|node, tree, context| {
            if !node.data().supports_action(Action::SetTextSelection) {
                return Ok(false);
            }
            // Simply collapse the selection to the position of the caret if a caret is
            // visible, otherwise set the selection to 0.
            let selection_end = node
                .text_selection_focus()
                .unwrap_or_else(|| node.document_range().start());
            let (target_node, target_tree) = tree.locate_node(node.id()).ok_or(Error::Defunct)?;
            context.do_action(ActionRequest {
                action: Action::SetTextSelection,
                target_tree,
                target_node,
                data: Some(ActionData::SetTextSelection(
                    selection_end.to_degenerate_range().to_text_selection(),
                )),
            });
            Ok(true)
        })
    }

    pub fn set_selection(
        &self,
        selection_num: i32,
        start_offset: i32,
        end_offset: i32,
    ) -> Result<bool> {
        if selection_num != 0 {
            return Ok(false);
        }

        self.resolve_for_text_with_context(|node, tree, context| {
            if !node.data().supports_action(Action::SetTextSelection) {
                return Ok(false);
            }
            let range = text_range_from_offsets(&node, start_offset, end_offset)
                .ok_or(Error::IndexOutOfRange)?;
            let (target_node, target_tree) = tree.locate_node(node.id()).ok_or(Error::Defunct)?;
            context.do_action(ActionRequest {
                action: Action::SetTextSelection,
                target_tree,
                target_node,
                data: Some(ActionData::SetTextSelection(range.to_text_selection())),
            });
            Ok(true)
        })
    }

    pub fn range_extents(
        &self,
        start_offset: i32,
        end_offset: i32,
        coord_type: CoordType,
    ) -> Result<AtspiRect> {
        self.resolve_for_text_with_context(|node, _, context| {
            if let Some(rect) = text_range_bounds_from_offsets(&node, start_offset, end_offset) {
                let window_bounds = context.read_root_window_bounds();
                let new_origin = window_bounds.accesskit_point_to_atspi_point(
                    rect.origin(),
                    Some(node),
                    coord_type,
                );
                Ok(rect.with_origin(new_origin).into())
            } else {
                Ok(AtspiRect::INVALID)
            }
        })
    }

    pub fn text_attribute_run(
        &self,
        _offset: i32,
        _include_defaults: bool,
    ) -> Result<(HashMap<String, String>, i32, i32)> {
        // TODO: Implement rich text.
        // For now, just report a range spanning the entire text with no attributes,
        // this is required by Orca to announce selection content and caret movements.
        let character_count = self.character_count()?;
        Ok((HashMap::new(), 0, character_count))
    }

    pub fn scroll_substring_to(
        &self,
        start_offset: i32,
        end_offset: i32,
        scroll_type: ScrollType,
    ) -> Result<bool> {
        self.resolve_for_text_with_context(|node, tree, context| {
            if let Some(range) = text_range_from_offsets(&node, start_offset, end_offset) {
                let position = if matches!(
                    scroll_type,
                    ScrollType::BottomRight | ScrollType::BottomEdge | ScrollType::RightEdge
                ) {
                    range.end()
                } else {
                    range.start()
                };
                let (target_node, target_tree) = tree
                    .locate_node(position.inner_node().id())
                    .ok_or(Error::Defunct)?;
                context.do_action(ActionRequest {
                    action: Action::ScrollIntoView,
                    target_tree,
                    target_node,
                    data: atspi_scroll_type_to_scroll_hint(scroll_type).map(ActionData::ScrollHint),
                });
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    pub fn scroll_substring_to_point(
        &self,
        start_offset: i32,
        end_offset: i32,
        coord_type: CoordType,
        x: i32,
        y: i32,
    ) -> Result<bool> {
        self.resolve_for_text_with_context(|node, tree, context| {
            let window_bounds = context.read_root_window_bounds();
            let target_point = window_bounds.atspi_point_to_accesskit_point(
                Point::new(x.into(), y.into()),
                Some(node),
                coord_type,
            );

            if let Some(rect) = text_range_bounds_from_offsets(&node, start_offset, end_offset) {
                let point = Point::new(target_point.x - rect.x0, target_point.y - rect.y0);
                let (target_node, target_tree) =
                    tree.locate_node(node.id()).ok_or(Error::Defunct)?;
                context.do_action(ActionRequest {
                    action: Action::ScrollToPoint,
                    target_tree,
                    target_node,
                    data: Some(ActionData::ScrollToPoint(point)),
                });
                return Ok(true);
            }
            Ok(false)
        })
    }

    /// String replacement is distinct from the numeric Value interface.
    /// Only controls explicitly advertising SetValue accept this operation.
    pub fn set_text_contents(&self, value: &str) -> Result<bool> {
        self.resolve_with_context(|node, tree, context| {
            if !NodeWrapper(&node).supports_editable_text() {
                return Ok(false);
            }
            let (target_node, target_tree) = tree.locate_node(node.id()).ok_or(Error::Defunct)?;
            context.do_action(ActionRequest {
                action: Action::SetValue,
                target_tree,
                target_node,
                data: Some(ActionData::Value(value.into())),
            });
            Ok(true)
        })
    }

    pub fn minimum_value(&self) -> Result<f64> {
        self.resolve(|node| Ok(node.min_numeric_value().unwrap_or(f64::MIN)))
    }

    pub fn maximum_value(&self) -> Result<f64> {
        self.resolve(|node| Ok(node.max_numeric_value().unwrap_or(f64::MAX)))
    }

    pub fn minimum_increment(&self) -> Result<f64> {
        self.resolve(|node| Ok(node.numeric_value_step().unwrap_or(0.0)))
    }

    pub fn current_value(&self) -> Result<f64> {
        self.resolve(|node| {
            let wrapper = NodeWrapper(&node);
            Ok(wrapper.current_value().unwrap_or(0.0))
        })
    }

    pub fn set_current_value(&self, value: f64) -> Result<()> {
        self.do_action_internal(self.id, |_, _, target_node, target_tree| ActionRequest {
            action: Action::SetValue,
            target_tree,
            target_node,
            data: Some(ActionData::NumericValue(value)),
        })
    }
}

impl PartialEq for PlatformNode {
    fn eq(&self, other: &Self) -> bool {
        self.adapter_id == other.adapter_id && self.id == other.id
    }
}

impl Eq for PlatformNode {}

impl Hash for PlatformNode {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.adapter_id.hash(state);
        self.id.hash(state);
    }
}

#[derive(Clone)]
pub struct PlatformRoot {
    app_context: Weak<RwLock<AppContext>>,
}

impl PlatformRoot {
    pub fn new(app_context: &Arc<RwLock<AppContext>>) -> Self {
        Self {
            app_context: Arc::downgrade(app_context),
        }
    }

    fn resolve_app_context<F, T>(&self, f: F) -> Result<T>
    where
        for<'a> F: FnOnce(RwLockReadGuard<'a, AppContext>) -> Result<T>,
    {
        let app_context = match self.app_context.upgrade() {
            Some(context) => context,
            None => return Err(Error::Defunct),
        };
        let app_context = app_context.read().unwrap();
        f(app_context)
    }

    pub fn name(&self) -> Result<String> {
        self.resolve_app_context(|context| Ok(context.name.clone().unwrap_or_default()))
    }

    pub fn child_count(&self) -> Result<i32> {
        self.resolve_app_context(|context| {
            i32::try_from(context.adapters.len()).map_err(|_| Error::TooManyChildren)
        })
    }

    pub fn child_at_index(&self, index: usize) -> Result<Option<PlatformNode>> {
        self.resolve_app_context(|context| {
            let child = context
                .adapters
                .get(index)
                .map(PlatformNode::from_adapter_root);
            Ok(child)
        })
    }

    pub fn child_id_at_index(&self, index: usize) -> Result<Option<(usize, NodeId)>> {
        self.resolve_app_context(|context| {
            let child = context
                .adapters
                .get(index)
                .map(|(adapter_id, context)| (*adapter_id, context.read_tree().state().root_id()));
            Ok(child)
        })
    }

    pub fn map_children<T, I>(&self, f: impl Fn(PlatformNode) -> I) -> Result<T>
    where
        T: FromIterator<I>,
    {
        self.resolve_app_context(|context| {
            let children = context
                .adapters
                .iter()
                .map(PlatformNode::from_adapter_root)
                .map(f)
                .collect();
            Ok(children)
        })
    }

    pub fn map_child_ids<T, I>(&self, f: impl Fn((usize, NodeId)) -> I) -> Result<T>
    where
        T: FromIterator<I>,
    {
        self.resolve_app_context(|context| {
            let children = context
                .adapters
                .iter()
                .map(|(adapter_id, context)| (*adapter_id, context.read_tree().state().root_id()))
                .map(f)
                .collect();
            Ok(children)
        })
    }

    pub fn toolkit_name(&self) -> Result<String> {
        self.resolve_app_context(|context| Ok(context.toolkit_name.clone().unwrap_or_default()))
    }

    pub fn toolkit_version(&self) -> Result<String> {
        self.resolve_app_context(|context| Ok(context.toolkit_version.clone().unwrap_or_default()))
    }

    pub fn id(&self) -> Result<i32> {
        self.resolve_app_context(|context| Ok(context.id.unwrap_or(-1)))
    }

    pub fn set_id(&mut self, id: i32) -> Result<()> {
        let app_context = match self.app_context.upgrade() {
            Some(context) => context,
            None => return Err(Error::Defunct),
        };
        let mut app_context = app_context.write().unwrap();
        app_context.id = Some(id);
        Ok(())
    }
}

impl PartialEq for PlatformRoot {
    fn eq(&self, other: &Self) -> bool {
        self.app_context.ptr_eq(&other.app_context)
    }
}

impl Hash for PlatformRoot {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.app_context.as_ptr().hash(state);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeIdOrRoot {
    Node(NodeId),
    Root,
}

#[cfg(test)]
mod editable_text_tests {
    use super::*;
    use std::sync::Mutex;
    struct Callback;
    impl crate::AdapterCallback for Callback {
        fn register_interfaces(&self, _: &crate::Adapter, _: NodeId, _: InterfaceSet) {}
        fn unregister_interfaces(&self, _: &crate::Adapter, _: NodeId, _: InterfaceSet) {}
        fn emit_event(&self, _: &crate::Adapter, _: crate::Event) {}
    }
    struct Capture(Arc<Mutex<Vec<ActionRequest>>>);
    impl accesskit::ActionHandler for Capture {
        fn do_action(&mut self, request: ActionRequest) {
            self.0.lock().unwrap().push(request);
        }
    }
    #[test]
    fn text_replacement_dispatches_string_data_and_refuses_read_only_controls() {
        for read_only in [false, true] {
            let mut node = accesskit::Node::new(Role::TextInput);
            node.add_action(Action::SetValue);
            if read_only {
                node.set_read_only();
            }
            let actions = Arc::new(Mutex::new(Vec::new()));
            let app = AppContext::new(Some("private-test".into()));
            let adapter = crate::Adapter::new(
                &app,
                Callback,
                accesskit::TreeUpdate {
                    nodes: vec![(accesskit::NodeId(0), node)],
                    tree: Some(accesskit::Tree::new(accesskit::NodeId(0))),
                    tree_id: accesskit::TreeId::ROOT,
                    focus: accesskit::NodeId(0),
                },
                false,
                crate::WindowBounds::default(),
                Capture(actions.clone()),
            );
            let field = adapter.platform_node(adapter.root_id());
            assert_eq!(
                field.set_text_contents("é replacement").unwrap(),
                !read_only
            );
            let actions = actions.lock().unwrap();
            if read_only {
                assert!(actions.is_empty());
            } else {
                assert!(
                    matches!(actions.as_slice(), [ActionRequest { action: Action::SetValue, data: Some(ActionData::Value(value)), .. }] if value.as_ref() == "é replacement")
                );
            }
        }
    }
    #[test]
    fn only_explicit_writable_string_controls_advertise_editable_text() {
        for (role, set_value, read_only, expected) in [
            (Role::TextInput, true, false, true),
            (Role::MultilineTextInput, true, false, true),
            (Role::PasswordInput, true, false, true),
            (Role::TextInput, false, false, false),
            (Role::TextInput, true, true, false),
            (Role::Slider, true, false, false),
        ] {
            let mut node = accesskit::Node::new(role);
            if set_value {
                node.add_action(Action::SetValue);
            }
            if read_only {
                node.set_read_only();
            }
            let tree = accesskit_consumer::Tree::new(
                accesskit::TreeUpdate {
                    nodes: vec![(accesskit::NodeId(0), node)],
                    tree: Some(accesskit::Tree::new(accesskit::NodeId(0))),
                    tree_id: accesskit::TreeId::ROOT,
                    focus: accesskit::NodeId(0),
                },
                false,
            );
            assert_eq!(
                NodeWrapper(&tree.state().root())
                    .interfaces()
                    .contains(Interface::EditableText),
                expected
            );
        }
    }
}

// In-process platform projection/callback contracts. These do not start a
// native window, AT-SPI service, accessibility client, or screen reader.
#[cfg(test)]
mod dropdown_accessibility_tests {
    use super::*;
    use std::sync::Mutex;

    struct Callback(Arc<Mutex<Vec<crate::Event>>>);
    impl crate::AdapterCallback for Callback {
        fn register_interfaces(&self, _: &crate::Adapter, _: NodeId, _: InterfaceSet) {}
        fn unregister_interfaces(&self, _: &crate::Adapter, _: NodeId, _: InterfaceSet) {}
        fn emit_event(&self, _: &crate::Adapter, event: crate::Event) {
            self.0.lock().unwrap().push(event);
        }
    }
    struct Capture(Arc<Mutex<Vec<ActionRequest>>>);
    impl accesskit::ActionHandler for Capture {
        fn do_action(&mut self, request: ActionRequest) {
            self.0.lock().unwrap().push(request);
        }
    }
    fn update(node: accesskit::Node) -> accesskit::TreeUpdate {
        accesskit::TreeUpdate {
            nodes: vec![(LocalNodeId(0), node)],
            tree: Some(accesskit::Tree::new(LocalNodeId(0))),
            tree_id: TreeId::ROOT,
            focus: LocalNodeId(0),
        }
    }
    fn combo(expanded: bool) -> accesskit::Node {
        let mut node = accesskit::Node::new(Role::ComboBox);
        node.set_label("Settings category");
        node.set_expanded(expanded);
        for action in [Action::Click, Action::Expand, Action::Collapse] {
            node.add_action(action);
        }
        node
    }
    fn adapter(
        state: accesskit::TreeUpdate,
        events: Arc<Mutex<Vec<crate::Event>>>,
        actions: Arc<Mutex<Vec<ActionRequest>>>,
    ) -> crate::Adapter {
        let app = AppContext::new(Some("private-dropdown-test".into()));
        crate::Adapter::new(
            &app,
            Callback(events),
            state,
            false,
            crate::WindowBounds::default(),
            Capture(actions),
        )
    }

    #[test]
    fn native_dropdown_actions_keep_click_index_and_dispatch_stable_expand_collapse() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let actions = Arc::new(Mutex::new(Vec::new()));
        let mut adapter = adapter(update(combo(false)), events.clone(), actions.clone());
        let field = adapter.platform_node(adapter.root_id());
        assert_eq!(field.n_actions().unwrap(), 3);
        assert_eq!(field.action_name(0).unwrap(), "click");
        assert_eq!(field.action_name(1).unwrap(), "expand");
        assert_eq!(field.action_name(2).unwrap(), "collapse");
        assert!(field.state().contains(State::Expandable));
        assert!(!field.state().contains(State::Expanded));
        assert!(field.do_action(1).unwrap());
        events.lock().unwrap().clear();
        adapter.update(update(combo(true)));
        assert_eq!(field.action_name(1).unwrap(), "expand");
        assert!(field.do_action(1).unwrap());
        assert!(field.do_action(2).unwrap());
        assert!(field.state().contains(State::Expanded));
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            crate::Event::Object {
                event: ObjectEvent::StateChanged(State::Expanded, true),
                ..
            }
        )));
        let actions = actions.lock().unwrap();
        assert_eq!(
            actions
                .iter()
                .map(|request| request.action)
                .collect::<Vec<_>>(),
            [Action::Expand, Action::Expand, Action::Collapse]
        );
        assert!(
            actions
                .iter()
                .all(|request| request.target_node == LocalNodeId(0)
                    && request.target_tree == TreeId::ROOT
                    && request.data.is_none())
        );
    }

    #[test]
    fn native_dropdown_actions_refuse_disabled_unsupported_and_invalid_indices() {
        for disabled in [false, true] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let actions = Arc::new(Mutex::new(Vec::new()));
            let mut node = accesskit::Node::new(Role::ListBox);
            node.add_action(Action::Collapse);
            if disabled {
                node.set_disabled();
            }
            let adapter = adapter(update(node), events, actions.clone());
            let list = adapter.platform_node(adapter.root_id());
            assert_eq!(list.supports_action().unwrap(), !disabled);
            assert_eq!(list.n_actions().unwrap(), if disabled { 0 } else { 1 });
            assert_eq!(
                list.action_name(0).unwrap(),
                if disabled { "" } else { "collapse" }
            );
            assert!(!list.do_action(-1).unwrap());
            assert!(!list.do_action(1).unwrap());
            assert!(!list.do_action(i32::MAX).unwrap());
            assert_eq!(list.do_action(0).unwrap(), !disabled);
            assert_eq!(actions.lock().unwrap().len(), usize::from(!disabled));
            assert!(
                actions
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|request| request.action == Action::Collapse)
            );
        }
        let actions = Arc::new(Mutex::new(Vec::new()));
        let adapter = adapter(
            update(accesskit::Node::new(Role::ComboBox)),
            Default::default(),
            actions.clone(),
        );
        let field = adapter.platform_node(adapter.root_id());
        assert!(!field.supports_action().unwrap());
        assert!(!field.do_action(0).unwrap());
        assert!(actions.lock().unwrap().is_empty());
        assert!(!field.state().contains(State::Expandable));
    }
    fn selection_update(
        open: bool,
        scroll: f64,
        selected: Option<usize>,
        display: u64,
    ) -> accesskit::TreeUpdate {
        let labels = ["Appearance", "Terminal", "Editor"];
        let mut window = accesskit::Node::new(Role::Window);
        window.set_children(if open {
            vec![LocalNodeId(1), LocalNodeId(3)]
        } else {
            vec![LocalNodeId(1)]
        });
        let mut field = combo(open);
        field.set_bounds(Rect::new(0.0, 0.0, 200.0, 32.0));
        let mut nodes = Vec::new();
        if let Some(label) = selected.and_then(|index| labels.get(index)) {
            field.set_value(*label);
            field.set_children(vec![LocalNodeId(display)]);
            let mut value = accesskit::Node::new(Role::ListBoxOption);
            value.set_label(*label);
            value.set_selected(true);
            value.set_read_only();
            value.set_bounds(Rect::new(8.0, 7.0, 190.0, 25.0));
            nodes.push((LocalNodeId(display), value));
        }
        if open {
            field.set_controls(vec![LocalNodeId(3)]);
            let mut popup = accesskit::Node::new(Role::ListBox);
            popup.set_bounds(Rect::new(0.0, 40.0, 200.0, 75.0));
            popup.set_clips_children();
            popup.add_action(Action::Collapse);
            popup.set_children((4..7).map(LocalNodeId).collect::<Vec<_>>());
            nodes.push((LocalNodeId(3), popup));
            for (index, label) in labels.into_iter().enumerate() {
                let mut option = accesskit::Node::new(Role::ListBoxOption);
                option.set_label(label);
                option.set_selected(selected == Some(index));
                option.add_action(Action::Click);
                let top = 40.0 + index as f64 * 30.0 - scroll;
                option.set_bounds(Rect::new(0.0, top, 200.0, top + 30.0));
                nodes.push((LocalNodeId(4 + index as u64), option));
            }
        }
        nodes.extend([(LocalNodeId(0), window), (LocalNodeId(1), field)]);
        accesskit::TreeUpdate {
            nodes,
            tree: Some(accesskit::Tree::new(LocalNodeId(0))),
            tree_id: TreeId::ROOT,
            focus: LocalNodeId(1),
        }
    }

    #[test]
    fn native_dropdown_selection_exposes_one_real_display_value_across_popup_scroll_and_changes() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let actions = Arc::new(Mutex::new(Vec::new()));
        let mut adapter = adapter(
            selection_update(false, 0.0, Some(0), 2),
            events.clone(),
            actions.clone(),
        );
        let window = adapter.platform_node(adapter.root_id());
        let field = adapter.platform_node(window.child_at_index(0).unwrap().unwrap());
        for (open, scroll) in [(false, 0.0), (true, 0.0), (true, 100.0)] {
            adapter.update(selection_update(open, scroll, Some(0), 2));
            assert_eq!(field.name().unwrap(), "Settings category");
            assert!(field.interfaces().unwrap().contains(Interface::Selection));
            assert!(
                !field.supports_value().unwrap(),
                "string choices are not numeric Value"
            );
            assert_eq!(field.n_selected_children().unwrap(), 1);
            let selected = field.selected_child(0).unwrap().unwrap();
            assert_eq!(
                adapter.platform_node(selected).name().unwrap(),
                "Appearance"
            );
            assert!(!adapter.platform_node(selected).do_action(0).unwrap());
            assert!(field.selected_child(1).unwrap().is_none());
            if open {
                let popup = window.child_at_index(1).unwrap().unwrap();
                let relations = field.relation_set(|id| id).unwrap();
                assert_eq!(
                    relations.get(&RelationType::ControllerFor),
                    Some(&vec![popup])
                );
            }
        }
        events.lock().unwrap().clear();
        adapter.update(selection_update(false, 0.0, Some(1), 7));
        assert_eq!(field.n_selected_children().unwrap(), 1);
        let selected = field.selected_child(0).unwrap().unwrap();
        assert_eq!(adapter.platform_node(selected).name().unwrap(), "Terminal");
        assert!(events.lock().unwrap().iter().any(|event| matches!(event,
            crate::Event::Object { target: NodeIdOrRoot::Node(target), event: ObjectEvent::SelectionChanged } if *target == field.id()
        )), "a changed display identity must notify the actual Combo Selection owner");
        adapter.update(selection_update(false, 0.0, None, 8));
        assert_eq!(field.n_selected_children().unwrap(), 0);
        assert!(field.selected_child(0).unwrap().is_none());
        assert!(actions.lock().unwrap().is_empty());
    }
    #[test]
    fn native_dropdown_active_descendant_notifies_current_controlled_option_and_never_fakes_none() {
        fn highlighted(target: Option<u64>) -> accesskit::TreeUpdate {
            let mut state = selection_update(true, 0.0, Some(0), 2);
            let field = &mut state
                .nodes
                .iter_mut()
                .find(|(id, _)| *id == LocalNodeId(1))
                .unwrap()
                .1;
            if let Some(target) = target {
                field.set_active_descendant(LocalNodeId(target));
            }
            let window = &mut state
                .nodes
                .iter_mut()
                .find(|(id, _)| *id == LocalNodeId(0))
                .unwrap()
                .1;
            window.set_children(vec![LocalNodeId(1), LocalNodeId(3), LocalNodeId(8)]);
            state
                .nodes
                .push((LocalNodeId(8), accesskit::Node::new(Role::Button)));
            state
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut adapter = adapter(highlighted(Some(4)), events.clone(), Default::default());
        let window = adapter.platform_node(adapter.root_id());
        let field = adapter.platform_node(window.child_at_index(0).unwrap().unwrap());
        let popup = adapter.platform_node(window.child_at_index(1).unwrap().unwrap());
        let current = popup.child_at_index(1).unwrap().unwrap();
        events.lock().unwrap().clear();
        adapter.update(highlighted(Some(5)));
        assert!(events.lock().unwrap().iter().any(|event| matches!(event,
            crate::Event::Object { target: NodeIdOrRoot::Node(owner), event: ObjectEvent::ActiveDescendantChanged(target) }
                if *owner == field.id() && *target == current
        )));
        for target in [None, Some(8)] {
            events.lock().unwrap().clear();
            adapter.update(highlighted(target));
            assert!(
                !events.lock().unwrap().iter().any(|event| matches!(
                    event,
                    crate::Event::Object {
                        event: ObjectEvent::ActiveDescendantChanged(_),
                        ..
                    }
                )),
                "no null or unrelated target may be fabricated"
            );
        }
    }
}
