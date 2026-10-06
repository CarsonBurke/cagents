use crate::Control;
use crate::futures::futures::channel::mpsc;

use iced_accessibility::accesskit::{
    ActivationHandler, Node, NodeId, Role, Tree, TreeId, TreeUpdate,
};
use iced_runtime::core;

pub struct WinitActivationHandler {
    pub proxy: mpsc::UnboundedSender<Control>,
    pub title: String,
    pub id: core::window::Id,
    pub node_id: u64,
}

impl ActivationHandler for WinitActivationHandler {
    fn request_initial_tree(
        &mut self,
    ) -> Option<iced_accessibility::accesskit::TreeUpdate> {
        let node_id = self.node_id;
        let _ = self
            .proxy
            .unbounded_send(Control::AccessibilityEnabled(self.id, true));
        let mut node = Node::new(Role::Window);
        node.set_label(self.title.clone());
        let root = NodeId(node_id);
        Some(TreeUpdate {
            tree_id: TreeId::ROOT,
            nodes: vec![(root, node)],
            tree: Some(Tree::new(root)),
            focus: root,
        })
    }
}

pub struct WinitActionHandler {
    pub id: core::window::Id,
    pub proxy: mpsc::UnboundedSender<Control>,
}

impl iced_accessibility::accesskit::ActionHandler for WinitActionHandler {
    fn do_action(
        &mut self,
        request: iced_accessibility::accesskit::ActionRequest,
    ) {
        let _ = self
            .proxy
            .unbounded_send(Control::Accessibility(self.id, request));
    }
}

pub struct WinitDeactivationHandler {
    pub id: core::window::Id,
    pub proxy: mpsc::UnboundedSender<Control>,
}

impl iced_accessibility::accesskit::DeactivationHandler
    for WinitDeactivationHandler
{
    fn deactivate_accessibility(&mut self) {
        let _ = self
            .proxy
            .unbounded_send(Control::AccessibilityEnabled(self.id, false));
    }
}

/// Read the current widget focus without changing it.
#[derive(Default)]
pub(crate) struct Focused {
    pub id: Option<core::widget::Id>,
}

impl core::widget::Operation for Focused {
    fn traverse(
        &mut self,
        operate: &mut dyn FnMut(&mut dyn core::widget::Operation),
    ) {
        operate(self);
    }

    fn focusable(
        &mut self,
        id: Option<&core::widget::Id>,
        _bounds: core::Rectangle,
        state: &mut dyn core::widget::operation::Focusable,
    ) {
        if state.is_focused() {
            self.id = id.cloned();
        }
    }
}

/// Publish a complete snapshot. Match semantic IDs against the emitted nodes:
/// a rebuilt custom ID can carry a new numeric value while naming the same widget.
pub(crate) fn tree_update(
    root: u64,
    title: String,
    widgets: iced_accessibility::A11yTree,
    focused: Option<core::widget::Id>,
    scale_factor: f64,
) -> TreeUpdate {
    use iced_accessibility::{A11yId, IdEq};
    let root = NodeId(root);
    let focus = focused
        .map(A11yId::from)
        .and_then(|focused| {
            widgets
                .root()
                .iter()
                .chain(widgets.children())
                .find(|node| IdEq::eq(node.id(), &focused))
                .map(|node| NodeId::from(node.id().clone()))
        })
        .unwrap_or(root);
    let mut window = Node::new(Role::Window);
    window.set_label(title);
    window.set_transform(iced_accessibility::accesskit::Affine::scale(
        scale_factor,
    ));
    window.set_children(
        widgets
            .root()
            .iter()
            .map(|node| NodeId::from(node.id().clone()))
            .collect::<Vec<_>>(),
    );
    let mut nodes: Vec<_> = widgets.into();
    nodes.push((root, window));
    TreeUpdate {
        tree_id: TreeId::ROOT,
        nodes,
        tree: Some(Tree::new(root)),
        focus,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_accessibility::{A11yTree, Id};

    #[test]
    fn publishes_children_and_resolves_rebuilt_focus_id() {
        let root = core::id::window_node_id();
        let emitted = Id::new("composer");
        let focused = Id::new("composer");
        let widgets =
            A11yTree::leaf(Node::new(Role::TextInput), emitted.clone());
        let update =
            tree_update(root, "Workspace".into(), widgets, Some(focused), 1.0);
        assert_eq!(
            update.focus,
            NodeId::from(iced_accessibility::A11yId::from(emitted))
        );
        assert_eq!(update.nodes.len(), 2);
        assert_eq!(update.nodes[1].1.children(), &[update.focus]);
        assert_eq!(update.nodes[1].1.label(), Some("Workspace"));
    }

    #[test]
    fn missing_focus_falls_back_to_the_stable_window_root() {
        let root = core::id::window_node_id();
        let update = tree_update(
            root,
            String::new(),
            Default::default(),
            Some(Id::unique()),
            1.0,
        );
        assert_eq!(update.focus, NodeId(root));
        assert!(update.nodes[0].1.children().is_empty());
    }

    #[test]
    fn activation_keeps_the_adapter_root_and_identifies_its_window() {
        let (proxy, mut receiver) = mpsc::unbounded();
        let id = core::window::Id::unique();
        let node_id = core::id::window_node_id();
        let mut handler = WinitActivationHandler {
            proxy,
            title: "Window".into(),
            id,
            node_id,
        };
        assert_eq!(
            handler.request_initial_tree().unwrap().focus,
            NodeId(node_id)
        );
        assert!(
            matches!(receiver.try_next().unwrap(), Some(Control::AccessibilityEnabled(window, true)) if window == id)
        );
    }
}
