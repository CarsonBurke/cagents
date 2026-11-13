//! Named child tabs, working first, with a searchable chooser for the rest.
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
/// Tabs mounted beside the Conversation tab, apart from a selected or
/// focused child pinned to the strip. The chooser is the complete catalog.
const MAX_DIRECT_TABS: usize = 24;

// Presentation key only. Selection commands retain and validate the exact ID.
fn child_key(id: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(id.as_bytes()).into()
}

/// Working children lead, so what is still running is what the reader sees
/// first; a question or a failure follows them, and finished work goes last.
fn rank(child: &chat::Subagent) -> u8 {
    match child.status {
        // A child waiting on its own background work is alive and asks nothing
        // of anyone, so it stays with the working ones rather than in attention.
        chat::ToolStatus::Running if !child.waiting => 0,
        chat::ToolStatus::Waiting => 0,
        chat::ToolStatus::Running => 1,
        chat::ToolStatus::Failed | chat::ToolStatus::Denied => 2,
        _ => 3,
    }
}

fn needs_attention(child: &chat::Subagent) -> bool {
    matches!(rank(child), 1 | 2)
}

fn child_icon(child: &chat::Subagent) -> Icon {
    match child.status {
        chat::ToolStatus::Waiting => Icon::AgentBackground,
        chat::ToolStatus::Running if child.waiting => Icon::AgentWaiting,
        chat::ToolStatus::Running => Icon::AgentWorking,
        chat::ToolStatus::Failed | chat::ToolStatus::Denied => Icon::Warning,
        chat::ToolStatus::Succeeded => Icon::Check,
        chat::ToolStatus::Stopped => Icon::Cancel,
    }
}

/// The small mark beside a child's name. Work in progress moves, the way the
/// Conversation tab's own mark does; every other state is a still glyph.
fn status_mark<'a>(child: &chat::Subagent) -> Element<'a, Message> {
    match child_icon(child) {
        Icon::AgentWorking => crate::ui_controls::activity_spinner(
            f32::from(design::COMPACT_ICON_SIZE),
            design::accent_color,
        ),
        icon => crate::ui_controls::passive_icon(icon, design::COMPACT_ICON_SIZE),
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
    // reference for every child just to mount a bounded row of native tabs.
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

/// How many characters of its name a child's tab shows.
pub(super) const TAB_CHARS: usize = 24;

/// What a tab shows of `name` before any number: the name itself, or its
/// first [`TAB_CHARS`] characters when the tab cuts it, which is then marked.
/// Two names that read the same there are namesakes, so two tasks that start
/// alike are told apart by their numbers.
fn tab_namesake(name: &str) -> (&str, bool) {
    match name.char_indices().nth(TAB_CHARS) {
        Some((end, _)) => (name[..end].trim_end(), true),
        None => (name, false),
    }
}

/// The number each child's name carries, so two Explore agents read
/// "Explore" and "Explore 2", and so do two tasks a tab cuts to one text. A child keeps its number from the publication
/// it first appears in under that name: re-sorting the tabs, trimming older
/// children or renaming another child never renumbers it.
///
/// The host does not number children, and its trimmed snapshot cannot say
/// which names were taken before, so the pane remembers what it has seen.
#[derive(Default)]
pub(super) struct Ordinals(HashMap<String, (String, usize)>);

impl Ordinals {
    /// Record each installed publication. Unchanged names cost one lookup
    /// each; only a new or renamed child pays for assigning a number.
    pub fn observe(&mut self, children: &[chat::Subagent]) {
        if children.len() == self.0.len() && children.iter().all(|child| self.get(child).is_some())
        {
            return;
        }
        let names: HashMap<&str, &str> = children
            .iter()
            .map(|child| (child.id.as_str(), Runtime::native_child_name(child)))
            .collect();
        // A child that left, or now goes by another name, frees its number.
        self.0
            .retain(|id, (name, _)| names.get(id.as_str()) == Some(&name.as_str()));
        let mut taken: HashMap<(String, bool), HashSet<usize>> = HashMap::new();
        let namesake = |name: &str| {
            let (shown, cut) = tab_namesake(name);
            (shown.to_owned(), cut)
        };
        for (name, ordinal) in self.0.values() {
            taken.entry(namesake(name)).or_default().insert(*ordinal);
        }
        let literal: HashSet<&str> = names.values().copied().collect();
        for child in children {
            if self.0.contains_key(&child.id) {
                continue;
            }
            let name = Runtime::native_child_name(child);
            let taken = taken.entry(namesake(name)).or_default();
            // Skip a number another child already carries in its own name,
            // so a second Explore beside a real "Explore 2" reads "Explore 3".
            let ordinal = (1..)
                .find(|ordinal| {
                    !taken.contains(ordinal)
                        && (*ordinal == 1
                            || !literal.contains(format!("{name} {ordinal}").as_str()))
                })
                .unwrap_or(1);
            taken.insert(ordinal);
            self.0.insert(child.id.clone(), (name.to_owned(), ordinal));
        }
    }

    fn get(&self, child: &chat::Subagent) -> Option<usize> {
        self.0
            .get(&child.id)
            .filter(|(name, _)| name == Runtime::native_child_name(child))
            .map(|(_, ordinal)| *ordinal)
    }
}

/// `child`'s name as it is listed among `children`, cut to `limit`
/// characters before its number so the number always shows.
pub(super) fn child_label<'a>(
    children: &[chat::Subagent],
    ordinals: &Ordinals,
    child: &'a chat::Subagent,
    limit: usize,
) -> Cow<'a, str> {
    let name = Runtime::native_child_name(child);
    // A publication the pane has not installed yet, such as one a test built
    // in place, numbers by earlier namesakes: one pass with no allocation.
    let ordinal = ordinals.get(child).unwrap_or_else(|| {
        let namesake = tab_namesake(name);
        1 + children
            .iter()
            .take_while(|other| other.id != child.id)
            .filter(|other| tab_namesake(Runtime::native_child_name(other)) == namesake)
            .count()
    });
    let cut = name.chars().nth(limit).is_some();
    match (ordinal, cut) {
        (1, false) => Cow::Borrowed(name),
        (1, true) => Cow::Owned(Runtime::short_description(name, limit)),
        (ordinal, _) => Cow::Owned(format!(
            "{} {ordinal}",
            Runtime::short_description(name, limit)
        )),
    }
}

