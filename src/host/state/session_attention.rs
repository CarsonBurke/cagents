use super::*;
use crate::host::model::session_attention::References;

impl HostState {
    pub(in crate::host) fn attention_session_inventory(&self) -> References {
        let mut references = References::default();
        references.extend(self.resident_terminals().0);
        references.extend(self.paused_update_terminals.iter().copied());
        references.extend(self.handoff_terminals.keys().copied());
        for record in self.terminals.values() {
            references.terminal(&record.session);
            for source in &record.upgrade_sources {
                references.terminal(source);
            }
            for relaunch in &record.upgrade_relaunches {
                references.terminal(&relaunch.source);
            }
        }
        for source in self.spawning_terminals.values() {
            references.terminal(source);
        }
        for record in self.chats.values() {
            references.chat(&record.session);
        }
        for source in self.chat_spawning.values() {
            references.chat(source);
        }
        for (source, _) in self.execution_startups.values() {
            references.hosted(source);
        }
        for source in self.upgrade_chats.values().flatten() {
            references.chat(&source.source);
        }
        if let Some(checkpoint) = self.retained_startup_checkpoint() {
            references.checkpoint(checkpoint);
        }
        references
    }
}
