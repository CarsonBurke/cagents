use crate::Control;
use crate::futures::futures::channel::mpsc;

use iced_accessibility::accesskit::{
    ActivationHandler, Node, NodeId, Role, Tree, TreeId, TreeUpdate,
};
use iced_runtime::core;

pub struct WinitActivationHandler {
    pub event_loop: winit::event_loop::EventLoopProxy,
    pub(crate) proxy: mpsc::UnboundedSender<Control>,
    pub title: String,
    pub id: core::window::Id,
    pub node_id: u64,
}

impl ActivationHandler for WinitActivationHandler {
    fn request_initial_tree(
        &mut self,
    ) -> Option<iced_accessibility::accesskit::TreeUpdate> {
        send_control(
            &self.proxy,
            Control::AccessibilityEnabled(self.id, true),
            || self.event_loop.wake_up(),
        );
        Some(activation_tree(self.node_id, self.title.clone()))
    }
}

pub struct WinitActionHandler {
    pub event_loop: winit::event_loop::EventLoopProxy,
    pub id: core::window::Id,
    pub(crate) proxy: mpsc::UnboundedSender<Control>,
}

impl iced_accessibility::accesskit::ActionHandler for WinitActionHandler {
    fn do_action(
        &mut self,
        request: iced_accessibility::accesskit::ActionRequest,
    ) {
        send_control(
            &self.proxy,
            Control::Accessibility(self.id, request),
            || self.event_loop.wake_up(),
        );
    }
}

pub struct WinitDeactivationHandler {
    pub event_loop: winit::event_loop::EventLoopProxy,
    pub id: core::window::Id,
    pub(crate) proxy: mpsc::UnboundedSender<Control>,
}

impl iced_accessibility::accesskit::DeactivationHandler
    for WinitDeactivationHandler
{
    fn deactivate_accessibility(&mut self) {
        send_control(
            &self.proxy,
            Control::AccessibilityEnabled(self.id, false),
            || self.event_loop.wake_up(),
        );
    }
}

// Native accessibility callbacks can run off the event-loop thread. The
// runner polls this control queue with a noop waker, so enqueueing alone cannot
// wake an idle window. Wake only after admission, keeping the message available
// when the event loop resumes; a retired runner needs no further wakeup.
fn send_control(
    proxy: &mpsc::UnboundedSender<Control>,
    control: Control,
    wake: impl FnOnce(),
) {
    if proxy.unbounded_send(control).is_ok() {
        wake();
    }
}

fn activation_tree(node_id: u64, title: String) -> TreeUpdate {
    let root = NodeId(node_id);
    let mut node = Node::new(Role::Window);
    node.set_label(title);
    TreeUpdate {
        tree_id: TreeId::ROOT,
        nodes: vec![(root, node)],
        tree: Some(Tree::new(root)),
        focus: root,
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
    fn activation_keeps_the_adapter_root_and_title() {
        let node_id = core::id::window_node_id();
        let tree = activation_tree(node_id, "Window".into());
        assert_eq!(tree.focus, NodeId(node_id));
        assert_eq!(tree.nodes[0].0, NodeId(node_id));
        assert_eq!(tree.nodes[0].1.label(), Some("Window"));
    }

    #[test]
    fn native_control_is_available_when_the_idle_loop_is_woken() {
        let (proxy, mut receiver) = mpsc::unbounded();
        let id = core::window::Id::unique();
        send_control(&proxy, Control::AccessibilityEnabled(id, true), || {
            assert!(matches!(
                receiver.try_next().unwrap(),
                Some(Control::AccessibilityEnabled(window, true)) if window == id
            ));
        });
        assert!(receiver.try_next().is_err());
    }

    #[test]
    fn retired_native_control_queue_does_not_wake_the_event_loop() {
        let (proxy, receiver) = mpsc::unbounded();
        drop(receiver);
        send_control(
            &proxy,
            Control::AccessibilityEnabled(core::window::Id::unique(), false),
            || panic!("closed runner must not be woken"),
        );
    }
}