impl Picker {
    /// The chooser keeps the order it opened with, so a row does not move out
    /// from under the pointer when its child's state changes. It is the
    /// tabs' order at that moment: working first, then the provider's order.
    pub fn new(children: &[chat::Subagent]) -> Self {
        let mut order: Vec<_> = children.iter().collect();
        order.sort_by_key(|child| rank(child));
        Self {
            order: order.into_iter().map(|child| child.id.clone()).collect(),
            ..Self::default()
        }
    }

    pub fn rows<'a>(
        &self,
        children: &'a [chat::Subagent],
        ordinals: &Ordinals,
    ) -> Vec<&'a chat::Subagent> {
        let query = self.query.trim().to_lowercase();
        let mut rows: Vec<_> = children
            .iter()
            .filter(|child| {
                let admitted = match self.filter {
                    Filter::All => true,
                    Filter::Working => rank(child) == 0,
                    Filter::Attention => needs_attention(child),
                };
                // The reader searches what the chooser shows: the numbered
                // name, the task's first line, the model and the state.
                admitted
                    && (query.is_empty()
                        || [
                            &child_label(children, ordinals, child, usize::MAX),
                            Runtime::native_child_task(child).unwrap_or_default(),
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
            order
                .get(child.id.as_str())
                .copied()
                .unwrap_or(self.order.len())
        });
        rows
    }
}

impl Runtime {
    /// While a subagent's transcript is open, the main conversation's own
    /// work, a question for the reader or a failure is marked on its tab,
    /// since its transcript is not there to say so. What its subagents do is
    /// on their own tabs, so it is not repeated here.
    pub(super) fn parent_tab_state(&self) -> Option<(Icon, &str)> {
        let (glyph, label) = self.conversation_status_base(false);
        match glyph? {
            Icon::AgentWorking if self.snapshot.busy => Some((Icon::AgentWorking, label)),
            Icon::AgentWaiting if self.snapshot.waiting || !self.snapshot.requests.is_empty() => {
                Some((Icon::AgentWaiting, label))
            }
            Icon::Warning => Some((Icon::Warning, label)),
            _ => None,
        }
    }

