//! Named child tabs, working first, with a searchable chooser for the rest.
use super::*;

mod reveal;

/// Where the chooser lists a child: what asks something of the reader first,
/// then what is still going, then what has finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    Attention,
    Working,
    Finished,
}

impl Section {
    pub(super) fn of(child: &chat::Subagent) -> Self {
        match rank(child) {
            0 => Self::Working,
            FINISHED => Self::Finished,
            _ => Self::Attention,
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Attention => "Needs attention",
            Self::Working => "Working",
            Self::Finished => "Finished",
        }
    }
}

#[derive(Default)]
pub(super) struct Picker {
    pub query: String,
    /// The section a control opened the chooser on, so that pressing that
    /// control again closes it.
    pub opened_on: Option<Section>,
    /// The list's scroll offset and height, which choose the rows it builds.
    pub scroll: f32,
    pub viewport: Option<f32>,
    /// Each child's place, and the section it held, when the chooser opened.
    order: Vec<(String, Section)>,
}

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

/// The rank of a child that finished, stopped or ended. Finished children
/// fold into one control on the strip rather than each keeping a tab.
const FINISHED: u8 = 3;

fn needs_attention(child: &chat::Subagent) -> bool {
    matches!(rank(child), 1 | 2)
}

fn finished(child: &chat::Subagent) -> bool {
    rank(child) == FINISHED
}

fn child_icon(child: &chat::Subagent) -> Icon {
    match child.status {
        chat::ToolStatus::Waiting => Icon::AgentBackground,
        chat::ToolStatus::Running if child.waiting => Icon::AgentWaiting,
        chat::ToolStatus::Running => Icon::AgentWorking,
        // As its row and report mark it.
        chat::ToolStatus::Failed => Icon::Error,
        chat::ToolStatus::Denied => Icon::Unavailable,
        chat::ToolStatus::Succeeded => Icon::Check,
        // Stopped, or done without a word on how it went: neither a success
        // nor a failure, and not the Stop control.
        chat::ToolStatus::Stopped | chat::ToolStatus::Ended => Icon::Ended,
    }
}

/// The small mark beside a child's name. Work in progress moves, the way the
/// Conversation tab's own mark does; every other state is a still glyph, in
/// its attention tone when it is a failure or asks something of the reader.
pub(super) fn status_mark<'a>(child: &chat::Subagent) -> Element<'a, Message> {
    crate::sidebar::status_glyph(child_icon(child), design::COMPACT_ICON_SIZE)
}

