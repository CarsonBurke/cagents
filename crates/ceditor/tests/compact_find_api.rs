use cagents::editor_search::{FindOptions, MatchIndex, Matches, find_in_lines};
use cagents::model::CardId;
use cagents::repository_widget::RepositoryAction;

#[test]
fn public_find_results_complete_a_standalone_editor_action() {
    let matches: MatchIndex = find_in_lines(
        ["λa λa"].into_iter(),
        "λa",
        FindOptions {
            case_sensitive: true,
            ..Default::default()
        },
        || false,
    )
    .unwrap()
    .unwrap();
    assert_eq!(matches.len(), 2);
    let (ordinal, visible): (_, Matches<'_>) = matches.visible(0, 4, 7);
    assert_eq!(ordinal, 1);
    assert_eq!(visible.len(), 1);
    assert_eq!(matches.get(1).unwrap().0.column, 4);
    assert_eq!(matches.iter().count(), 2);

    let card = CardId::new();
    let action = RepositoryAction::FindLoaded {
        card,
        ticket: card.as_uuid(),
        result: Ok(Some(matches)),
    };
    let RepositoryAction::FindLoaded { result, .. } = action else {
        panic!("expected the standalone editor's asynchronous Find completion");
    };
    assert_eq!(result.unwrap().unwrap().len(), 2);

    let empty = RepositoryAction::FindLoaded {
        card,
        ticket: card.as_uuid(),
        result: Ok(Some(MatchIndex::default())),
    };
    let RepositoryAction::FindLoaded { result, .. } = empty else {
        panic!("expected an empty Find completion");
    };
    assert!(result.unwrap().unwrap().is_empty());
}