    /// The name a child goes by wherever it is listed, numbered when it
    /// shares that name with another child. See [`Ordinals`].
    pub(super) fn child_label<'a>(&self, child: &'a chat::Subagent, limit: usize) -> Cow<'a, str> {
        child_label(&self.snapshot.subagents, &self.child_ordinals, child, limit)
    }

    /// A chooser row's accessible name. It ends with the exact child ID,
    /// which tells apart rows whose names and tasks read the same.
    pub(super) fn child_choice_name(child: &chat::Subagent, label: &str) -> String {
        let status = Self::native_child_status(child);
        match Self::native_child_task(child).filter(|_| !Self::named_by_task(child)) {
            Some(task) => format!(
                "{label} · {status} · Task: {} · {}",
                Self::short_description(task, 120),
                child.id
            ),
            None => format!("{label} · {status} · {}", child.id),
        }
    }

    pub(super) fn child_navigation(&self, _width: f32) -> Element<'_, Message> {
        let target = self.target;
        let children = &self.snapshot.subagents;
        let selected = self.selected_subagent.as_deref();
        let focused = self.child_tab_focus.borrow().clone();
        let shown = direct_tabs(children, selected, focused.as_deref());
        let identities: HashMap<_, _> = shown
            .iter()
            .map(|child| (child_key(&child.id), child.id.clone()))
            .collect();
        let tabs = shown
            .into_iter()
            .map(|child| {
                let description = self.child_description(child, &self.child_label(child, 80));
                let tab = crate::ui_controls::view_tab(
                    crate::ui_controls::ViewTab {
                        label: self.child_label(child, TAB_CHARS),
                        description: description.into(),
                        count: None,
                        selected: selected == Some(child.id.as_str()),
                        id: Some(card_control_id(
                            target.card,
                            &format!("chat-subagent-tab-{}", child.id),
                        )),
                        on_press: Some(action(Action::SelectSubagent(
                            target,
                            Some(child.id.clone()),
                        ))),
                    },
                    true,
                    Some(status_mark(child)),
                );
                (child_key(&child.id), tab)
            })
            .collect();
        // Focus follows the child, not its slot, when a state change re-sorts
        // the strip; the strip keeps that child mounted while it has focus.
        let focus = self.child_tab_focus.clone();
        let scroll_id = card_control_id(target.card, "chat-subagent-tabs");
        let tabs = widget::scrollable(crate::interaction::keyed_control_row(
            tabs,
            space::XS,
            move |key| {
                let next = key.and_then(|key| identities.get(&key));
                if focus.borrow().as_ref() != next {
                    *focus.borrow_mut() = next.cloned();
                }
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

        let working = children.iter().filter(|child| rank(child) == 0).count();
        let attention = children
            .iter()
            .filter(|child| needs_attention(child))
            .count();
        // The strip's marks say each child's state; the chooser's name keeps
        // the totals for children scrolled past the strip's edge.
        let trigger = widget::button::custom(crate::ui_controls::passive_icon(
            Icon::PanDown,
            design::COMPACT_ICON_SIZE,
        ))
        .name(format!(
            "Search and filter subagents: {} total, {working} working, {attention} need attention",
            children.len()
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

        let parent = selected
            .and_then(|_| self.parent_tab_state())
            .map(|(glyph, label)| {
                let mark = if glyph == Icon::AgentWorking {
                    crate::ui_controls::activity_spinner(
                        f32::from(design::COMPACT_ICON_SIZE),
                        design::accent_color,
                    )
                } else {
                    crate::ui_controls::passive_icon(glyph, design::COMPACT_ICON_SIZE)
                };
                (mark, label)
            });
        let (mark, description) = match parent {
            Some((mark, label)) => (Some(mark), format!("Main conversation: {label}").into()),
            None => (None, "Main conversation".into()),
        };
        widget::container(
            widget::row::with_children(vec![
                crate::ui_controls::view_tab(
                    crate::ui_controls::ViewTab {
                        label: "Conversation".into(),
                        description,
                        count: None,
                        selected: selected.is_none(),
                        id: None,
                        on_press: Some(action(Action::SelectSubagent(target, None))),
                    },
                    true,
                    mark,
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

        let matches = picker.rows(&self.snapshot.subagents, &self.child_ordinals);
        let page = picker.page.min(matches.len().saturating_sub(1) / PAGE_SIZE);
        let count = matches.len();
        let first = matches.get(page * PAGE_SIZE).map(|child| child.id.clone());
        let page_rows: Vec<_> = matches
            .into_iter()
            .skip(page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .collect();
        let mut rows = Vec::with_capacity(PAGE_SIZE);
        for child in page_rows {
            let chosen = self.selected_subagent.as_ref() == Some(&child.id);
            let status = Self::native_child_status(child);
            let label = self.child_label(child, 80);
            let name = Self::child_choice_name(child, &label);
            let row = widget::button::custom(
                widget::row::with_children(vec![
                    crate::ui_controls::passive_icon(child_icon(child), design::COMPACT_ICON_SIZE),
                    widget::column::with_children(vec![
                        design::body(label).width(Length::Fill).into(),
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
            description: "A long task description told apart from its name".into(),
            status,
            waiting,
            model: None,
            detail: String::new(),
            background: true,
            items: Vec::new(),
        }
    }

    fn named(id: &str, kind: &str, status: chat::ToolStatus, waiting: bool) -> chat::Subagent {
        chat::Subagent {
            kind: kind.into(),
            ..child(id, status, waiting)
        }
    }

    fn ids<'a>(rows: impl IntoIterator<Item = &'a chat::Subagent>) -> Vec<&'a str> {
        rows.into_iter().map(|child| child.id.as_str()).collect()
    }

    /// Mount the strip as the pane does and read each subagent tab's native
    /// accessible name, in the order the tabs are laid out.
    fn mounted_tabs(runtime: &Runtime) -> Vec<(String, String)> {
        use cosmic::iced::advanced::{Layout, layout, widget::Tree};
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
        let mut tabs: Vec<_> = runtime
            .snapshot
            .subagents
            .iter()
            .filter_map(|child| {
                let id = iced_accessibility::A11yId::Widget(card_control_id(
                    runtime.target.card,
                    &format!("chat-subagent-tab-{}", child.id),
                ));
                let native =
                    accessible
                        .root()
                        .iter()
                        .chain(accessible.children())
                        .find(|node| {
                            node.id() == &id
                                && node.node().role() == iced_accessibility::accesskit::Role::Button
                        })?;
                let bounds = native.node().bounds().expect("mounted native bounds");
                assert!(bounds.width() > 0.0 && bounds.height() > 0.0);
                Some((
                    bounds.x0,
                    child.id.clone(),
                    native.node().label().expect("native tab name").to_owned(),
                ))
            })
            .collect();
        tabs.sort_by(|a, b| a.0.total_cmp(&b.0));
        tabs.into_iter().map(|(_, id, label)| (id, label)).collect()
    }

    #[test]
    fn tabs_lead_with_working_agents_by_name_and_name_an_unnamed_one_by_its_task() {
        use chat::ToolStatus::*;
        let (mut runtime, _handle) = super::super::view::prepared_runtime_for_test(&[]);
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents = vec![
            named("done", "Reviewer", Succeeded, false),
            named("asking", "Planner", Running, true),
            named("first", "Explore", Running, false),
            named("broken", "Builder", Failed, false),
            named("second", "Explore", Running, false),
            named("unnamed", "", Stopped, false),
        ];
        let tabs = mounted_tabs(&runtime);
        assert_eq!(
            tabs.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["first", "second", "asking", "broken", "done", "unnamed"],
            "working first, then a question, a failure and finished work; provider order within each"
        );
        for (id, starts) in [
            ("first", "Explore · Working · "),
            ("second", "Explore 2 · Working · "),
            ("asking", "Planner · Waiting · "),
            ("broken", "Builder · Failed · "),
            ("done", "Reviewer · Finished · "),
        ] {
            let (_, label) = tabs.iter().find(|(tab, _)| tab == id).unwrap();
            assert!(label.starts_with(starts), "{id}: {label}");
            assert!(
                label.ends_with("Task: A long task description told apart from its name"),
                "{label}"
            );
        }
        // An agent with no name goes by its task, which its description then
        // does not repeat; its tab cuts it short.
        let (_, unnamed) = tabs.iter().find(|(tab, _)| tab == "unnamed").unwrap();
        assert!(
            unnamed.starts_with("A long task description told apart from its name · Stopped · "),
            "{unnamed}"
        );
        assert!(!unnamed.contains("Task:"), "{unnamed}");
        let children = &runtime.snapshot.subagents;
        assert_eq!(runtime.child_label(&children[4], 24), "Explore 2");
        assert_eq!(
            runtime.child_label(&children[5], 24),
            "A long task description…"
        );
        assert!(
            runtime.child_cache.is_empty(),
            "naming tabs must not prepare any hidden transcript"
        );
    }

    fn labels(children: &[chat::Subagent], ordinals: &Ordinals) -> Vec<String> {
        children
            .iter()
            .map(|child| child_label(children, ordinals, child, 24).into_owned())
            .collect()
    }

    #[test]
    fn duplicate_names_keep_their_number_when_states_reorder_the_tabs() {
        use chat::ToolStatus::*;
        let mut children = vec![
            named("a", "Explore", Running, false),
            named("b", "Explore", Running, false),
            named("c", "Explore", Succeeded, false),
        ];
        let mut ordinals = Ordinals::default();
        ordinals.observe(&children);
        assert_eq!(ids(direct_tabs(&children, None, None)), ["a", "b", "c"]);
        assert_eq!(
            labels(&children, &ordinals),
            ["Explore", "Explore 2", "Explore 3"]
        );
        children[0].status = Succeeded;
        children[2].status = Running;
        ordinals.observe(&children);
        assert_eq!(ids(direct_tabs(&children, None, None)), ["b", "c", "a"]);
        assert_eq!(
            labels(&children, &ordinals),
            ["Explore", "Explore 2", "Explore 3"]
        );
        // A publication the pane never installed numbers the same way.
        assert_eq!(
            labels(&children, &Ordinals::default()),
            ["Explore", "Explore 2", "Explore 3"]
        );
    }

    #[test]
    fn tasks_a_tab_cuts_to_one_text_are_numbered_apart() {
        use chat::ToolStatus::*;
        let children = vec![
            named(
                "a",
                "Review persistence stage 4 of the plan",
                Running,
                false,
            ),
            named(
                "b",
                "Review persistence stage 5 of the plan",
                Running,
                false,
            ),
            named("c", "Review persistence", Running, false),
        ];
        let mut ordinals = Ordinals::default();
        ordinals.observe(&children);
        for ordinals in [&ordinals, &Ordinals::default()] {
            assert_eq!(
                labels(&children, ordinals),
                [
                    "Review persistence stage…",
                    "Review persistence stage… 2",
                    "Review persistence",
                ]
            );
        }
    }

    #[test]
    fn trimming_or_renaming_children_never_renumbers_the_rest() {
        use chat::ToolStatus::*;
        // More children than the host keeps rich history for, the first of
        // them finished, as the runtime trims it.
        let mut children: Vec<_> = (0..40)
            .map(|index| {
                named(
                    &format!("child-{index}"),
                    if index % 2 == 0 { "Explore" } else { "Scout" },
                    if index == 0 { Succeeded } else { Running },
                    false,
                )
            })
            .collect();
        let (mut runtime, handle) = super::super::view::prepared_runtime_for_test(&[]);
        let publish = |runtime: &mut Runtime, children: &[chat::Subagent]| {
            let mut snapshot = runtime.snapshot.as_ref().clone();
            snapshot.generation += 1;
            snapshot.subagents = children.to_vec();
            handle.publish_snapshot_for_test(snapshot);
            runtime.refresh();
            runtime
                .snapshot
                .subagents
                .iter()
                .map(|child| {
                    (
                        child.id.clone(),
                        runtime.child_label(child, 24).into_owned(),
                    )
                })
                .collect::<HashMap<_, _>>()
        };
        let before = publish(&mut runtime, &children);
        assert_eq!(before["child-0"], "Explore");
        assert_eq!(before["child-2"], "Explore 2");
        assert_eq!(before["child-3"], "Scout 2");
        children.remove(0);
        let trimmed = publish(&mut runtime, &children);
        assert_eq!(trimmed.len(), 39);
        for (id, label) in &trimmed {
            assert_eq!(label, &before[id], "{id} kept its number after a trim");
        }
        // A rename numbers only the renamed child; a newcomer takes the
        // lowest number its name has free.
        children[1].kind = "Scout".into();
        children.push(named("late", "Explore", Running, false));
        let renamed = publish(&mut runtime, &children);
        assert_eq!(renamed["child-1"], "Scout");
        assert_eq!(renamed["child-2"], "Scout 21");
        assert_eq!(renamed["late"], "Explore");
        for (id, label) in &trimmed {
            if id != "child-2" {
                assert_eq!(&renamed[id], label, "{id} kept its number after a rename");
            }
        }
    }

    #[test]
    fn literal_numbered_names_and_long_names_stay_distinct() {
        use chat::ToolStatus::*;
        let children = vec![
            named("a", "Explore", Running, false),
            named("b", "Explore 2", Running, false),
            named("c", "Explore", Running, false),
            named(
                "d",
                "A very long agent name that cannot fit",
                Running,
                false,
            ),
            named(
                "e",
                "A very long agent name that cannot fit",
                Running,
                false,
            ),
        ];
        let mut ordinals = Ordinals::default();
        ordinals.observe(&children);
        assert_eq!(
            labels(&children, &ordinals),
            [
                "Explore",
                "Explore 2",
                "Explore 3",
                "A very long agent name t…",
                "A very long agent name t… 2",
            ]
        );
    }

    #[test]
    fn selected_child_keeps_its_tab_and_conversation_stays_first() {
        use chat::ToolStatus::*;
        let (mut runtime, _handle) = super::super::view::prepared_runtime_for_test(&[]);
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents = (0..40)
            .map(|index| child(&index.to_string(), Running, false))
            .chain([named("finished", "Scout", Succeeded, false)])
            .collect();
        runtime.selected_subagent = Some("finished".into());
        let tabs = mounted_tabs(&runtime);
        assert_eq!(tabs.len(), MAX_DIRECT_TABS + 1);
        assert_eq!(
            tabs.last().map(|(id, _)| id.as_str()),
            Some("finished"),
            "a selected child outside the working prefix keeps its own tab"
        );
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
            picker.rows(&children, &Ordinals::default()).len(),
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
            assert_eq!(ids(visible), expected);
        }
    }

    #[test]
    fn picker_keeps_its_opening_order_while_states_change_and_finds_by_task() {
        use chat::ToolStatus::*;
        let mut children = vec![
            child("done", Succeeded, false),
            child("first", Running, false),
            child("waiting", Running, true),
            child("second", Running, false),
            child("failed", Failed, false),
        ];
        let mut picker = Picker::new(&children);
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default())),
            ["first", "second", "waiting", "failed", "done"]
        );
        // Rows do not move under the pointer while the chooser is open.
        children[0].status = Running;
        children[1].status = Succeeded;
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default())),
            ["first", "second", "waiting", "failed", "done"]
        );
        picker.filter = Filter::Attention;
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default())),
            ["waiting", "failed"]
        );
        picker.filter = Filter::Working;
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default())),
            ["second", "done"]
        );
        picker.filter = Filter::All;
        picker.query = "Agent second".into();
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default())),
            ["second"]
        );
        children[3].description = "Audit the parser\nInternal prompt framing".into();
        picker.query = "audit".into();
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default())),
            ["second"],
            "the task stays searchable after leaving the label"
        );
        picker.query = "framing".into();
        assert!(
            picker.rows(&children, &Ordinals::default()).is_empty(),
            "only the task's first line is shown, so only it is searched"
        );
        children[4].kind = "Agent second".into();
        picker.query = "agent second 2".into();
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default())),
            ["failed"],
            "the visible number is searchable"
        );
        assert_eq!(Runtime::native_child_name(&children[0]), "Agent done");
        children[0].kind = " \t".into();
        assert_eq!(
            Runtime::native_child_name(&children[0]),
            "A long task description told apart from its name",
            "an unnamed agent goes by its task"
        );
        children[0].description = " \n".into();
        assert_eq!(Runtime::native_child_name(&children[0]), "Subagent");
    }

    #[test]
    fn a_child_waiting_on_background_work_stays_with_the_working_and_out_of_attention() {
        use chat::ToolStatus::*;
        let waiting = child("waiting", Waiting, false);
        assert_eq!(rank(&waiting), rank(&child("working", Running, false)));
        assert!(!needs_attention(&waiting));
        // Its own glyph: the one for waiting on the reader would ask for them.
        assert_eq!(child_icon(&waiting), Icon::AgentBackground);
        assert!(needs_attention(&child("asking", Running, true)));
        assert!(needs_attention(&child("failed", Failed, false)));
    }
}
