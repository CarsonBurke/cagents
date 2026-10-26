//! Named child tabs with bounded transcript preparation and a searchable chooser.
use super::*;

mod reveal;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Filter {
    #[default]
    All,
    Working,
    Attention,
}

#[derive(Default)]
pub(super) struct Picker {
    pub query: String,
    pub filter: Filter,
    pub page: usize,
    order: Vec<String>,
}

const PAGE_SIZE: usize = 24;
const MAX_DIRECT_TABS: usize = 24;

// Presentation key only. Selection commands retain and validate the exact ID.
fn child_key(id: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(id.as_bytes()).into()
}

fn rank(child: &chat::Subagent) -> u8 {
    match child.status {
        chat::ToolStatus::Running if !child.waiting => 0,
        chat::ToolStatus::Running => 1,
        chat::ToolStatus::Failed | chat::ToolStatus::Denied => 2,
        _ => 3,
    }
}

fn needs_attention(child: &chat::Subagent) -> bool {
    (child.status == chat::ToolStatus::Running && child.waiting)
        || matches!(
            child.status,
            chat::ToolStatus::Failed | chat::ToolStatus::Denied
        )
}

fn child_icon(child: &chat::Subagent) -> Icon {
    match child.status {
        chat::ToolStatus::Running if child.waiting => Icon::AgentWaiting,
        chat::ToolStatus::Running => Icon::AgentWorking,
        chat::ToolStatus::Failed | chat::ToolStatus::Denied => Icon::Warning,
        chat::ToolStatus::Succeeded => Icon::Check,
        chat::ToolStatus::Stopped => Icon::Cancel,
    }
}

/// Keep metadata scanning cheap and mounted controls bounded. The chooser is
/// the complete catalog; its selected child always keeps a direct tab.
fn direct_tabs<'a>(
    children: &'a [chat::Subagent],
    selected: Option<&str>,
    focused: Option<&str>,
) -> Vec<&'a chat::Subagent> {
    // Each rank needs at most one strip's prefix. Never allocate or sort a
    // reference for every child just to mount at most 26 native controls.
    let mut groups: [Vec<_>; 4] = std::array::from_fn(|_| Vec::new());
    let mut pins = [None, None];
    for (index, child) in children.iter().enumerate() {
        let row = (index, child);
        let group = &mut groups[usize::from(rank(child))];
        if group.len() < MAX_DIRECT_TABS {
            group.push(row);
        }
        for (pin, identity) in pins.iter_mut().zip([selected, focused]) {
            if pin.is_none() && identity == Some(child.id.as_str()) {
                *pin = Some(row);
            }
        }
    }
    let mut rows = Vec::with_capacity(MAX_DIRECT_TABS + 2);
    let mut remaining = MAX_DIRECT_TABS;
    for group in groups {
        let count = remaining.min(group.len());
        rows.extend(group.into_iter().take(count));
        remaining -= count;
    }
    for pin in pins.into_iter().flatten() {
        if !rows.iter().any(|(index, _)| *index == pin.0) {
            rows.push(pin);
        }
    }
    // Only the bounded strip is sorted. Source indices retain the provider's
    // order within each status group, including selected/focused pins.
    rows.sort_unstable_by_key(|(index, child)| (rank(child), *index));
    rows.into_iter().map(|(_, child)| child).collect()
}

impl Picker {
    pub fn new(children: &[chat::Subagent]) -> Self {
        Self {
            order: children.iter().map(|child| child.id.clone()).collect(),
            ..Self::default()
        }
    }

