use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use crate::model::{CardId, Exposure, Residency, RuntimeError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleBudget {
    pub max_hot_cards: usize,
    pub offscreen_grace: Duration,
}

impl Default for LifecycleBudget {
    fn default() -> Self {
        Self {
            max_hot_cards: 6,
            offscreen_grace: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleAction {
    SetExposure { card: CardId, exposure: Exposure },
    Hydrate { card: CardId, epoch: u64 },
    Hibernate { card: CardId, epoch: u64 },
}

#[derive(Debug, Clone)]
struct RuntimeCard {
    exposure: Exposure,
    residency: Residency,
    last_visible: Instant,
    epoch: u64,
}

/// Enforces the hot-card budget independently of durable card existence.
///
/// The manager issues generation-tagged actions. Callers must return the same
/// epoch when asynchronous hydration or eviction completes; stale completions
/// are ignored rather than reviving resources after a newer transition.
#[derive(Debug)]
pub struct LifecycleManager {
    budget: LifecycleBudget,
    cards: HashMap<CardId, RuntimeCard>,
}

impl LifecycleManager {
    pub fn new(budget: LifecycleBudget) -> Self {
        assert!(
            budget.max_hot_cards > 0,
            "at least one card must be renderable"
        );
        Self {
            budget,
            cards: HashMap::new(),
        }
    }

    /// Updates policy without disturbing card epochs or residency. The next
    /// reconcile applies tighter limits and revised offscreen grace.
    pub fn update_budget(&mut self, budget: LifecycleBudget) {
        assert!(
            budget.max_hot_cards > 0,
            "at least one card must be renderable"
        );
        self.budget = budget;
    }

    pub fn next_offscreen_sweep_delay(&self, now: Instant) -> Option<Duration> {
        self.cards
            .values()
            .filter(|runtime| {
                runtime.exposure == Exposure::Offscreen
                    && matches!(runtime.residency, Residency::Hot)
            })
            .map(|runtime| {
                self.budget
                    .offscreen_grace
                    .saturating_sub(now.saturating_duration_since(runtime.last_visible))
            })
            .min()
    }

    pub fn register(
        &mut self,
        card: CardId,
        exposure: Exposure,
        residency: Residency,
        now: Instant,
    ) {
        self.cards.insert(
            card,
            RuntimeCard {
                exposure,
                residency,
                last_visible: now,
                epoch: 0,
            },
        );
    }

    pub fn remove(&mut self, card: CardId) {
        self.cards.remove(&card);
    }

    pub fn reconcile(
        &mut self,
        visible: &HashSet<CardId>,
        prefetch: &HashSet<CardId>,
        now: Instant,
    ) -> Vec<LifecycleAction> {
        let mut actions = Vec::new();

        for (&card, runtime) in &mut self.cards {
            let next_exposure = if visible.contains(&card) {
                Exposure::Visible
            } else if prefetch.contains(&card) {
                Exposure::Prefetch
            } else {
                Exposure::Offscreen
            };
            if runtime.exposure != next_exposure {
                runtime.exposure = next_exposure;
                actions.push(LifecycleAction::SetExposure {
                    card,
                    exposure: next_exposure,
                });
            }
            if next_exposure == Exposure::Visible {
                runtime.last_visible = now;
            }
        }

        let mut hydrate_candidates: Vec<_> = self
            .cards
            .iter()
            .filter(|(_, runtime)| {
                runtime.exposure != Exposure::Offscreen
                    && matches!(runtime.residency, Residency::Cold | Residency::Hibernated)
            })
            .map(|(&card, runtime)| (card, runtime.exposure))
            .collect();
        hydrate_candidates.sort_by_key(|(_, exposure)| match exposure {
            Exposure::Visible => 0,
            Exposure::Prefetch => 1,
            Exposure::Offscreen => 2,
        });

        let mut hot = self
            .cards
            .values()
            .filter(|runtime| {
                matches!(
                    runtime.residency,
                    Residency::Hot | Residency::Hydrating { .. }
                )
            })
            .count();
        for (card, _) in hydrate_candidates {
            if hot >= self.budget.max_hot_cards {
                break;
            }
            let runtime = self.cards.get_mut(&card).expect("candidate exists");
            runtime.epoch = runtime.epoch.wrapping_add(1);
            runtime.residency = Residency::Hydrating {
                epoch: runtime.epoch,
            };
            actions.push(LifecycleAction::Hydrate {
                card,
                epoch: runtime.epoch,
            });
            hot += 1;
        }

        let mut eviction_candidates: Vec<_> = self
            .cards
            .iter()
            .filter(|(_, runtime)| {
                runtime.exposure == Exposure::Offscreen
                    && matches!(runtime.residency, Residency::Hot)
            })
            .map(|(&card, runtime)| (card, runtime.last_visible))
            .collect();
        eviction_candidates.sort_by_key(|(_, last_visible)| *last_visible);

        for (card, last_visible) in eviction_candidates {
            let grace_elapsed =
                now.saturating_duration_since(last_visible) >= self.budget.offscreen_grace;
            if !grace_elapsed && hot <= self.budget.max_hot_cards {
                continue;
            }
            let runtime = self.cards.get_mut(&card).expect("candidate exists");
            runtime.epoch = runtime.epoch.wrapping_add(1);
            runtime.residency = Residency::Evicting {
                epoch: runtime.epoch,
            };
            actions.push(LifecycleAction::Hibernate {
                card,
                epoch: runtime.epoch,
            });
            hot = hot.saturating_sub(1);
        }

        actions
    }

    pub fn hydration_finished(
        &mut self,
        card: CardId,
        epoch: u64,
        result: Result<(), RuntimeError>,
    ) -> bool {
        let Some(runtime) = self.cards.get_mut(&card) else {
            return false;
        };
        if runtime.residency != (Residency::Hydrating { epoch }) {
            return false;
        }
        runtime.residency = match result {
            Ok(()) => Residency::Hot,
            Err(typed_error) => Residency::Failed {
                typed_error,
                retryable: true,
            },
        };
        true
    }

    pub fn hibernation_finished(&mut self, card: CardId, epoch: u64) -> bool {
        let Some(runtime) = self.cards.get_mut(&card) else {
            return false;
        };
        if runtime.residency != (Residency::Evicting { epoch }) {
            return false;
        }
        runtime.residency = Residency::Hibernated;
        true
    }

    pub fn state(&self, card: CardId) -> Option<(&Exposure, &Residency)> {
        self.cards
            .get(&card)
            .map(|runtime| (&runtime.exposure, &runtime.residency))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_stale_transition_completions() {
        let now = Instant::now();
        let id = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget::default());
        manager.register(id, Exposure::Offscreen, Residency::Hibernated, now);
        let actions = manager.reconcile(&HashSet::from([id]), &HashSet::new(), now);
        let epoch = actions
            .iter()
            .find_map(|action| match action {
                LifecycleAction::Hydrate { epoch, .. } => Some(*epoch),
                _ => None,
            })
            .unwrap();

        assert!(!manager.hydration_finished(id, epoch.wrapping_add(1), Ok(())));
        assert!(manager.hydration_finished(id, epoch, Ok(())));
        assert_eq!(manager.state(id).unwrap().1, &Residency::Hot);
    }

    #[test]
    fn revised_grace_preserves_existing_offscreen_age() {
        let now = Instant::now();
        let id = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget::default());
        manager.register(id, Exposure::Offscreen, Residency::Hot, now);
        manager.update_budget(LifecycleBudget {
            max_hot_cards: 6,
            offscreen_grace: Duration::from_secs(31),
        });

        assert_eq!(
            manager.next_offscreen_sweep_delay(now + Duration::from_secs(29)),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            manager.next_offscreen_sweep_delay(now + Duration::from_secs(32)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn the_sweep_delay_is_the_earliest_grace_still_held_by_a_hot_offscreen_card() {
        let now = Instant::now();
        let grace = Duration::from_secs(30);
        let oldest = CardId::new();
        let newest = CardId::new();
        let visible = CardId::new();
        let hibernated = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 6,
            offscreen_grace: grace,
        });
        manager.register(oldest, Exposure::Offscreen, Residency::Hot, now);
        manager.register(
            newest,
            Exposure::Offscreen,
            Residency::Hot,
            now + Duration::from_secs(20),
        );
        // Neither of these owns a grace: one is on screen, the other has
        // already been evicted.
        manager.register(visible, Exposure::Visible, Residency::Hot, now);
        manager.register(hibernated, Exposure::Offscreen, Residency::Hibernated, now);

        // The earliest expiry wins, so no card waits past its own grace for a
        // deadline belonging to a card that went offscreen later.
        assert_eq!(
            manager.next_offscreen_sweep_delay(now + Duration::from_secs(25)),
            Some(Duration::from_secs(5))
        );

        // After the sweep that evicts the oldest, the wakeup moves to the card
        // that still holds a grace - the property that lets a sweep re-arm
        // itself instead of leaving a hot card un-swept.
        let swept = now + grace;
        let actions = manager.reconcile(&HashSet::from([visible]), &HashSet::new(), swept);
        let epoch = actions
            .iter()
            .find_map(|action| match action {
                LifecycleAction::Hibernate { card, epoch } if *card == oldest => Some(*epoch),
                _ => None,
            })
            .expect("the oldest card's grace has expired");
        assert!(manager.hibernation_finished(oldest, epoch));
        assert_eq!(
            manager.next_offscreen_sweep_delay(swept),
            Some(Duration::from_secs(20))
        );
    }

    #[test]
    fn hidden_hot_card_is_evicted_after_grace() {
        let now = Instant::now();
        let visible = CardId::new();
        let hidden = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::ZERO,
        });
        manager.register(visible, Exposure::Visible, Residency::Hot, now);
        manager.register(hidden, Exposure::Visible, Residency::Hot, now);

        let actions = manager.reconcile(&HashSet::from([visible]), &HashSet::new(), now);
        assert!(actions.iter().any(|action| matches!(
            action,
            LifecycleAction::Hibernate { card, .. } if *card == hidden
        )));
    }
}
