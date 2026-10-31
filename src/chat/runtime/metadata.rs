//! Exact metadata-size accounting without revisiting unchanged provider catalogs.
use super::{ByteCounter, Snapshot};
use crate::chat::presentation::Catalogs;

#[derive(Default)]
pub(super) struct Budget {
    catalogs: Option<(Catalogs, usize)>,
    #[cfg(test)]
    catalog_scans: usize,
}
impl Budget {
    pub(super) fn count(&mut self, snapshot: &mut Snapshot) -> usize {
        if self
            .catalogs
            .as_ref()
            .is_none_or(|(previous, _)| !previous.same(snapshot))
        {
            let mut bytes = ByteCounter(0);
            // These arrays replace three empty arrays in the cheap base count.
            let _ = serde_json::to_writer(&mut bytes, &snapshot.models);
            let _ = serde_json::to_writer(&mut bytes, &snapshot.commands);
            let _ = serde_json::to_writer(&mut bytes, &snapshot.skills);
            self.catalogs = Some((Catalogs::of(snapshot), bytes.0.saturating_sub(6)));
            #[cfg(test)]
            {
                self.catalog_scans += 1;
            }
        }
        let models = std::mem::take(&mut snapshot.models);
        let commands = std::mem::take(&mut snapshot.commands);
        let skills = std::mem::take(&mut snapshot.skills);
        let mut bytes = ByteCounter(0);
        let _ = serde_json::to_writer(&mut bytes, &*snapshot);
        snapshot.models = models;
        snapshot.commands = commands;
        snapshot.skills = skills;
        bytes.0.saturating_add(self.catalogs.as_ref().unwrap().1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unchanged_catalog_budget_is_exact_without_revisiting_payload() {
        let mut snapshot = Snapshot {
            models: vec![crate::chat::Model {
                id: "model".into(),
                label: "模型".into(),
                reasoning: vec!["low".into()],
            }]
            .into(),
            skills: vec![crate::chat::Skill {
                name: "review".into(),
                description: "quoted \" skill 🦀".into(),
                invocation: "$review".into(),
            }]
            .into(),
            commands: (0..1000)
                .map(|at| crate::chat::SlashCommand {
                    name: format!("command-{at}"),
                    description: "catalog\\\"description".repeat(100),
                })
                .collect::<Vec<_>>()
                .into(),
            ..Default::default()
        };
        let commands = snapshot.commands.clone();
        let mut budget = Budget::default();
        for generation in 0..100 {
            snapshot.generation = generation;
            snapshot.detail = format!("running-{generation}");
            assert_eq!(
                budget.count(&mut snapshot),
                serde_json::to_vec(&snapshot).unwrap().len()
            );
            assert!(std::sync::Arc::ptr_eq(&commands, &snapshot.commands));
        }
        assert_eq!(budget.catalog_scans, 1);
        snapshot.commands = Default::default();
        assert_eq!(
            budget.count(&mut snapshot),
            serde_json::to_vec(&snapshot).unwrap().len()
        );
        assert_eq!(budget.catalog_scans, 2);
        snapshot.models = snapshot.models.to_vec().into();
        assert_eq!(
            budget.count(&mut snapshot),
            serde_json::to_vec(&snapshot).unwrap().len()
        );
        assert_eq!(
            budget.catalog_scans, 3,
            "equal rows in a replacement allocation must refresh budget evidence"
        );
        snapshot.skills = Default::default();
        assert_eq!(
            budget.count(&mut snapshot),
            serde_json::to_vec(&snapshot).unwrap().len()
        );
        assert_eq!(budget.catalog_scans, 4);
    }
}
