//! The model and effort chosen for a draft's first message, carried in the
//! draft's own launch (`Launch::draft_model`) so that every window showing
//! the draft shows and starts it on the same choice.
//!
//! A draft's other launch options already travel this way: its account and
//! permission mode are in its launch recipe, and the shared view publishes the
//! launch to every window. Only an explicit choice is recorded; a draft
//! without one follows the window's remembered choice for its provider.

use crate::chat;
use crate::model::ChatDraftModel;

/// A model and effort chosen for a draft, `None` meaning the default.
pub(super) type Choice = (Option<String>, Option<String>);

/// The model and effort chosen for unstarted draft `session`, if one was. A
/// launch copied into another conversation, as a clone's is, keeps its
/// choice, which does not apply there.
pub(super) fn get(session: &chat::Session) -> Option<Choice> {
    session
        .launch
        .draft_model
        .as_ref()
        .filter(|chosen| chosen.applies_to(session))
        .map(|chosen| (chosen.model.clone(), chosen.reasoning.clone()))
}

/// Records `choice` as draft `session`'s.
pub(super) fn set(session: &mut chat::Session, (model, reasoning): &Choice) {
    session.launch.draft_model = Some(Box::new(ChatDraftModel {
        session: session.id,
        provider: session.launch.provider,
        model: model.clone(),
        reasoning: reasoning.clone(),
    }));
}

/// Gives `to`, the same draft launched another way, the choice `from` records,
/// or none if it records none.
pub(super) fn carry(from: &chat::Session, to: &mut chat::Session) {
    match get(from) {
        Some(choice) => set(to, &choice),
        None => to.launch.draft_model = None,
    }
}

/// Whether `a` and `b` launch the same conversation apart from the model and
/// effort chosen for it, which change neither its home, its directory nor
/// the skills it offers.
pub(super) fn same_launch(a: &chat::Launch, b: &chat::Launch) -> bool {
    a.provider == b.provider && a.session_id == b.session_id && a.recipe == b.recipe
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Account;
    use crate::agent::Provider;
    use crate::model::TerminalId;

    fn draft() -> chat::Session {
        chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            "/var/tmp".into(),
            &Account::system(Provider::Claude),
        )
        .unwrap()
    }

    #[test]
    fn a_choice_round_trips_including_the_explicit_default() {
        let mut session = draft();
        assert_eq!(get(&session), None);
        for choice in [
            (Some("haiku".to_owned()), Some("low".to_owned())),
            (Some("haiku".to_owned()), None),
            (None, None),
        ] {
            set(&mut session, &choice);
            assert_eq!(get(&session), Some(choice));
        }
    }

    #[test]
    fn a_choice_stays_with_the_unstarted_draft_it_was_made_in() {
        let mut chosen = draft();
        set(&mut chosen, &(Some("haiku".into()), None));
        let mut copy = chosen.clone();
        copy.id = TerminalId::new();
        assert_eq!(get(&copy), None);
        let mut started = chosen.clone();
        started.launch.session_id = Some("native".into());
        assert_eq!(get(&started), None);
        let mut moved = chosen.clone();
        moved.launch.provider = Provider::Codex;
        assert_eq!(get(&moved), None);
    }

    #[test]
    fn launches_differing_only_in_their_choice_are_the_same() {
        let plain = draft();
        let mut chosen = plain.clone();
        set(&mut chosen, &(Some("haiku".into()), Some("low".into())));
        assert!(same_launch(&plain.launch, &chosen.launch));
        let mut elsewhere = chosen.clone();
        elsewhere.launch.recipe.working_directory = "/tmp".into();
        assert!(!same_launch(&plain.launch, &elsewhere.launch));
        let mut carried = plain.clone();
        carry(&chosen, &mut carried);
        assert_eq!(carried, chosen);
        carry(&plain, &mut carried);
        assert_eq!(carried, plain);
    }
}
