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
    /// When the card was last known to be on screen: stamped by every sweep
    /// that finds it there, and once more by the sweep that sees it leave.
    last_visible: Instant,
    /// Came on screen or beside it while the camera was on its way somewhere
    /// else and has not been stopped at since, which is not a use of it and
    /// leaves `last_visible` be.
    crossed: bool,
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
    exposed: HashSet<CardId>,
    hot: HashSet<CardId>,
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
            exposed: HashSet::new(),
            hot: HashSet::new(),
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
        self.hot
            .iter()
            .filter_map(|card| self.cards.get(card))
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
        if exposure == Exposure::Offscreen {
            self.exposed.remove(&card);
        } else {
            self.exposed.insert(card);
        }
        if matches!(residency, Residency::Hot | Residency::Hydrating { .. }) {
            self.hot.insert(card);
        } else {
            self.hot.remove(&card);
        }
        self.cards.insert(
            card,
            RuntimeCard {
                exposure,
                residency,
                last_visible: now,
                crossed: false,
                epoch: 0,
            },
        );
    }

    pub fn remove(&mut self, card: CardId) {
        self.cards.remove(&card);
        self.exposed.remove(&card);
        self.hot.remove(&card);
    }

    pub fn reconcile(
        &mut self,
        visible: &HashSet<CardId>,
        prefetch: &HashSet<CardId>,
        now: Instant,
    ) -> Vec<LifecycleAction> {
        self.reconcile_in_flight(visible, &HashSet::new(), prefetch, now)
    }

    /// [`Self::reconcile`] while the camera is travelling. `passing` are the
    /// `visible` cards it is leaving or crossing rather than heading for. One
    /// that is cold stays cold, and one that was not in use before is not
    /// counted as used now: a card crossed is seen for a few frames, and the
    /// ones being left were in use a moment ago.
    pub fn reconcile_in_flight(
        &mut self,
        visible: &HashSet<CardId>,
        passing: &HashSet<CardId>,
        prefetch: &HashSet<CardId>,
        now: Instant,
    ) -> Vec<LifecycleAction> {
        let mut actions = Vec::new();

        // Cold offscreen cards cannot change during a viewport sweep. Visit
        // only the previous and next exposed bands, not every saved card.
        self.exposed.retain(|card| {
            if visible.contains(card) || prefetch.contains(card) {
                return true;
            }
            if let Some(runtime) = self.cards.get_mut(card) {
                // The grace runs from the card's departure, whether it left
                // from the screen or from the band kept warm beside it. Sweeps
                // only happen on transitions, so the stamp of the last one
                // that saw it visible can be as old as its stay, and would
                // evict it the moment it left.
                if !std::mem::take(&mut runtime.crossed) {
                    runtime.last_visible = now;
                }
                runtime.exposure = Exposure::Offscreen;
                actions.push(LifecycleAction::SetExposure {
                    card: *card,
                    exposure: Exposure::Offscreen,
                });
            }
            false
        });
        for (&card, next_exposure) in visible.iter().map(|card| (card, Exposure::Visible)).chain(
            prefetch
                .iter()
                .filter(|card| !visible.contains(*card))
                .map(|card| (card, Exposure::Prefetch)),
        ) {
            let Some(runtime) = self.cards.get_mut(&card) else {
                continue;
            };
            self.exposed.insert(card);
            let arrived = next_exposure == Exposure::Visible && !passing.contains(&card);
            if arrived || passing.is_empty() {
                // The camera is heading for it, or stopped on it or beside it.
                runtime.crossed = false;
            } else if runtime.exposure == Exposure::Offscreen {
                runtime.crossed = true;
            }
            if !runtime.crossed
                && (next_exposure == Exposure::Visible || runtime.exposure == Exposure::Visible)
            {
                runtime.last_visible = now;
            }
            if runtime.exposure != next_exposure {
                runtime.exposure = next_exposure;
                actions.push(LifecycleAction::SetExposure {
                    card,
                    exposure: next_exposure,
                });
            }
        }

        // Visible work always wins the budget. Iterating the two bands in
        // priority order avoids allocating and sorting hydration candidates.
        let candidates = || {
            visible
                .iter()
                .filter(|card| !passing.contains(*card))
                .map(|card| (card, true))
                .chain(prefetch.iter().map(|card| (card, false)))
        };
        let mut preemptible = None;
        for (card, on_screen) in candidates() {
            if !self.is_cold(card) {
                continue;
            }
            if self.hot.len() >= self.budget.max_hot_cards {
                // A pane the user is looking at does not wait out the grace of
                // one they are not. Warming a neighbour is never worth a slot
                // somebody else holds, and neither band comes after it.
                if !on_screen {
                    break;
                }
                let Some(evicted) = preemptible.get_or_insert_with(|| self.preemptible()).next()
                else {
                    break;
                };
                self.evict(evicted, &mut actions);
            }
            self.hydrate(*card, &mut actions);
        }

        let mut eviction_candidates: Vec<_> = self
            .hot
            .iter()
            .filter_map(|card| self.cards.get(card).map(|runtime| (*card, runtime)))
            .filter(|(_, runtime)| {
                runtime.exposure == Exposure::Offscreen
                    && matches!(runtime.residency, Residency::Hot)
            })
            .map(|(card, runtime)| (card, runtime.last_visible))
            .collect();
        eviction_candidates.sort_by_key(|(_, last_visible)| *last_visible);

        for (card, last_visible) in eviction_candidates {
            let grace_elapsed =
                now.saturating_duration_since(last_visible) >= self.budget.offscreen_grace;
            if !grace_elapsed && self.hot.len() <= self.budget.max_hot_cards {
                continue;
            }
            self.evict(card, &mut actions);
        }

        // Slots the grace just freed go to whoever was turned away above.
        // Nothing else would come back for them: the next sweep is the next
        // change of exposure.
        for (card, _) in candidates() {
            if self.hot.len() >= self.budget.max_hot_cards {
                break;
            }
            if self.is_cold(card) {
                self.hydrate(*card, &mut actions);
            }
        }

        actions
    }

    fn is_cold(&self, card: &CardId) -> bool {
        self.cards.get(card).is_some_and(|runtime| {
            matches!(runtime.residency, Residency::Cold | Residency::Hibernated)
        })
    }

    fn hydrate(&mut self, card: CardId, actions: &mut Vec<LifecycleAction>) {
        let runtime = self.cards.get_mut(&card).expect("candidate exists");
        runtime.epoch = runtime.epoch.wrapping_add(1);
        runtime.residency = Residency::Hydrating {
            epoch: runtime.epoch,
        };
        actions.push(LifecycleAction::Hydrate {
            card,
            epoch: runtime.epoch,
        });
        self.hot.insert(card);
    }

    /// The hot cards a visible one may take a slot from, in the order they
    /// give theirs up: those off screen before those only kept warm beside
    /// it, and among each the one seen longest ago.
    fn preemptible(&self) -> std::vec::IntoIter<CardId> {
        let mut candidates: Vec<_> = self
            .hot
            .iter()
            .filter_map(|card| self.cards.get(card).map(|runtime| (*card, runtime)))
            .filter(|(_, runtime)| {
                runtime.exposure != Exposure::Visible && matches!(runtime.residency, Residency::Hot)
            })
            .map(|(card, runtime)| {
                (
                    runtime.exposure == Exposure::Prefetch,
                    runtime.last_visible,
                    card,
                )
            })
            .collect();
        candidates.sort_by_key(|(warm, last_visible, _)| (*warm, *last_visible));
        candidates
            .into_iter()
            .map(|(.., card)| card)
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn evict(&mut self, card: CardId, actions: &mut Vec<LifecycleAction>) {
        let runtime = self.cards.get_mut(&card).expect("candidate exists");
        runtime.epoch = runtime.epoch.wrapping_add(1);
        runtime.residency = Residency::Evicting {
            epoch: runtime.epoch,
        };
        actions.push(LifecycleAction::Hibernate {
            card,
            epoch: runtime.epoch,
        });
        self.hot.remove(&card);
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
        if !matches!(runtime.residency, Residency::Hot) {
            self.hot.remove(&card);
        }
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
    fn failed_or_removed_hydration_releases_the_hot_budget() {
        let now = Instant::now();
        let first = CardId::new();
        let second = CardId::new();
        let third = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::from_secs(30),
        });
        for card in [first, second, third] {
            manager.register(card, Exposure::Offscreen, Residency::Cold, now);
        }
        let actions = manager.reconcile(&HashSet::from([first]), &HashSet::from([second]), now);
        let epoch = actions
            .iter()
            .find_map(|action| match action {
                LifecycleAction::Hydrate { card, epoch } if *card == first => Some(*epoch),
                _ => None,
            })
            .unwrap();
        assert!(!actions.iter().any(|action| matches!(
            action, LifecycleAction::Hydrate { card, .. } if *card == second
        )));
        assert!(manager.hydration_finished(
            first,
            epoch,
            Err(RuntimeError {
                code: "restore_failed".into(),
                message: "Restore failed".into(),
            })
        ));
        let actions = manager.reconcile(&HashSet::from([second]), &HashSet::new(), now);
        assert!(actions.iter().any(|action| matches!(
            action, LifecycleAction::Hydrate { card, .. } if *card == second
        )));
        assert_eq!(manager.state(first).unwrap().0, &Exposure::Offscreen);
        manager.remove(second);
        let actions = manager.reconcile(&HashSet::from([third]), &HashSet::new(), now);
        assert!(actions.iter().any(|action| matches!(
            action, LifecycleAction::Hydrate { card, .. } if *card == third
        )));
    }

    #[test]
    fn replacing_a_runtime_does_not_leave_stale_exposure_or_hot_budget() {
        let now = Instant::now();
        let replaced = CardId::new();
        let waiting = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::ZERO,
        });
        manager.register(replaced, Exposure::Visible, Residency::Hot, now);
        manager.register(replaced, Exposure::Offscreen, Residency::Hibernated, now);
        manager.register(waiting, Exposure::Offscreen, Residency::Cold, now);
        let actions = manager.reconcile(&HashSet::from([waiting]), &HashSet::new(), now);
        assert!(actions.iter().any(|action| matches!(
            action, LifecycleAction::Hydrate { card, .. } if *card == waiting
        )));
        assert!(!actions.iter().any(|action| matches!(
            action, LifecycleAction::SetExposure { card, .. }
                | LifecycleAction::Hibernate { card, .. } if *card == replaced
        )));
        assert_eq!(manager.next_offscreen_sweep_delay(now), None);
    }
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

    fn hibernated(actions: &[LifecycleAction]) -> Vec<CardId> {
        actions
            .iter()
            .filter_map(|action| match action {
                LifecycleAction::Hibernate { card, .. } => Some(*card),
                _ => None,
            })
            .collect()
    }

    fn hydrated(actions: &[LifecycleAction]) -> Vec<CardId> {
        actions
            .iter()
            .filter_map(|action| match action {
                LifecycleAction::Hydrate { card, .. } => Some(*card),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_visible_card_takes_the_slot_of_the_hidden_card_seen_longest_ago() {
        let now = Instant::now();
        let grace = Duration::from_secs(30);
        let older = CardId::new();
        let newer = CardId::new();
        let arriving = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 2,
            offscreen_grace: grace,
        });
        manager.register(older, Exposure::Offscreen, Residency::Hot, now);
        manager.register(
            newer,
            Exposure::Offscreen,
            Residency::Hot,
            now + Duration::from_secs(1),
        );
        manager.register(arriving, Exposure::Offscreen, Residency::Hibernated, now);

        // Both slots are held by cards still inside their grace. Waiting it
        // out would leave the pane on screen empty for up to that long.
        let sweep = now + Duration::from_secs(2);
        let actions = manager.reconcile(&HashSet::from([arriving]), &HashSet::new(), sweep);
        assert_eq!(hibernated(&actions), [older]);
        assert_eq!(hydrated(&actions), [arriving]);
        assert_eq!(manager.state(newer).unwrap().1, &Residency::Hot);
        // The slot is released before it is taken again.
        let released = actions
            .iter()
            .position(|action| matches!(action, LifecycleAction::Hibernate { .. }));
        let taken = actions
            .iter()
            .position(|action| matches!(action, LifecycleAction::Hydrate { .. }));
        assert!(released < taken);
    }

    #[test]
    fn a_card_the_camera_passes_takes_no_slot_and_does_not_outlast_the_one_left() {
        let now = Instant::now();
        let left = CardId::new();
        let crossed = CardId::new();
        let crossed_cold = CardId::new();
        let destination = CardId::new();
        let next = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 4,
            offscreen_grace: Duration::from_secs(30),
        });
        manager.register(left, Exposure::Visible, Residency::Hot, now);
        // Left a while ago and still within its grace.
        manager.register(crossed, Exposure::Offscreen, Residency::Hot, now);
        for card in [crossed_cold, destination, next] {
            manager.register(card, Exposure::Offscreen, Residency::Hibernated, now);
        }

        // The camera leaves one pane for another across two more. Only where
        // it is going is brought up, though a slot is free for another.
        let departed = now + Duration::from_secs(1);
        let actions = manager.reconcile_in_flight(
            &HashSet::from([left, crossed, crossed_cold, destination]),
            &HashSet::from([left, crossed, crossed_cold]),
            &HashSet::new(),
            departed,
        );
        assert_eq!(hydrated(&actions), [destination]);
        assert!(hibernated(&actions).is_empty());
        assert_eq!(
            manager.state(crossed_cold),
            Some((&Exposure::Visible, &Residency::Hibernated))
        );

        // At rest on the destination, the others off screen in the order the
        // camera left them.
        let actions = manager.reconcile(
            &HashSet::from([destination]),
            &HashSet::new(),
            departed + Duration::from_secs(1),
        );
        assert!(hydrated(&actions).is_empty());
        assert!(hibernated(&actions).is_empty());

        // The budget is not spent until the second of these, which takes the
        // slot of the card that was only crossed: the one left was in use
        // after it, however recently the other was on screen.
        let later = departed + Duration::from_secs(2);
        let actions =
            manager.reconcile(&HashSet::from([destination, next]), &HashSet::new(), later);
        assert_eq!(hydrated(&actions), [next]);
        assert!(hibernated(&actions).is_empty());
        let actions = manager.reconcile(
            &HashSet::from([destination, next, crossed_cold]),
            &HashSet::new(),
            later,
        );
        assert_eq!(hibernated(&actions), [crossed]);
        assert_eq!(hydrated(&actions), [crossed_cold]);
    }

    #[test]
    fn a_card_kept_warm_beside_the_one_left_keeps_its_grace_when_crossed() {
        let now = Instant::now();
        let left = CardId::new();
        let beside = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 2,
            offscreen_grace: Duration::from_secs(30),
        });
        manager.register(left, Exposure::Visible, Residency::Hot, now);
        manager.register(beside, Exposure::Prefetch, Residency::Hot, now);

        let departed = now + Duration::from_secs(60);
        let both = HashSet::from([left, beside]);
        let actions = manager.reconcile_in_flight(&both, &both, &HashSet::new(), departed);
        assert!(hibernated(&actions).is_empty());
        let gone = departed + Duration::from_secs(1);
        let actions = manager.reconcile(&HashSet::new(), &HashSet::new(), gone);
        assert!(hibernated(&actions).is_empty());

        let actions = manager.reconcile(
            &HashSet::new(),
            &HashSet::new(),
            gone + Duration::from_secs(30),
        );
        assert_eq!(hibernated(&actions).len(), 2);
    }

    #[test]
    fn a_card_the_camera_stops_on_after_passing_it_is_one_in_use() {
        let now = Instant::now();
        let hidden = CardId::new();
        let passed = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::from_secs(30),
        });
        manager.register(hidden, Exposure::Offscreen, Residency::Hot, now);
        manager.register(passed, Exposure::Offscreen, Residency::Hibernated, now);

        let actions = manager.reconcile_in_flight(
            &HashSet::from([passed]),
            &HashSet::from([passed]),
            &HashSet::new(),
            now,
        );
        assert!(hydrated(&actions).is_empty());

        let actions = manager.reconcile(&HashSet::from([passed]), &HashSet::new(), now);
        assert_eq!(hibernated(&actions), [hidden]);
        assert_eq!(hydrated(&actions), [passed]);
    }

    #[test]
    fn a_visible_card_takes_a_warm_slot_only_when_no_hidden_card_holds_one() {
        let now = Instant::now();
        let warm = CardId::new();
        let hidden = CardId::new();
        let shown = CardId::new();
        let first = CardId::new();
        let second = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 3,
            offscreen_grace: Duration::from_secs(30),
        });
        // The warm card was seen longer ago than the hidden one, and still
        // outranks it: it is the next pane a scroll would reveal.
        manager.register(warm, Exposure::Prefetch, Residency::Hot, now);
        manager.register(
            hidden,
            Exposure::Offscreen,
            Residency::Hot,
            now + Duration::from_secs(1),
        );
        manager.register(shown, Exposure::Visible, Residency::Hot, now);
        manager.register(first, Exposure::Offscreen, Residency::Hibernated, now);
        manager.register(second, Exposure::Offscreen, Residency::Hibernated, now);
        let sweep = now + Duration::from_secs(2);

        let actions = manager.reconcile(
            &HashSet::from([shown, first]),
            &HashSet::from([warm]),
            sweep,
        );
        assert_eq!(hibernated(&actions), [hidden]);
        assert_eq!(hydrated(&actions), [first]);

        let actions = manager.reconcile(
            &HashSet::from([shown, first, second]),
            &HashSet::from([warm]),
            sweep,
        );
        assert_eq!(hibernated(&actions), [warm]);
        assert_eq!(hydrated(&actions), [second]);

        // A card on screen is never displaced, and one that lost its slot to
        // the screen does not take one back to stay warm.
        let third = CardId::new();
        manager.register(third, Exposure::Offscreen, Residency::Hibernated, now);
        let actions = manager.reconcile(
            &HashSet::from([shown, first, second, third]),
            &HashSet::from([warm]),
            sweep,
        );
        assert_eq!(hibernated(&actions), []);
        assert_eq!(hydrated(&actions), []);
        assert_eq!(
            manager.state(warm).unwrap().1,
            &Residency::Evicting { epoch: 1 }
        );
    }

    #[test]
    fn a_warm_card_waits_for_a_slot_instead_of_taking_one() {
        let now = Instant::now();
        let hidden = CardId::new();
        let warm = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 1,
            offscreen_grace: Duration::from_secs(30),
        });
        manager.register(hidden, Exposure::Offscreen, Residency::Hot, now);
        manager.register(warm, Exposure::Offscreen, Residency::Hibernated, now);

        let actions = manager.reconcile(&HashSet::new(), &HashSet::from([warm]), now);
        assert_eq!(hibernated(&actions), []);
        assert_eq!(hydrated(&actions), []);
    }

    #[test]
    fn the_grace_runs_from_when_a_card_left_the_screen() {
        let now = Instant::now();
        let grace = Duration::from_secs(30);
        let leaving = CardId::new();
        let staying = CardId::new();
        let mut manager = LifecycleManager::new(LifecycleBudget {
            max_hot_cards: 6,
            offscreen_grace: grace,
        });
        manager.register(leaving, Exposure::Visible, Residency::Hot, now);
        manager.register(staying, Exposure::Visible, Residency::Hot, now);

        // Nothing sweeps while a pane simply stays on screen, so the sweep
        // that sees it leave is long after the last one that saw it there.
        let left = now + Duration::from_secs(600);
        let actions = manager.reconcile(&HashSet::from([staying]), &HashSet::from([leaving]), left);
        assert_eq!(hibernated(&actions), []);
        let actions = manager.reconcile(&HashSet::from([staying]), &HashSet::new(), left);
        assert_eq!(hibernated(&actions), []);
        assert_eq!(manager.next_offscreen_sweep_delay(left), Some(grace));

        let actions = manager.reconcile(
            &HashSet::from([staying]),
            &HashSet::new(),
            left + grace - Duration::from_secs(1),
        );
        assert_eq!(hibernated(&actions), []);
        let actions = manager.reconcile(&HashSet::from([staying]), &HashSet::new(), left + grace);
        assert_eq!(hibernated(&actions), [leaving]);
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
