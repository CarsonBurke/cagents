//! A bounded entrance to child transcripts, independent of their name lengths.
use super::*;

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

// Presentation key only. Selection commands retain and validate the exact ID.
fn child_key(id: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(id.as_bytes()).into()
}

// Keep a reported name primary. Only generic or visually repeated names need a
// secondary identity, computed from the full snapshot so filtering/paging
// cannot change it. Extend past eight characters on collisions; an ID that is
// a prefix of another is retained whole, never replaced with the other's ID.
fn identity_suffix<'a>(child: &'a chat::Subagent, peers: &[&chat::Subagent]) -> Option<&'a str> {
    if !child.kind.trim().is_empty() && peers.len() == 1 {
        return None;
    }
    let length = peers
        .iter()
        .filter(|peer| peer.id != child.id)
        .map(|peer| {
            child
                .id
                .chars()
                .zip(peer.id.chars())
                .take_while(|(left, right)| left == right)
                .count()
                + 1
        })
        .max()
        .unwrap_or(0)
        .max(8);
    let end = child
        .id
        .char_indices()
        .nth(length)
        .map_or(child.id.len(), |(end, _)| end);
    Some(&child.id[..end])
}

fn rank(child: &chat::Subagent) -> u8 {
    match child.status {
        chat::ToolStatus::Failed | chat::ToolStatus::Denied => 0,
        chat::ToolStatus::Running if child.waiting => 1,
        chat::ToolStatus::Running => 2,
        _ => 3,
    }
}

impl Picker {
    pub fn new(children: &[chat::Subagent]) -> Self {
        let mut order: Vec<_> = children.iter().collect();
        order.sort_by_key(|child| rank(child));
        Self {
            order: order.into_iter().map(|child| child.id.clone()).collect(),
            ..Self::default()
        }
    }

    pub fn rows<'a>(&self, children: &'a [chat::Subagent]) -> Vec<&'a chat::Subagent> {
        let query = self.query.trim();
        // Share transcript Find's streaming, full Unicode case folding. Keep
        // only query-sized matching state, never a folded copy of a task that
        // may contain a multi-megabyte first line. Whole source characters
        // still match across case-fold expansions (for example Straße/STRASSE).
        let pattern = find::Pattern::prepare(query, &|| true).expect("uncancelled picker search");
        let mut rows: Vec<_> = children
            .iter()
            .filter(|child| {
                let admitted = match self.filter {
                    Filter::All => true,
                    Filter::Working => child.status == chat::ToolStatus::Running && !child.waiting,
                    Filter::Attention => rank(child) < 2,
                };
                admitted
                    && (query.is_empty()
                        || [
                            Runtime::native_child_name(child),
                            Runtime::native_child_task(child).unwrap_or_default(),
                            &child.id,
                            child.model.as_deref().unwrap_or_default(),
                            Runtime::native_child_status(child),
                        ]
                        .iter()
                        .any(|part| {
                            find::matched_pattern_checked(part, &pattern, 0, &|| true)
                                .is_some_and(|matches| matches.truncated)
                        }))
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
    pub(super) fn child_navigation(&self, _width: f32) -> Element<'_, Message> {
        let target = self.target;
        let selected = self
            .selected_subagent
            .as_ref()
            .and_then(|id| self.snapshot.subagents.iter().find(|child| &child.id == id));
        let working = self
            .snapshot
            .subagents
            .iter()
            .filter(|child| child.status == chat::ToolStatus::Running && !child.waiting)
            .count();
        let attention = self
            .snapshot
            .subagents
            .iter()
            .filter(|child| rank(child) < 2)
            .count();
        let label = selected.map_or_else(
            || format!("Subagents ({})", self.snapshot.subagents.len()),
            |child| Self::short_description(Self::native_child_name(child), 24),
        );
        let description = format!(
            "Choose subagent: {} total, {working} working, {attention} need attention{}",
            self.snapshot.subagents.len(),
            selected.map_or(String::new(), |child| format!(
                ". Selected: {}",
                self.native_child_identity(child)
            ))
        );
        let trigger = widget::button::custom(
            widget::row::with_children(vec![
                design::caption(label)
                    .width(Length::Fill)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
                crate::ui_controls::passive_icon(Icon::PanDown, design::COMPACT_ICON_SIZE),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center),
        )
        .name(description)
        .id(card_control_id(target.card, "chat-subagent-picker"))
        .width(Length::Fill)
        .padding([space::XS, space::S])
        .class(crate::ui_controls::control_style(selected.is_some(), None))
        .on_press(action(Action::SubagentPicker(
            target,
            self.child_picker.is_none(),
        )));
        let mut controls = vec![
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
            trigger.into(),
        ];
        if attention > 0 {
            controls.push(
                crate::interaction::tooltip(
                    widget::row::with_children(vec![
                        crate::ui_controls::passive_icon(Icon::Warning, design::COMPACT_ICON_SIZE),
                        design::caption(attention.to_string()).into(),
                    ])
                    .spacing(space::XS),
                    format!("{attention} subagents need attention"),
                )
                .into(),
            );
        }
        if working > 0 {
            controls.push(
                crate::interaction::tooltip(
                    widget::row::with_children(vec![
                        crate::ui_controls::activity_spinner(
                            f32::from(design::COMPACT_ICON_SIZE),
                            design::accent_color,
                        ),
                        design::caption(working.to_string()).into(),
                    ])
                    .spacing(space::XS),
                    format!("{working} subagents working"),
                )
                .into(),
            );
        }
        widget::container(
            widget::row::with_children(controls)
                .spacing(space::S)
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
        let mut names: HashMap<String, Vec<&chat::Subagent>> = HashMap::new();
        for child in &self.snapshot.subagents {
            names
                .entry(Self::short_description(Self::native_child_name(child), 80))
                .or_default()
                .push(child);
        }
        let mut rows = Vec::with_capacity(PAGE_SIZE);
        for child in matches.into_iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
            let chosen = self.selected_subagent.as_ref() == Some(&child.id);
            let status = Self::native_child_status(child);
            let marker = match child.status {
                chat::ToolStatus::Running if child.waiting => Icon::AgentWaiting,
                chat::ToolStatus::Running => Icon::AgentWorking,
                chat::ToolStatus::Failed | chat::ToolStatus::Denied => Icon::Warning,
                chat::ToolStatus::Succeeded => Icon::Check,
                chat::ToolStatus::Stopped => Icon::Cancel,
            };
            let name = format!(
                "{} · {status} · {}",
                Self::native_child_name(child),
                child.id
            );
            let display_name = Self::short_description(Self::native_child_name(child), 80);
            let identity = identity_suffix(child, &names[display_name.as_str()]).map_or_else(
                || status.to_owned(),
                |suffix| format!("{status} · ID: {suffix}"),
            );
            let task = Self::native_child_task(child)
                .map(|task| format!("Task: {}", Self::short_description(task, 120)));
            let mut details = vec![
                design::body(display_name).width(Length::Fill).into(),
                design::caption(identity).into(),
            ];
            if let Some(task) = &task {
                details.push(design::caption(task.clone()).into());
            }
            let row = widget::button::custom(
                widget::row::with_children(vec![
                    crate::ui_controls::passive_icon(marker, design::COMPACT_ICON_SIZE),
                    widget::column::with_children(details)
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
            .description(task.unwrap_or_default())
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