/// Keep metadata scanning cheap and mounted controls bounded. The chooser is
/// the complete catalog; its selected child always keeps a direct tab.
/// Finished children get no tab of their own, so the strip holds what is
/// still live; a finished child keeps one only while it is selected or
/// focused.
fn direct_tabs<'a>(
    children: &'a [chat::Subagent],
    selected: Option<&str>,
    focused: Option<&str>,
) -> Vec<&'a chat::Subagent> {
    // Each rank needs at most one strip's prefix. Never allocate or sort a
    // reference for every child just to mount a bounded row of native tabs.
    let mut groups: [Vec<_>; FINISHED as usize] = std::array::from_fn(|_| Vec::new());
    let mut pins = [None, None];
    for (index, child) in children.iter().enumerate() {
        let row = (index, child);
        if let Some(group) = groups.get_mut(usize::from(rank(child)))
            && group.len() < MAX_DIRECT_TABS
        {
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

/// The most characters a chooser row's activity line shows of a report or
/// detail; past it the line fades out, as it does where the row runs out of
/// room.
const ACTIVITY_CHARS: usize = 160;

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

/// The number `child` goes by among children of its name, 1 for the first.
/// A publication the pane has not installed yet, such as one a test built
/// in place, numbers by earlier namesakes: one pass with no allocation.
fn child_ordinal(
    children: &[chat::Subagent],
    ordinals: &Ordinals,
    child: &chat::Subagent,
) -> usize {
    ordinals.get(child).unwrap_or_else(|| {
        let namesake = tab_namesake(Runtime::native_child_name(child));
        1 + children
            .iter()
            .take_while(|other| other.id != child.id)
            .filter(|other| tab_namesake(Runtime::native_child_name(other)) == namesake)
            .count()
    })
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
    let ordinal = child_ordinal(children, ordinals, child);
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

/// Each child's latest report to the conversation, by the ID the child is
/// listed under.
type Reports<'a> = HashMap<&'a str, &'a chat::Item>;

/// When a child last did anything its transcript or its report shows, in
/// Unix milliseconds. A child whose transcript was trimmed away and that
/// has not reported shows nothing.
fn last_activity(child: &chat::Subagent, report: Option<&chat::Item>) -> Option<u64> {
    let worked = child
        .items
        .iter()
        .rev()
        .find_map(|item| item.at.or(item.started_at));
    worked.max(report.and_then(|report| report.at.or(report.started_at)))
}

/// A report's body, or its summary when it has none.
fn report_text(report: &chat::Item) -> Option<&str> {
    match &report.content {
        ItemContent::Delivery { body, summary, .. } => Some(
            if body.trim().is_empty() {
                summary
            } else {
                body
            }
            .as_str(),
        ),
        _ => None,
    }
}

/// When a child's transcript shows it starting, in Unix milliseconds.
fn first_activity(child: &chat::Subagent) -> Option<u64> {
    child
        .items
        .iter()
        .find_map(|item| item.started_at.or(item.at))
}

/// How long something has gone on: `40s`, `12m`, `1h 05m`, `2d 3h`.
fn span(millis: u64) -> String {
    let seconds = millis / 1_000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h {:02}m", seconds / 3_600, seconds % 3_600 / 60),
        _ => format!("{}d {}h", seconds / 86_400, seconds % 86_400 / 3_600),
    }
}

/// A chooser row's time: how long a working child has been at it, and how
/// long ago any other child last did something.
fn row_time(child: &chat::Subagent, report: Option<&chat::Item>, now: u64) -> Option<String> {
    match Section::of(child) {
        Section::Working => first_activity(child).map(|start| span(now.saturating_sub(start))),
        _ => last_activity(child, report).map(|last| {
            cagents::account::format_ago(
                i64::try_from(now.saturating_sub(last) / 1_000).unwrap_or(i64::MAX),
            )
        }),
    }
}

/// The chooser's height for one section heading and for one child's row,
/// whose two lines are cut to one each, so the list can build only the
/// rows in and near view.
const HEADING_HEIGHT: f32 = 32.0;
const ROW_HEIGHT: f32 = 56.0;
/// How far beyond its viewport the list builds rows, so a key that moves
/// focus to the next row finds it built.
const OVERSCAN: f32 = 4.0 * ROW_HEIGHT;
/// The viewport assumed before the list has reported its own.
const FIRST_VIEWPORT: f32 = 900.0;
/// A list this tall or shorter is built whole: about fifty rows, cheap to
/// build, and taller than any pane the list could fit in.
const WHOLE_LIST: f32 = 3.0 * FIRST_VIEWPORT;

/// The span of the list, in its own offsets, whose rows are built: the
/// viewport and `OVERSCAN` either side.
///
/// The scrollable reports its offset and height only while its content
/// overflows, so once a list fits (the pane grew, or the chooser opened on
/// a section of a short list) both may be stale for good. A list short
/// enough to fit any pane is therefore built whole, and a longer one, which
/// overflows and so reports, has its offset held to the furthest it can
/// scroll.
fn built_span(total: f32, scroll: f32, viewport: Option<f32>) -> (f32, f32) {
    if total <= WHOLE_LIST {
        return (f32::NEG_INFINITY, f32::INFINITY);
    }
    let viewport = viewport.unwrap_or(FIRST_VIEWPORT);
    let scroll = scroll.clamp(0.0, (total - viewport).max(0.0));
    (scroll - OVERSCAN, scroll + viewport + OVERSCAN)
}

/// One line of the chooser's list.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Entry<'a> {
    Heading(Section, usize),
    Child(&'a chat::Subagent),
}

impl Entry<'_> {
    fn height(&self) -> f32 {
        match self {
            Self::Heading(..) => HEADING_HEIGHT,
            Self::Child(_) => ROW_HEIGHT,
        }
    }
}

/// `rows`, sorted by section, each section under a heading with its count.
fn entries<'a>(rows: &[&'a chat::Subagent]) -> Vec<Entry<'a>> {
    let mut entries = Vec::with_capacity(rows.len() + 3);
    for (index, child) in rows.iter().enumerate() {
        let section = Section::of(child);
        if index == 0 || Section::of(rows[index - 1]) != section {
            let count = rows[index..]
                .iter()
                .take_while(|other| Section::of(other) == section)
                .count();
            entries.push(Entry::Heading(section, count));
        }
        entries.push(Entry::Child(child));
    }
    entries
}