    pub fn rows<'a>(&self, children: &'a [chat::Subagent]) -> Vec<&'a chat::Subagent> {
        let query = self.query.trim().to_lowercase();
        let mut rows: Vec<_> = children
            .iter()
            .filter(|child| {
                let admitted = match self.filter {
                    Filter::All => true,
                    Filter::Working => child.status == chat::ToolStatus::Running && !child.waiting,
                    Filter::Attention => needs_attention(child),
                };
                admitted
                    && (query.is_empty()
                        || [
                            Runtime::native_child_name(child),
                            &child.id,
                            child.model.as_deref().unwrap_or_default(),
                            Runtime::native_child_status(child),
                        ]
                        .iter()
                        .any(|part| part.to_lowercase().contains(&query)))
            })
            .collect();
        // Stable ordering preserves the provider's order among equal states.
        let order: HashMap<_, _> = self
            .order
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect();
        rows.sort_by_key(|child| {
            (
                rank(child),
                order
                    .get(child.id.as_str())
                    .copied()
                    .unwrap_or(self.order.len()),
            )
        });
        rows
    }
}

impl Runtime {
    pub(super) fn child_navigation(&self, _width: f32) -> Element<'_, Message> {
        let target = self.target;
        let selected = self.selected_subagent.as_ref();
        let focused = self.child_tab_focus.borrow().clone();
        let children = direct_tabs(
            &self.snapshot.subagents,
            selected.map(String::as_str),
            focused.as_deref(),
        );
        let identities: HashMap<_, _> = children
            .iter()
            .map(|child| (child_key(&child.id), child.id.clone()))
            .collect();
        let focus = self.child_tab_focus.clone();
        let tabs = children
            .into_iter()
            .map(|child| {
                let chosen = selected == Some(&child.id);
                let description = self.native_child_identity(child);
                let icon = child_icon(child);
                let marker = if icon == Icon::AgentWorking {
                    crate::ui_controls::activity_spinner(
                        f32::from(design::COMPACT_ICON_SIZE),
                        design::accent_color,
                    )
                } else {
                    crate::ui_controls::passive_icon(icon, design::COMPACT_ICON_SIZE)
                };
                let tab = widget::button::custom(
                    widget::row::with_children(vec![
                        marker,
                        design::caption(Self::short_description(
                            Self::native_child_name(child),
                            24,
                        ))
                        .wrapping(iced::widget::text::Wrapping::None)
                        .into(),
                    ])
                    .spacing(space::XS)
                    .align_y(Alignment::Center),
                )
                .name(description.clone())
                .id(card_control_id(
                    target.card,
                    &format!("chat-subagent-tab-{}", child.id),
                ))
                .padding([space::XS, space::S])
                .class(crate::ui_controls::control_style(chosen, None))
                .selected(chosen)
                .on_press(action(Action::SelectSubagent(
                    target,
                    Some(child.id.clone()),
                )));
                (
                    child_key(&child.id),
                    crate::interaction::tooltip(tab, description),
                )
            })
            .collect();
        let scroll_id = card_control_id(target.card, "chat-subagent-tabs");
        let tabs = widget::scrollable(crate::interaction::keyed_control_row(
            tabs,
            space::XS,
            move |key| {
                *focus.borrow_mut() = key.and_then(|key| identities.get(&key).cloned());
            },
        ))
        .id(scroll_id.clone())
        .direction(iced::widget::scrollable::Direction::Horizontal(
            iced::widget::scrollable::Scrollbar::hidden(),
        ))
        .width(Length::Fill);
        let tabs = reveal::view(
            tabs,
            scroll_id,
            selected.map(|id| card_control_id(target.card, &format!("chat-subagent-tab-{id}"))),
        );
        let trigger = widget::button::custom(crate::ui_controls::passive_icon(
            Icon::PanDown,
            design::COMPACT_ICON_SIZE,
        ))
        .name(format!(
            "Choose subagent: {} total",
            self.snapshot.subagents.len()
        ))
        .id(card_control_id(target.card, "chat-subagent-picker"))
        .padding([space::XS, space::S])
        .class(crate::ui_controls::control_style(
            self.child_picker.is_some(),
            None,
        ))
        .on_press(action(Action::SubagentPicker(
            target,
            self.child_picker.is_none(),
        )));
        widget::container(
            widget::row::with_children(vec![
                crate::ui_controls::view_tab(
                    crate::ui_controls::ViewTab {
                        label: "Conversation".into(),
                        description: "Main conversation".into(),
                        count: None,
                        selected: selected.is_none(),
                        id: None,
                        on_press: Some(action(Action::SelectSubagent(target, None))),
                    },
                    true,
                    None,
                ),
                crate::interaction::overflow_fade_x(crate::interaction::horizontal_wheel(tabs)),
                crate::interaction::tooltip(trigger, "Search and filter subagents").into(),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .clip(true)
        .into()
    }
    /// Mount the chooser in the pane's ordinary widget tree. COSMIC popovers
    /// expose only their trigger to accessibility, hiding their choices.
    pub(super) fn child_picker_view(&self) -> Option<Element<'_, Message>> {
        let target = self.target;
        let picker = self.child_picker.as_ref()?;

        let matches = picker.rows(&self.snapshot.subagents);
        let page = picker.page.min(matches.len().saturating_sub(1) / PAGE_SIZE);
        let count = matches.len();
        let first = matches.get(page * PAGE_SIZE).map(|child| child.id.clone());
        let mut rows = Vec::with_capacity(PAGE_SIZE);
        for child in matches.into_iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
            let chosen = self.selected_subagent.as_ref() == Some(&child.id);
            let status = Self::native_child_status(child);
            let marker = child_icon(child);
            let name = format!(
                "{} · {status} · {}",
                Self::native_child_name(child),
                child.id
            );
            let row = widget::button::custom(
                widget::row::with_children(vec![
                    crate::ui_controls::passive_icon(marker, design::COMPACT_ICON_SIZE),
                    widget::column::with_children(vec![
                        design::body(Self::short_description(Self::native_child_name(child), 80))
                            .width(Length::Fill)
                            .into(),
                        design::caption(status).into(),
                    ])
                    .width(Length::Fill)
                    .spacing(space::XS)
                    .into(),
                    crate::ui_controls::passive_icon(
                        if chosen { Icon::Check } else { Icon::PanEnd },
                        design::COMPACT_ICON_SIZE,
                    ),
                ])
                .spacing(space::S)
                .align_y(Alignment::Center),
            )
            .name(name)
            .id(card_control_id(
                target.card,
                &format!("chat-subagent-{}", child.id),
            ))
            .width(Length::Fill)
            .padding([space::S, space::S])
            .class(crate::ui_controls::control_style(chosen, None))
            .selected(chosen)
            .on_press(action(Action::SelectSubagent(
                target,
                Some(child.id.clone()),
            )));
            rows.push((
                child_key(&child.id),
                crate::interaction::keyboard_list_row(row),
            ));
        }
        if rows.is_empty() {
            rows.push(([0u8; 32], design::caption("No matching subagents").into()));
        }
        let mut filters = Vec::new();
        for (filter, label) in [
            (Filter::All, "All"),
            (Filter::Working, "Working"),
            (Filter::Attention, "Needs attention"),
        ] {
            filters.push(crate::ui_controls::view_tab(
                crate::ui_controls::ViewTab {
                    label: label.into(),
                    description: format!("Filter subagents: {label}").into(),
                    count: None,
                    selected: picker.filter == filter,
                    id: None,
                    on_press: Some(action(Action::SubagentFilter(target, filter))),
                },
                true,
                None,
            ));
        }
        let list = crate::interaction::keyboard_list(
            widget::column::with_children(vec![
                widget::row::with_children(vec![
                    design::heading("Subagents").width(Length::Fill).into(),
                    crate::ui_controls::button::quiet("Close")
                        .name("Close subagent chooser — Escape")
                        .on_press(action(Action::SubagentPicker(target, false)))
                        .into(),
                ])
                .align_y(Alignment::Center)
                .into(),
                crate::interaction::keyboard_list_entry(
                    crate::ui_controls::field::search("Search subagents", &picker.query)
                        .id(card_control_id(target.card, "chat-subagent-search"))
                        .width(Length::Fill)
                        .on_input(move |query| action(Action::SubagentQuery(target, query)))
                        .on_submit_maybe(first.map(|id| {
                            move |_: String| {
                                action(Action::SelectSubagent(target, Some(id.clone())))
                            }
                        })),
                ),
                widget::row::with_children(filters)
                    .spacing(space::XS)
                    .into(),
                widget::row::with_children(vec![
                    design::caption(format!(
                        "{}–{} of {count}",
                        if count == 0 { 0 } else { page * PAGE_SIZE + 1 },
                        ((page + 1) * PAGE_SIZE).min(count)
                    ))
                    .width(Length::Fill)
                    .into(),
                    crate::ui_controls::button::quiet("Previous")
                        .on_press_maybe(
                            (page > 0).then_some(action(Action::SubagentPage(target, false))),
                        )
                        .into(),
                    crate::ui_controls::button::quiet("Next")
                        .on_press_maybe(
                            ((page + 1) * PAGE_SIZE < count)
                                .then_some(action(Action::SubagentPage(target, true))),
                        )
                        .into(),
                ])
                .spacing(space::XS)
                .align_y(Alignment::Center)
                .into(),
                widget::container(crate::interaction::keyed_list_rows(rows))
                    .padding(design::with_vertical_rail(iced::Padding::ZERO))
                    .into(),
            ])
            .spacing(space::S)
            .width(Length::Fill),
            true,
        );
        let panel = widget::container(
            widget::scrollable(list)
                .id(card_control_id(target.card, "chat-subagent-list"))
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                ))
                .height(Length::Fill),
        )
        .padding(space::M)
        .width(Length::Fill)
        .height(Length::Fill)
        .class(design::surface(
            Some(design::sunken),
            [design::RADIUS_M; 4],
            false,
        ));
        Some(crate::interaction::focused_escape(
            crate::interaction::opaque(panel),
            move || action(Action::SubagentPicker(target, false)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(id: &str, status: chat::ToolStatus, waiting: bool) -> chat::Subagent {
        chat::Subagent {
            id: id.into(),
            kind: format!("Agent {id}"),
            description: "A long task description is not an agent name".into(),
            status,
            waiting,
            model: None,
            detail: String::new(),
            background: true,
            items: Vec::new(),
        }
    }

    #[test]
    fn unnamed_child_navigation_mounts_exact_id_labels_without_task_identity() {
        use cosmic::iced::advanced::{Layout, layout, widget::Tree};
        let (mut runtime, _handle) = super::super::view::prepared_runtime_for_test(&[]);
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents = ["role-only", "blank-nickname"]
            .into_iter()
            .map(|id| chat::Subagent {
                // Actual adapter/runtime regressions in the HOST lane
                // prove absent/blank nickname publications project this.
                kind: String::new(),
                ..child(id, chat::ToolStatus::Running, false)
            })
            .collect();
        let renderer = crate::app::tests::headless_renderer();
        let mut element = runtime.child_navigation(800.0);
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(800.0, 100.0)),
        );
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        for id in ["role-only", "blank-nickname"] {
            let native = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .find(|node| {
                    node.id()
                        == &iced_accessibility::A11yId::Widget(card_control_id(
                            runtime.target.card,
                            &format!("chat-subagent-tab-{id}"),
                        ))
                        && node.node().role() == iced_accessibility::accesskit::Role::Button
                })
                .expect("the exact unnamed child mounts a real native tab");
            let label = native.node().label().expect("native tab identity");
            assert!(label.contains(&format!("native subagent · {id} ·")));
            assert!(!label.contains("long task description"));
            let bounds = native.node().bounds().expect("mounted native bounds");
            assert!(bounds.width() > 0.0 && bounds.height() > 0.0);
        }
        assert!(runtime.child_cache.is_empty());
    }

    #[test]
    fn direct_tabs_bound_large_catalog_while_retaining_selected_identity_and_working_prefix() {
        let children: Vec<_> = (0..1000)
            .map(|index| {
                child(
                    &index.to_string(),
                    if index < 30 {
                        chat::ToolStatus::Running
                    } else {
                        chat::ToolStatus::Succeeded
                    },
                    false,
                )
            })
            .collect();
        let visible = direct_tabs(&children, Some("999"), None);
        assert_eq!(visible.len(), MAX_DIRECT_TABS + 1);
        assert_eq!(visible.first().unwrap().id, "0");
        assert_eq!(visible.last().unwrap().id, "999");
        assert_eq!(
            visible
                .iter()
                .filter(|child| child.status == chat::ToolStatus::Running)
                .count(),
            MAX_DIRECT_TABS
        );
        let picker = Picker::new(&children);
        assert_eq!(
            picker.rows(&children).len(),
            1000,
            "the chooser keeps every child reachable"
        );
    }

    #[test]
    fn direct_tabs_preserve_full_catalog_status_order_and_both_pins_without_mounting_it() {
        use chat::ToolStatus::*;
        let children: Vec<_> = (0..5000)
            .map(|index| {
                let (status, waiting) = match index % 6 {
                    0 => (Succeeded, false),
                    1 => (Running, true),
                    2 => (Failed, false),
                    3 => (Running, false),
                    4 => (Stopped, false),
                    _ => (Denied, false),
                };
                child(&index.to_string(), status, waiting)
            })
            .collect();
        for (selected, focused) in [
            (None, None),
            (Some("3"), Some("9")),
            (Some("4998"), Some("4999")),
            (Some("4997"), Some("4995")),
            (Some("4997"), Some("4997")),
        ] {
            // Compare against the complete catalog's presentation contract:
            // a stable working-first prefix plus selected/focused identities.
            let mut catalog: Vec<_> = children.iter().collect();
            catalog.sort_by_key(|child| rank(child));
            let expected: Vec<_> = catalog
                .iter()
                .enumerate()
                .filter(|(index, child)| {
                    *index < MAX_DIRECT_TABS
                        || selected == Some(child.id.as_str())
                        || focused == Some(child.id.as_str())
                })
                .map(|(_, child)| child.id.as_str())
                .collect();
            let visible = direct_tabs(&children, selected, focused);
            assert!(visible.len() <= MAX_DIRECT_TABS + 2);
            let actual: Vec<_> = visible.iter().map(|child| child.id.as_str()).collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn picker_resorts_live_states_without_replacing_names_or_changing_tie_order() {
        use chat::ToolStatus::*;
        let mut children = vec![
            child("done", Succeeded, false),
            child("first", Running, false),
            child("waiting", Running, true),
            child("second", Running, false),
            child("failed", Failed, false),
        ];
        let mut picker = Picker::new(&children);
        let ids =
            |rows: Vec<&chat::Subagent>| rows.iter().map(|c| c.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(picker.rows(&children)),
            ["first", "second", "waiting", "failed", "done"]
        );
        children[0].status = Running;
        children[1].status = Succeeded;
        assert_eq!(
            ids(picker.rows(&children)),
            ["done", "second", "waiting", "failed", "first"]
        );
        picker.filter = Filter::Attention;
        assert_eq!(ids(picker.rows(&children)), ["waiting", "failed"]);
        picker.filter = Filter::Working;
        assert_eq!(ids(picker.rows(&children)), ["done", "second"]);
        picker.query = "Agent second".into();
        assert_eq!(ids(picker.rows(&children)), ["second"]);
        assert_eq!(Runtime::native_child_name(&children[0]), "Agent done");
        children[0].kind.clear();
        assert_eq!(Runtime::native_child_name(&children[0]), "Agent");
        assert_ne!(Runtime::native_child_name(&children[0]), children[0].id);
        assert_ne!(
            Runtime::native_child_name(&children[0]),
            children[0].description
        );
    }
}
