//! What the attention fold reads of the durable workspace.
//!
//! Copy-on-write gives the workspace a new address after any edit at all: a
//! focus change, a title, a saved draft, a channel reorder. Keying the fold on
//! that address refolded attention across every card for edits that cannot
//! change it. The fold reads only the open cards, in order, with their
//! conversations and recorded scopes, and channel membership; it keeps a copy
//! of those, and an edit that leaves them as they were keeps the fold.
//!
//! The fold reads the workspace only through [`Inputs`], so the comparison
//! cannot leave out anything it depends on.

use std::path::{Path, PathBuf};

use crate::model::{CardId, CardKind, ChannelId, ProjectId, WorkspaceState};

/// The logical resource behind a card. Shared native and console recipes can
/// open one session in more than one card, and those count once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Conversation {
    Session(uuid::Uuid),
    Card(uuid::Uuid),
}

/// Whether a card is a pane the list may give a row of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pane {
    Chat,
    Terminal,
    Other,
}

impl Pane {
    pub(super) fn is_pane(self) -> bool {
        self != Self::Other
    }

    pub(super) fn is_terminal(self) -> bool {
        self == Self::Terminal
    }
}

/// One open card as the fold reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Card {
    pub(super) id: CardId,
    pub(super) conversation: Conversation,
    pub(super) pane: Pane,
    /// The workspace's explicit filing for the card, if any.
    pub(super) scope: Option<PathBuf>,
}

/// [`Card`] borrowed from the workspace, so an unchanged workspace is
/// recognised without copying anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CardRef<'a> {
    id: CardId,
    conversation: Conversation,
    pane: Pane,
    scope: Option<&'a Path>,
}

impl Card {
    fn borrowed(&self) -> CardRef<'_> {
        CardRef {
            id: self.id,
            conversation: self.conversation,
            pane: self.pane,
            scope: self.scope.as_deref(),
        }
    }
}

impl CardRef<'_> {
    fn owned(self) -> Card {
        Card {
            id: self.id,
            conversation: self.conversation,
            pane: self.pane,
            scope: self.scope.map(Path::to_path_buf),
        }
    }
}

/// A card a channel holds: the channel's project and the channel.
pub(super) type Member = (CardId, ProjectId, ChannelId);

/// Everything the fold reads of the workspace.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Inputs {
    /// Open cards in workspace order.
    pub(super) cards: Vec<Card>,
    /// Every card each channel holds, in channel order.
    pub(super) members: Vec<Member>,
}

impl Inputs {
    pub(super) fn read(durable: &WorkspaceState) -> Self {
        Self {
            cards: open_cards(durable).map(CardRef::owned).collect(),
            members: members(durable).collect(),
        }
    }

    /// Whether `durable` holds what these were read from. Costs a pass over
    /// the open cards and channel members, with one scope lookup per card,
    /// and allocates nothing.
    pub(super) fn describes(&self, durable: &WorkspaceState) -> bool {
        self.members.iter().copied().eq(members(durable))
            && self
                .cards
                .iter()
                .map(Card::borrowed)
                .eq(open_cards(durable))
    }
}

fn open_cards(durable: &WorkspaceState) -> impl Iterator<Item = CardRef<'_>> {
    durable
        .cards
        .iter()
        .filter(|card| card.existence.is_open())
        .map(|card| {
            let (conversation, pane) = match &card.kind {
                CardKind::Chat(session) => {
                    (Conversation::Session(session.id.as_uuid()), Pane::Chat)
                }
                CardKind::Terminal(session) => {
                    (Conversation::Session(session.id.as_uuid()), Pane::Terminal)
                }
                _ => (Conversation::Card(card.id.as_uuid()), Pane::Other),
            };
            CardRef {
                id: card.id,
                conversation,
                pane,
                scope: durable.card_scopes.get(&card.id).map(PathBuf::as_path),
            }
        })
}

fn members(durable: &WorkspaceState) -> impl Iterator<Item = Member> + '_ {
    durable.channels.iter().flat_map(|channel| {
        channel
            .cards
            .iter()
            .map(|card| (*card, channel.project_id, channel.id))
    })
}
