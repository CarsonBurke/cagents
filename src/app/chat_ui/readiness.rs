//! Draft setup is cached account state, never a provider launch or a frame-time
//! filesystem read. Setup actions leave the unsent composer untouched.
use super::*;
use crate::app::account_ui::ProviderReadiness;

impl ProviderReadiness {
    pub(super) fn draft_status(&self) -> &'static str {
        match self {
            Self::Ready(_) => "Ready",
            Self::Checking => "Checking setup",
            Self::MissingCli => "CLI not installed",
            Self::MissingHome(_) | Self::NeedsLogin(_) => "Not signed in",
            Self::LoginBusy(_) => "Sign-in in progress",
        }
    }
}

impl Cagents {
    pub(super) fn chat_draft_readiness(&self, runtime: &Runtime) -> Option<ProviderReadiness> {
        if !runtime.is_draft() || !Account::PROVIDERS.contains(&runtime.session.launch.provider) {
            return None;
        }
        let launch = &runtime.session.launch;
        let selected = self.chat_account(launch.provider, &launch.recipe);
        Some(self.draft_account_readiness(
            launch.provider,
            &launch.recipe.working_directory,
            selected.as_ref(),
        ))
    }

    pub(super) fn chat_draft_setup_view(&self, runtime: &Runtime) -> Option<Element<'_, Message>> {
        let readiness = self.chat_draft_readiness(runtime)?;
        if matches!(readiness, ProviderReadiness::Ready(_)) {
            return None;
        }
        let provider = runtime.session.launch.provider;
        let target = runtime.target;
        let (message, action) = match readiness {
            ProviderReadiness::Checking => (format!("Checking {} setup…", provider.label()), None),
            ProviderReadiness::MissingCli => (
                format!("{} is not installed on this machine yet.", provider.label()),
                Some(("Install CLI", account_ui::Action::Install(provider))),
            ),
            ProviderReadiness::MissingHome(id) => (
                format!("{} isn't set up on this machine yet.", provider.label()),
                Some(("Sign in", account_ui::Action::SignIn(id))),
            ),
            ProviderReadiness::NeedsLogin(id) => (
                format!("Not signed in to {}.", provider.label()),
                Some(("Sign in", account_ui::Action::SignIn(id))),
            ),
            ProviderReadiness::LoginBusy(id) => (
                format!(
                    "Finish signing in to {} to start this conversation.",
                    provider.label()
                ),
                Some(("Show account", account_ui::Action::ShowAccount(id))),
            ),
            ProviderReadiness::Ready(_) => return None,
        };
        let mut controls = vec![design::body(message).into()];
        if let Some((label, action)) = action {
            controls.push(
                widget::button::custom(design::caption(label))
                    .id(card_control_id(target.card, "chat-setup-action"))
                    .class(crate::ui_controls::control_style(false, None))
                    .padding([step::GAP_XS, step::GAP_S])
                    .on_press(Message::AccountUi(action))
                    .into(),
            );
        }
        controls.push(
            widget::button::custom(design::caption("Use another provider or account"))
                .id(card_control_id(target.card, "chat-setup-alternative"))
                .class(crate::ui_controls::control_style(false, None))
                .padding([step::GAP_XS, step::GAP_S])
                .on_press(Message::ChatUi(Action::ModelPicker(target, true)))
                .into(),
        );
        Some(
            widget::container(
                widget::row::with_children(controls)
                    .spacing(step::GAP_S)
                    .align_y(Alignment::Center),
            )
            .padding(step::GAP_M)
            .width(Length::Fill)
            .into(),
        )
    }

    pub(super) fn reconcile_ready_draft_setup(&mut self) {
        let recovered = self
            .cards
            .iter()
            .filter_map(|(card, runtime)| {
                let CardRuntime::Chat(runtime) = runtime else {
                    return None;
                };
                (runtime.error.as_deref()
                    == Some(
                        "Codex's account setup isn't available. Sign in or choose another account.",
                    )
                    && matches!(
                        self.chat_draft_readiness(runtime),
                        Some(ProviderReadiness::Ready(_))
                    ))
                .then_some(*card)
            })
            .collect::<Vec<_>>();
        for card in recovered {
            if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) {
                runtime.error = None;
            }
            self.notices.resolve(&format!("chat-setup:{card}"));
        }
    }

    /// Reconsider only untouched automatic placeholders. Typing, explicit model
    /// or account selection, and issued conversations never change underneath a user.
    pub(super) fn reconcile_usable_default_chats(&mut self) -> Task<Message> {
        let replacements = self
            .chat_ui
            .default_chats
            .iter()
            .filter_map(|card| {
                if !self.chat_is_unused(*card) {
                    return None;
                }
                let Some(CardRuntime::Chat(runtime)) = self.cards.get(card) else {
                    return None;
                };
                let current = runtime.session.launch.provider;
                let directory = &runtime.session.launch.recipe.working_directory;
                if matches!(
                    self.chat_draft_readiness(runtime),
                    Some(ProviderReadiness::Ready(_))
                ) {
                    return None;
                }
                let provider = self.usable_chat_provider_for_directory(directory)?;
                (provider != current).then_some((runtime.target, provider))
            })
            .collect::<Vec<_>>();
        Task::batch(
            replacements
                .into_iter()
                .map(|(target, provider)| self.choose_draft_model(target, provider, None)),
        )
    }

    pub(super) fn usable_chat_provider_for_directory(&self, directory: &Path) -> Option<Provider> {
        let recent = self
            .workspace
            .focused()
            .and_then(|card| self.card_state(card))
            .into_iter()
            .chain(
                self.durable
                    .cards
                    .iter()
                    .rev()
                    .filter(|card| card.existence.is_open()),
            )
            .filter_map(|card| match &card.kind {
                CardKind::Chat(session) => Some(session.launch.provider),
                _ => None,
            });
        first_usable_provider(recent.chain(MODEL_FAMILIES), |provider| {
            self.usable_default_account_for_directory(provider, directory)
                .is_some()
        })
    }
}

fn first_usable_provider(
    providers: impl IntoIterator<Item = Provider>,
    ready: impl Fn(Provider) -> bool,
) -> Option<Provider> {
    providers.into_iter().find(|provider| ready(*provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_status_never_animates_setup_and_default_selection_skips_unusable_providers() {
        let id = Account::system(Provider::Codex).id();
        for readiness in [
            ProviderReadiness::Checking,
            ProviderReadiness::MissingCli,
            ProviderReadiness::MissingHome(id.clone()),
            ProviderReadiness::NeedsLogin(id.clone()),
            ProviderReadiness::LoginBusy(id.clone()),
            ProviderReadiness::Ready(id),
        ] {
            assert!(!matches!(
                readiness.draft_status(),
                "Connecting" | "Starting"
            ));
        }
        assert_eq!(
            first_usable_provider([Provider::Codex, Provider::Claude], |provider| provider
                == Provider::Claude),
            Some(Provider::Claude)
        );
        assert_eq!(
            first_usable_provider([Provider::Codex, Provider::Claude], |_| false),
            None
        );
        assert_eq!(
            first_usable_provider([Provider::Claude, Provider::Codex], |_| true),
            Some(Provider::Claude)
        );
    }
}