/// Where `section`'s heading starts in the list, when it lists any child.
fn section_offset(entries: &[Entry<'_>], section: Section) -> Option<f32> {
    let mut offset = 0.0;
    for entry in entries {
        if matches!(entry, Entry::Heading(shown, _) if *shown == section) {
            return Some(offset);
        }
        offset += entry.height();
    }
    None
}

impl Picker {
    /// The chooser keeps the order it opened with within each section, so a
    /// row does not move out from under the pointer while children report;
    /// only a change of state moves a child to another section. Within a
    /// section the child that did something last leads.
    #[cfg(test)]
    pub fn new(children: &[chat::Subagent]) -> Self {
        Self::ordered(children, &Reports::new())
    }

    fn ordered(children: &[chat::Subagent], reports: &Reports<'_>) -> Self {
        let mut order: Vec<_> = children.iter().collect();
        order.sort_by_key(|child| {
            (
                Section::of(child),
                std::cmp::Reverse(last_activity(
                    child,
                    reports.get(child.id.as_str()).copied(),
                )),
            )
        });
        Self {
            order: order
                .into_iter()
                .map(|child| (child.id.clone(), Section::of(child)))
                .collect(),
            ..Self::default()
        }
    }

    pub fn rows<'a>(
        &self,
        children: &'a [chat::Subagent],
        ordinals: &Ordinals,
        work: &super::view::AgentWork,
    ) -> Vec<&'a chat::Subagent> {
        let query = self.query.trim().to_lowercase();
        let mut rows: Vec<_> = children
            .iter()
            .filter(|child| {
                // The reader searches what the chooser shows: the numbered
                // name, the task's first line, the model and the state.
                query.is_empty()
                    || [
                        &child_label(children, ordinals, child, usize::MAX),
                        Runtime::native_child_task(child).unwrap_or_default(),
                        &child.id,
                        child.model.as_deref().unwrap_or_default(),
                        work.caption(child),
                    ]
                    .iter()
                    .any(|part| part.to_lowercase().contains(&query))
            })
            .collect();
        let order: HashMap<_, _> = self
            .order
            .iter()
            .enumerate()
            .map(|(index, (id, section))| (id.as_str(), (index, *section)))
            .collect();
        // A child that arrived, or moved to another section, after the
        // chooser opened leads its section, as the newest activity does.
        rows.sort_by_key(|child| {
            let section = Section::of(child);
            (
                section,
                order
                    .get(child.id.as_str())
                    .filter(|(_, opened)| *opened == section)
                    .map_or(0, |(index, _)| index + 1),
            )
        });
        rows
    }
}

impl Runtime {
    /// A chooser opening on this conversation's children as they stand.
    pub(super) fn child_picker_for_now(&self) -> Picker {
        Picker::ordered(&self.snapshot.subagents, &self.child_reports())
    }

    /// Each child's latest report in the conversation, under the ID the
    /// child is listed by: a report names its sender as the provider
    /// addresses it.
    fn child_reports(&self) -> Reports<'_> {
        let mut reports = Reports::new();
        for item in self.snapshot.items.iter().rev() {
            if let ItemContent::Delivery {
                kind: chat::DeliveryKind::Report,
                sender,
                ..
            } = &item.content
                && !sender.is_empty()
            {
                let listed = self
                    .agent_addresses
                    .get(sender)
                    .map_or(sender.as_str(), String::as_str);
                reports.entry(listed).or_insert(item.as_ref());
            }
        }
        reports
    }

    /// What the conversation itself is doing, in the glyph and words its
    /// status line uses, for the pane's inspector. Its subagents are left to
    /// the strip of agent tabs above the composer.
    pub(in crate::app) fn inspector_status(&self) -> (Option<Icon>, String) {
        self.own_conversation_status()
    }

    /// While a subagent's transcript is open, the main conversation's own
    /// work, a question for the reader or a failure is marked on its tab,
    /// since its transcript is not there to say so. What its subagents do is
    /// on their own tabs, so it is not repeated here.
    pub(super) fn parent_tab_state(&self) -> Option<(Icon, &str)> {
        let (glyph, label) = self.conversation_status_base(false);
        match glyph? {
            Icon::AgentWorking if self.snapshot.busy => Some((Icon::AgentWorking, label)),
            glyph
                if (self.snapshot.waiting || !self.snapshot.requests.is_empty())
                    && crate::sidebar::AttentionState::of_glyph(glyph)
                        .is_some_and(crate::sidebar::AttentionState::waits_on_user) =>
            {
                Some((glyph, label))
            }
            glyph @ (Icon::Warning | Icon::Error) => Some((glyph, label)),
            _ => None,
        }
    }

    /// The name a child goes by wherever it is listed, numbered when it
    /// shares that name with another child. See [`Ordinals`].
    pub(super) fn child_label<'a>(&self, child: &'a chat::Subagent, limit: usize) -> Cow<'a, str> {
        child_label(&self.snapshot.subagents, &self.child_ordinals, child, limit)
    }

    /// The number `child` goes by among children of its name; see
    /// [`Self::child_label`].
    pub(super) fn child_ordinal(&self, child: &chat::Subagent) -> usize {
        child_ordinal(&self.snapshot.subagents, &self.child_ordinals, child)
    }

    /// A chooser row's accessible name. It ends with the exact child ID,
    /// which tells apart rows whose names and tasks read the same.
    pub(super) fn child_choice_name(&self, child: &chat::Subagent, label: &str) -> String {
        let status = self.native_child_status(child);
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

        let working = self.snapshot.subagent_activity().working;
        let attention = children
            .iter()
            .filter(|child| needs_attention(child))
            .count();
        // The strip's marks say each child's state; the chooser's name keeps
        // the totals for children scrolled past the strip's edge.
        let trigger = widget::button::custom(crate::ui_controls::passive_icon(
            Icon::ChevronDown,
            design::COMPACT_ICON_SIZE,
        ))
        .name(format!(
            "All subagents: {} total, {working} working, {attention} need attention",
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

        // Finished children share one control that opens the chooser on
        // them, so finished work stays reachable without crowding the strip.
        let done = children.iter().filter(|child| finished(child)).count();
        let done = (done > 0).then(|| {
            crate::ui_controls::view_tab(
                crate::ui_controls::ViewTab {
                    label: format!("{done} finished").into(),
                    description: format!(
                        "Choose from {done} finished {}",
                        if done == 1 { "subagent" } else { "subagents" }
                    )
                    .into(),
                    count: None,
                    // A toggle like the chooser's own, so never the selected
                    // tab, which would take its press away; the chooser's
                    // filter row says what it shows.
                    selected: false,
                    id: Some(card_control_id(target.card, "chat-subagent-finished")),
                    on_press: Some(action(Action::SubagentChooser(target, Section::Finished))),
                },
                true,
                Some(crate::ui_controls::passive_icon(
                    Icon::Check,
                    design::COMPACT_ICON_SIZE,
                )),
            )
        });

        let parent = selected
            .and_then(|_| self.parent_tab_state())
            .map(|(glyph, label)| {
                (
                    crate::sidebar::status_glyph(glyph, design::COMPACT_ICON_SIZE),
                    label,
                )
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
            ])
            .push_maybe(done)
            .push(crate::interaction::tooltip(trigger, "All subagents"))
            .spacing(space::XS)
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .clip(true)
        .into()
    }
    /// Where the open chooser's list shows `section`, when it lists any
    /// child there.
    pub(super) fn child_section_offset(&self, section: Section) -> Option<f32> {
        let rows = self.child_picker.as_ref()?.rows(
            &self.snapshot.subagents,
            &self.child_ordinals,
            &self.agent_work,
        );
        section_offset(&entries(&rows), section)
    }

    /// What a chooser row says a child is doing, or how it ended: its
    /// current step while it works, what it waits on, its error, or the
    /// first line of its report.
    pub(super) fn child_activity(
        &self,
        child: &chat::Subagent,
        report: Option<&chat::Item>,
    ) -> String {
        let status = self.native_child_status(child);
        let detail = child.detail.trim();
        let reported = || {
            report
                .and_then(report_text)
                .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
                .map(|line| line.chars().take(ACTIVITY_CHARS + 1).collect())
        };
        let failure = || {
            chat::failure::Failure::parse(self.session.launch.provider, detail)
                .map(|failure| failure.summary)
        };
        // The section's heading and the row's mark already say a child
        // finished or failed; a stop or a denial is worth naming.
        let named = !matches!(
            child.status,
            chat::ToolStatus::Succeeded | chat::ToolStatus::Failed
        );
        let said = |words: Option<String>| match words.filter(|words| !words.is_empty()) {
            Some(words) if words == status || !named => words,
            Some(words) => format!("{status} · {words}"),
            None => status.to_owned(),
        };
        match child.status {
            chat::ToolStatus::Running if child.waiting => said(failure()),
            chat::ToolStatus::Running => {
                if let Some(error) = self.child_error(child) {
                    return chat::failure::summary(self.session.launch.provider, error)
                        .into_owned();
                }
                match child.items.last() {
                    Some(item)
                        if item.content.status().is_some_and(chat::ToolStatus::is_live)
                            && !matches!(item.content, ItemContent::Text { .. }) =>
                    {
                        super::turn::step_label(&item.content).into_owned()
                    }
                    _ => status.to_owned(),
                }
            }
            chat::ToolStatus::Waiting => status.to_owned(),
            chat::ToolStatus::Failed | chat::ToolStatus::Denied => said(
                failure()
                    .or_else(reported)
                    .or_else(|| detail.lines().next().map(str::to_owned)),
            ),
            _ => said(reported().or_else(|| {
                detail
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(|line| line.chars().take(ACTIVITY_CHARS + 1).collect())
            })),
        }
    }

    /// Mount the chooser in the pane's ordinary widget tree. COSMIC popovers
    /// expose only their trigger to accessibility, hiding their choices.
    ///
    /// One list, by section, builds only the rows in and near view: each
    /// row is one height, so where any row sits is known without building
    /// the rows before it.
    pub(super) fn child_picker_view(&self) -> Option<Element<'_, Message>> {
        let target = self.target;
        let picker = self.child_picker.as_ref()?;
        let rows = picker.rows(
            &self.snapshot.subagents,
            &self.child_ordinals,
            &self.agent_work,
        );
        let first = rows.first().map(|child| child.id.clone());
        let listed = entries(&rows);
        let reports = self.child_reports();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let (view_top, view_bottom) = built_span(
            listed.iter().map(Entry::height).sum(),
            picker.scroll,
            picker.viewport,
        );
        let mut list: Vec<([u8; 32], Element<'_, Message>)> = Vec::new();
        let mut skipped = 0.0;
        let mut top = 0.0;
        let gap = |height: f32, key: &str| -> ([u8; 32], Element<'_, Message>) {
            (
                child_key(key),
                widget::Space::new().height(Length::Fixed(height)).into(),
            )
        };
        for entry in &listed {
            let height = entry.height();
            let shown = top + height >= view_top && top <= view_bottom;
            top += height;
            if !shown {
                skipped += height;
                continue;
            }
            if skipped > 0.0 {
                list.push(gap(skipped, "\0above"));
                skipped = 0.0;
            }
            list.push(match *entry {
                Entry::Heading(section, count) => (
                    child_key(&format!("\0section-{section:?}")),
                    widget::container(
                        widget::row::with_children(vec![
                            design::caption(section.heading())
                                .class(cosmic::theme::Text::Custom(design::muted_text))
                                .into(),
                            design::caption(count.to_string())
                                .class(cosmic::theme::Text::Custom(design::muted_text))
                                .into(),
                        ])
                        .spacing(space::XS),
                    )
                    .padding([0.0, space::S])
                    .height(Length::Fixed(HEADING_HEIGHT))
                    .align_y(iced::alignment::Vertical::Bottom)
                    .into(),
                ),
                Entry::Child(child) => (
                    child_key(&child.id),
                    crate::interaction::keyboard_list_row(self.child_choice(
                        child,
                        reports.get(child.id.as_str()).copied(),
                        now,
                    )),
                ),
            });
        }
        if skipped > 0.0 {
            list.push(gap(skipped, "\0below"));
        }
        if list.is_empty() {
            list.push((
                [0u8; 32],
                widget::container(design::caption(if picker.query.trim().is_empty() {
                    "No subagents"
                } else {
                    "No matching subagents"
                }))
                .padding([space::S, space::S])
                .into(),
            ));
        }
        // A letter typed on a row goes on searching, as it would in the field.
        let list = crate::interaction::focused_key(
            crate::interaction::keyed_list_rows(list),
            move |event| match event {
                iced::keyboard::Event::KeyPressed {
                    text: Some(text),
                    modifiers,
                    ..
                } if !(modifiers.control() || modifiers.alt() || modifiers.logo())
                    && text.chars().all(|letter| !letter.is_control())
                    && !text.trim().is_empty() =>
                {
                    Some(action(Action::SubagentType(target, text.to_string())))
                }
                _ => None,
            },
        );
        let header = widget::column::with_children(vec![
            widget::row::with_children(vec![
                design::heading("Subagents").width(Length::Fill).into(),
                crate::ui_controls::icon_button(Icon::Close, "Close subagents (Esc)")
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
                        move |_: String| action(Action::SelectSubagent(target, Some(id.clone())))
                    })),
            ),
        ])
        .spacing(space::S);
        let scroller = crate::interaction::reveal_focus(crate::interaction::overflow_fade_y(
            widget::scrollable(
                // No inset above the rows: the list's offsets are the rows' own.
                widget::container(list).padding(design::with_vertical_rail(iced::Padding {
                    top: 0.0,
                    right: 0.0,
                    bottom: space::M,
                    left: 0.0,
                })),
            )
            .id(card_control_id(target.card, "chat-subagent-list"))
            .direction(iced::widget::scrollable::Direction::Vertical(
                design::scrollbar(),
            ))
            .on_scroll(move |viewport| {
                action(Action::SubagentScroll(
                    target,
                    viewport.absolute_offset().y,
                    viewport.bounds().height,
                ))
            })
            .height(Length::Fill),
            design::sunken,
        ));
        let panel = widget::container(crate::interaction::keyboard_list(
            widget::column::with_children(vec![header.into(), scroller])
                .spacing(space::S)
                .width(Length::Fill)
                .height(Length::Fill),
            true,
        ))
        .padding([space::M, space::M, 0.0, space::M])
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

    /// One child's row in the chooser: its state's mark, its name over what
    /// it is doing or how it ended, and its time.
    fn child_choice(
        &self,
        child: &chat::Subagent,
        report: Option<&chat::Item>,
        now: u64,
    ) -> Element<'_, Message> {
        let target = self.target;
        let chosen = self.selected_subagent.as_ref() == Some(&child.id);
        let label = self.child_label(child, 80);
        let name = self.child_choice_name(child, &label);
        // Shown in full, to fade where the row runs out of room.
        let label = self.child_label(child, usize::MAX);
        let line = |text: String, muted: bool| {
            let text = crate::ui_controls::FadeText::caption(text);
            if muted {
                text.class(cosmic::theme::Text::Custom(design::muted_text))
            } else {
                text
            }
        };
        let title = crate::ui_controls::fade_text(label.into_owned());
        let mut parts = vec![
            widget::container(status_mark(child))
                .width(Length::Fixed(f32::from(design::COMPACT_ICON_SIZE)))
                .into(),
            widget::column::with_children(vec![
                title.width(Length::Fill).into(),
                line(self.child_activity(child, report), true)
                    .limit(ACTIVITY_CHARS)
                    .width(Length::Fill)
                    .into(),
            ])
            .width(Length::Fill)
            .spacing(space::XXS)
            .into(),
        ];
        if let Some(time) = row_time(child, report, now) {
            parts.push(line(time, true).into());
        }
        widget::button::custom(
            widget::row::with_children(parts)
                .spacing(space::S)
                .align_y(Alignment::Center),
        )
        .name(name)
        .id(card_control_id(
            target.card,
            &format!("chat-subagent-{}", child.id),
        ))
        .width(Length::Fill)
        .height(Length::Fixed(ROW_HEIGHT))
        .padding([space::XS, space::S])
        .class(crate::sidebar::sidebar_button_style(chosen))
        .selected(chosen)
        .on_press(action(Action::SelectSubagent(
            target,
            Some(child.id.clone()),
        )))
        .into()
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

    /// Mount the strip as the pane does and read the native position and
    /// accessible name of each button whose control ID `controls` lists.
    fn mounted_buttons<'a>(
        runtime: &Runtime,
        controls: impl IntoIterator<Item = (&'a str, String)>,
    ) -> Vec<(f64, &'a str, String)> {
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
        let mut buttons: Vec<_> = controls
            .into_iter()
            .filter_map(|(key, control)| {
                let id = iced_accessibility::A11yId::Widget(card_control_id(
                    runtime.target.card,
                    &control,
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
                    key,
                    native
                        .node()
                        .label()
                        .expect("native button name")
                        .to_owned(),
                ))
            })
            .collect();
        buttons.sort_by(|a, b| a.0.total_cmp(&b.0));
        buttons
    }

    /// Each mounted subagent tab's ID and native accessible name, in the
    /// order the tabs are laid out.
    fn mounted_tabs(runtime: &Runtime) -> Vec<(String, String)> {
        mounted_buttons(
            runtime,
            runtime
                .snapshot
                .subagents
                .iter()
                .map(|child| (child.id.as_str(), format!("chat-subagent-tab-{}", child.id))),
        )
        .into_iter()
        .map(|(_, id, label)| (id.to_owned(), label))
        .collect()
    }

    /// The native name of the control finished subagents fold into.
    fn mounted_finished(runtime: &Runtime) -> Option<String> {
        mounted_buttons(runtime, [("finished", "chat-subagent-finished".to_owned())])
            .pop()
            .map(|(_, _, label)| label)
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
            ["first", "second", "asking", "broken"],
            "working first, then a question and a failure; provider order within each"
        );
        // Finished work folds into one control after the live tabs.
        let strip = mounted_buttons(
            &runtime,
            tabs.iter()
                .map(|(id, _)| (id.as_str(), format!("chat-subagent-tab-{id}")))
                .chain([("finished", "chat-subagent-finished".to_owned())]),
        );
        let (_, last, finished) = strip.last().expect("a mounted strip");
        assert_eq!(
            *last, "finished",
            "the finished control follows every live tab"
        );
        assert_eq!(finished, "Choose from 2 finished subagents");
        // A finished child the reader opened or moved to keeps its tab.
        runtime.selected_subagent = Some("unnamed".into());
        *runtime.child_tab_focus.borrow_mut() = Some("done".into());
        let tabs = mounted_tabs(&runtime);
        assert_eq!(
            tabs.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["first", "second", "asking", "broken", "done", "unnamed"],
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

    #[test]
    fn the_finished_control_opens_the_chooser_on_finished_subagents() {
        use chat::ToolStatus::*;
        use iced::advanced::clipboard;
        use iced::runtime::user_interface::{Cache, UserInterface};
        let (mut runtime, _handle) = super::super::view::prepared_runtime_for_test(&[]);
        let mut app = crate::app::tests::context_inspector_test_app(true);
        runtime.target.card = app.workspace.focused().unwrap();
        let target = runtime.target;
        std::sync::Arc::make_mut(&mut runtime.snapshot).subagents = vec![
            named("live", "Explore", Running, false),
            named("done", "Reviewer", Succeeded, false),
        ];
        assert_eq!(
            mounted_finished(&runtime).as_deref(),
            Some("Choose from 1 finished subagent")
        );
        let mut renderer = crate::app::tests::headless_renderer();
        let mut ui = UserInterface::build(
            runtime.child_navigation(800.0),
            iced::Size::new(800.0, 100.0),
            Cache::new(),
            &mut renderer,
        );
        let id = iced_accessibility::A11yId::Widget(card_control_id(
            target.card,
            "chat-subagent-finished",
        ));
        let accessible = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
        let bounds = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| node.id() == &id)
            .and_then(|node| node.node().bounds())
            .expect("a mounted finished control");
        let pointer = iced::Point::new(
            (bounds.x0 + bounds.x1) as f32 / 2.0,
            (bounds.y0 + bounds.y1) as f32 / 2.0,
        );
        let mut messages = Vec::new();
        ui.update(
            &[
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: pointer }),
                iced::Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left)),
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )),
            ],
            iced::mouse::Cursor::Available(pointer),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        drop(ui);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        let open = messages
            .into_iter()
            .find_map(|message| match message {
                Message::ChatUi(action @ Action::SubagentChooser(_, Section::Finished)) => {
                    Some(action)
                }
                _ => None,
            })
            .expect("pressing the finished control opens the chooser on finished subagents");
        drop(app.handle_chat_ui(open));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let picker = runtime.child_picker.as_ref().unwrap();
        assert_eq!(picker.opened_on, Some(Section::Finished));
        assert_eq!(
            ids(picker.rows(
                &runtime.snapshot.subagents,
                &runtime.child_ordinals,
                &runtime.agent_work,
            )),
            ["live", "done"]
        );
        // It opens at the finished section, below the working one.
        assert_eq!(picker.scroll, HEADING_HEIGHT + ROW_HEIGHT);
        // Pressed again, it closes the chooser, as the chooser's toggle does.
        drop(app.handle_chat_ui(Action::SubagentChooser(target, Section::Finished)));
        assert!(app.chat_runtime_mut(target).unwrap().child_picker.is_none());
        // Pressed while the chooser is open, it keeps
        // what the reader searched for.
        drop(app.handle_chat_ui(Action::SubagentPicker(target, true)));
        drop(app.handle_chat_ui(Action::SubagentQuery(target, "review".into())));
        drop(app.handle_chat_ui(Action::SubagentChooser(target, Section::Finished)));
        let picker = app
            .chat_runtime_mut(target)
            .unwrap()
            .child_picker
            .as_ref()
            .unwrap();
        assert_eq!(
            (picker.opened_on, picker.query.as_str()),
            (Some(Section::Finished), "review")
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
        assert_eq!(ids(direct_tabs(&children, None, None)), ["a", "b"]);
        assert_eq!(
            labels(&children, &ordinals),
            ["Explore", "Explore 2", "Explore 3"]
        );
        children[0].status = Succeeded;
        children[2].status = Running;
        ordinals.observe(&children);
        assert_eq!(ids(direct_tabs(&children, None, None)), ["b", "c"]);
        assert_eq!(
            ids(direct_tabs(&children, Some("a"), None)),
            ["b", "c", "a"]
        );
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
            picker
                .rows(&children, &Ordinals::default(), &Default::default())
                .len(),
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
            ids(picker.rows(&children, &Ordinals::default(), &Default::default())),
            ["waiting", "failed", "first", "second", "done"]
        );
        // Reporting does not move a row under the pointer while the chooser
        // is open; a change of state moves it to the top of its new section,
        // as its latest activity, and the rest keep their places.
        children[0].status = Running;
        children[1].status = Succeeded;
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default(), &Default::default())),
            ["waiting", "failed", "done", "second", "first"]
        );
        picker.query = "Agent second".into();
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default(), &Default::default())),
            ["second"]
        );
        children[3].description = "Audit the parser\nInternal prompt framing".into();
        picker.query = "audit".into();
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default(), &Default::default())),
            ["second"],
            "the task stays searchable after leaving the label"
        );
        picker.query = "framing".into();
        assert!(
            picker
                .rows(&children, &Ordinals::default(), &Default::default())
                .is_empty(),
            "only the task's first line is shown, so only it is searched"
        );
        children[4].kind = "Agent second".into();
        picker.query = "agent second 2".into();
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default(), &Default::default())),
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

    /// Only a failure is drawn as one, with the error mark its row and
    /// report use; a stopped agent, and one that ended without saying how it
    /// went, show the still "ended" mark rather than the Stop control's.
    #[test]
    fn only_a_failed_child_is_marked_as_failing() {
        use chat::ToolStatus::*;
        assert_eq!(child_icon(&child("failed", Failed, false)), Icon::Error);
        assert_eq!(child_icon(&child("stopped", Stopped, false)), Icon::Ended);
        assert_eq!(child_icon(&child("ended", Ended, false)), Icon::Ended);
        assert_eq!(
            child_icon(&child("finished", Succeeded, false)),
            Icon::Check
        );
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
        let children = vec![
            waiting,
            child("working", Running, false),
            child("asking", Running, true),
        ];
        let picker = Picker::new(&children);
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default(), &Default::default())),
            ["asking", "waiting", "working"]
        );
    }

    fn active(mut child: chat::Subagent, started: u64, last: u64) -> chat::Subagent {
        let item = |id: &str, at: u64| {
            std::sync::Arc::new(chat::Item {
                id: id.into(),
                role: chat::Role::Assistant,
                content: ItemContent::Text {
                    text: "Working".into(),
                    thinking: false,
                    phase: Default::default(),
                },
                started_at: Some(at),
                at: Some(at),
            })
        };
        child.items = vec![item("first", started), item("last", last)];
        child
    }

    /// Within each section the child that did something last leads, and a
    /// working child's row says how long it has worked while any other's
    /// says how long ago it last did something.
    #[test]
    fn the_chooser_leads_each_section_with_its_latest_activity() {
        use chat::ToolStatus::*;
        let minute = 60_000;
        let now = 1_000 * minute;
        let children = vec![
            active(child("old-done", Succeeded, false), 0, now - 180 * minute),
            active(child("new-done", Succeeded, false), 0, now - 5 * minute),
            active(
                child("long-run", Running, false),
                now - 90 * minute,
                now - minute,
            ),
            active(child("fresh-run", Running, false), now - 2 * minute, now),
            child("asking", Running, true),
        ];
        let picker = Picker::new(&children);
        let rows = picker.rows(&children, &Ordinals::default(), &Default::default());
        assert_eq!(
            ids(rows.iter().copied()),
            ["asking", "fresh-run", "long-run", "new-done", "old-done"]
        );
        assert_eq!(row_time(&children[2], None, now).as_deref(), Some("1h 30m"));
        assert_eq!(row_time(&children[1], None, now).as_deref(), Some("5m ago"));
        assert_eq!(
            row_time(&children[4], None, now),
            None,
            "nothing to time it by"
        );
        // Each section is headed by its count, and the list knows where each
        // starts without building a row.
        let listed = entries(&rows);
        assert_eq!(
            listed
                .iter()
                .filter_map(|entry| match entry {
                    Entry::Heading(section, count) => Some((*section, *count)),
                    Entry::Child(_) => None,
                })
                .collect::<Vec<_>>(),
            [
                (Section::Attention, 1),
                (Section::Working, 2),
                (Section::Finished, 2)
            ]
        );
        assert_eq!(section_offset(&listed, Section::Attention), Some(0.0));
        assert_eq!(
            section_offset(&listed, Section::Finished),
            Some(2.0 * HEADING_HEIGHT + 3.0 * ROW_HEIGHT)
        );
        let none = entries(&rows[..1]);
        assert_eq!(section_offset(&none, Section::Finished), None);
    }

    /// A list that fits its viewport reports no scrolling, so an offset left
    /// from opening on a section, or from a shorter window, must not leave
    /// the rows above it unbuilt.
    #[test]
    fn a_stale_scroll_still_builds_every_row_in_view() {
        // The pane grew to hold a list it had scrolled to the end of: the
        // list last reported a short viewport and the offset at its end.
        let fits = 2.0 * HEADING_HEIGHT + 30.0 * ROW_HEIGHT;
        for (scroll, viewport) in [
            (fits - 500.0, Some(500.0)),
            (400.0, Some(1_000.0)),
            (fits / 2.0, None),
        ] {
            let (top, bottom) = built_span(fits, scroll, viewport);
            assert!(top <= 0.0 && bottom >= fits, "{top}..{bottom}");
        }
        // A long list scrolled past its end shows its last rows.
        let long = 1_000.0 * ROW_HEIGHT;
        let (top, bottom) = built_span(long, 2.0 * long, Some(500.0));
        assert_eq!((top, bottom), (long - 500.0 - OVERSCAN, long + OVERSCAN));
        let (top, _) = built_span(long, 300.0, Some(500.0));
        assert_eq!(top, 300.0 - OVERSCAN);
    }

    /// A finished child whose transcript was trimmed away is timed, ordered
    /// and summarized by the report it sent the conversation.
    #[test]
    fn a_finished_child_is_known_by_its_report() {
        use chat::ToolStatus::*;
        let minute = 60_000;
        let now = 1_000 * minute;
        let report = |sender: &str, body: &str, at: u64| chat::Item {
            id: format!("report-{sender}"),
            role: chat::Role::User,
            content: ItemContent::Delivery {
                kind: chat::DeliveryKind::Report,
                from: "general-purpose".into(),
                sender: sender.into(),
                summary: format!("Agent \"{sender}\" finished"),
                body: body.into(),
            },
            started_at: None,
            at: Some(at),
        };
        let children = vec![
            child("early", Succeeded, false),
            active(child("late", Succeeded, false), 0, now - 30 * minute),
            child("quiet", Succeeded, false),
        ];
        let early = report(
            "early",
            "\nNo problem found.\nDetails follow.",
            now - 2 * minute,
        );
        let late = report("late", "", now - 10 * minute);
        let reports: Reports<'_> = [("early", &early), ("late", &late)].into_iter().collect();
        let picker = Picker::ordered(&children, &reports);
        assert_eq!(
            ids(picker.rows(&children, &Ordinals::default(), &Default::default())),
            ["early", "late", "quiet"]
        );
        assert_eq!(
            row_time(&children[0], Some(&early), now).as_deref(),
            Some("2m ago")
        );
        assert_eq!(
            report_text(&early).map(str::trim_start),
            Some("No problem found.\nDetails follow.")
        );
        assert_eq!(report_text(&late), Some("Agent \"late\" finished"));
    }
}
