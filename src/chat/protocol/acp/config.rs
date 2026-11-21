//! What an ACP session lets the reader choose: its model, the model's
//! reasoning level and its permission mode.
//!
//! ACP v1 carries these as session config options, selects whose `category`
//! says what they are for. Agents may also, or instead, report the older
//! `modes` state and the unstable `models` state, which have methods of
//! their own. Config options win where both describe the same choice, since
//! they are the stable interface and an agent keeps them current with
//! `config_option_update`.
use crate::chat::{ActiveModel, Event, Model};
use serde_json::Value;
use std::collections::HashMap;

/// More choices than any picker can usefully show; an agent advertising more
/// is cut short rather than allowed to grow the conversation's metadata.
const MAX_CHOICES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Choice {
    pub value: String,
    pub name: String,
}

/// One select config option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Select {
    /// The identifier `session/set_config_option` names it by.
    pub id: String,
    pub current: String,
    pub choices: Vec<Choice>,
}

impl Select {
    fn parse(option: &Value) -> Option<Self> {
        if option["type"] != "select" {
            return None;
        }
        let mut choices = Vec::new();
        for entry in option["options"].as_array()? {
            // A grouped select nests its options one level down.
            let options = match entry["options"].as_array() {
                Some(grouped) => grouped.iter().collect::<Vec<_>>(),
                None => vec![entry],
            };
            for choice in options {
                if choices.len() == MAX_CHOICES {
                    break;
                }
                let Some(value) = choice["value"].as_str() else {
                    continue;
                };
                choices.push(Choice {
                    value: value.to_owned(),
                    name: choice["name"].as_str().unwrap_or(value).to_owned(),
                });
            }
        }
        Some(Self {
            id: option["id"].as_str()?.to_owned(),
            current: option["currentValue"].as_str()?.to_owned(),
            choices,
        })
    }

    fn name_of(&self, value: &str) -> Option<&str> {
        self.choices
            .iter()
            .find(|choice| choice.value == value)
            .map(|choice| choice.name.as_str())
    }

    fn values(&self) -> Vec<String> {
        self.choices
            .iter()
            .map(|choice| choice.value.clone())
            .collect()
    }
}

/// How the session's model and mode are changed, which depends on what the
/// agent reported them through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Setter {
    /// `session/set_config_option` with this option's identifier.
    Option(String),
    /// `session/set_model`, from the unstable `models` state.
    Model,
    /// `session/set_mode`, from the `modes` state.
    Mode,
}

#[derive(Debug, Default, Clone)]
pub(super) struct Config {
    model: Option<Select>,
    reasoning: Option<Select>,
    mode: Option<Select>,
    /// `models` from session setup, for an agent with no model option.
    legacy_models: Option<Select>,
    /// `modes` from session setup, for an agent with no mode option.
    legacy_modes: Option<Select>,
    /// The reasoning levels each model offered while it was the current
    /// one. A reasoning option lists only the current model's levels, and
    /// models differ: Muse offers a level for some and none for others.
    levels_seen: HashMap<String, Vec<String>>,
}

impl Config {
    /// Takes in whatever of the session's state a setup reply or update
    /// carries: `configOptions`, `modes` and `models` are each optional.
    pub fn learn(&mut self, value: &Value) {
        if let Some(options) = value["configOptions"].as_array() {
            self.learn_options(options);
        }
        if let Some(modes) = value.get("modes").filter(|modes| modes.is_object()) {
            self.legacy_modes = legacy(
                modes,
                "currentModeId",
                "availableModes",
                "id",
                "name",
                "mode",
            );
        }
        if let Some(models) = value.get("models").filter(|models| models.is_object()) {
            self.legacy_models = legacy(
                models,
                "currentModelId",
                "availableModels",
                "modelId",
                "name",
                "model",
            );
        }
        self.remember_levels();
    }

    /// The complete set of config options, as `config_option_update` and
    /// `session/set_config_option` report them: an option no longer listed
    /// no longer exists.
    pub fn learn_options(&mut self, options: &[Value]) {
        let (mut model, mut reasoning, mut mode) = (None, None, None);
        for option in options {
            let slot = match option["category"].as_str() {
                Some("model") => &mut model,
                Some("thought_level") => &mut reasoning,
                Some("mode") => &mut mode,
                _ => continue,
            };
            if slot.is_none() {
                *slot = Select::parse(option);
            }
        }
        (self.model, self.reasoning, self.mode) = (model, reasoning, mode);
        self.remember_levels();
    }

    /// Keeps the levels the reasoning option now lists as the current
    /// model's. Only a listed model is kept, so the record stays as small
    /// as the agent's own list.
    fn remember_levels(&mut self) {
        let Some(models) = self.models() else {
            return;
        };
        if models.name_of(&models.current).is_none() {
            return;
        }
        let current = models.current.clone();
        let levels = self.reasoning.as_ref().map(Select::values);
        self.levels_seen.insert(current, levels.unwrap_or_default());
    }

    /// The combined identifiers in the `models` state, such as agy's
    /// `gemini-4-neon-high`, where they name models' reasoning levels.
    fn combined_models(&self) -> Option<&Select> {
        self.legacy_models.as_ref().filter(|models| {
            models
                .choices
                .iter()
                .any(|choice| self.split_model(&choice.value).is_some())
        })
    }

    /// The reasoning levels `model` offers, as far as the agent has said.
    /// `combined`, from [`Self::combined_models`], names every model's own
    /// levels. Otherwise only the models that have been current are known;
    /// another lists none.
    fn levels_of(&self, model: &str, combined: Option<&Select>) -> Vec<String> {
        match (&self.reasoning, combined) {
            (Some(reasoning), Some(combined)) => reasoning
                .choices
                .iter()
                .filter(|level| {
                    combined
                        .choices
                        .iter()
                        .any(|choice| joins(&choice.value, model, &level.value))
                })
                .map(|level| level.value.clone())
                .collect(),
            _ => self.levels_seen.get(model).cloned().unwrap_or_default(),
        }
    }

    /// Drops the models cagents does not offer from every list of them.
    pub fn retain_models(&mut self, offered: fn(&str) -> bool) {
        for select in [self.model.as_mut(), self.legacy_models.as_mut()]
            .into_iter()
            .flatten()
        {
            select.choices.retain(|choice| offered(&choice.value));
        }
    }

    /// `model` read as an offered model and an offered reasoning level
    /// joined by a hyphen, as a combined model identifier names both.
    pub fn split_model(&self, model: &str) -> Option<(String, String)> {
        self.reasoning.as_ref()?.choices.iter().find_map(|level| {
            let base = model
                .strip_suffix(level.value.as_str())?
                .strip_suffix('-')?;
            self.offers_model(base)
                .then(|| (base.to_owned(), level.value.clone()))
        })
    }

    /// The agent changed its mode itself, as `current_mode_update` reports.
    pub fn mode_changed(&mut self, mode: &str) {
        if let Some(select) = self.mode.as_mut().or(self.legacy_modes.as_mut()) {
            select.current = mode.to_owned();
        }
    }

    /// The agent confirmed a model chosen through `session/set_model`.
    pub fn model_changed(&mut self, model: &str) {
        if let Some(select) = self.legacy_models.as_mut() {
            select.current = model.to_owned();
        }
    }

    fn models(&self) -> Option<&Select> {
        self.model.as_ref().or(self.legacy_models.as_ref())
    }

    fn modes(&self) -> Option<&Select> {
        self.mode.as_ref().or(self.legacy_modes.as_ref())
    }

    pub fn model_setter(&self) -> Option<Setter> {
        match (&self.model, &self.legacy_models) {
            (Some(select), _) => Some(Setter::Option(select.id.clone())),
            (None, Some(_)) => Some(Setter::Model),
            (None, None) => None,
        }
    }

    pub fn reasoning_option(&self) -> Option<&str> {
        self.reasoning.as_ref().map(|select| select.id.as_str())
    }

    pub fn mode_setter(&self) -> Option<Setter> {
        match (&self.mode, &self.legacy_modes) {
            (Some(select), _) => Some(Setter::Option(select.id.clone())),
            (None, Some(_)) => Some(Setter::Mode),
            (None, None) => None,
        }
    }

    pub fn offers_model(&self, model: &str) -> bool {
        self.models()
            .is_some_and(|select| select.name_of(model).is_some())
    }

    /// Whether the current model offers `level`: the option may list
    /// levels another model has, as agy's lists Extra for every model.
    pub fn offers_reasoning(&self, level: &str) -> bool {
        let listed = self
            .reasoning
            .as_ref()
            .is_some_and(|select| select.name_of(level).is_some());
        // A model whose levels nothing has said yet is taken at the
        // option's word.
        let combined = self.combined_models();
        listed
            && self.models().is_none_or(|models| {
                (combined.is_none() && !self.levels_seen.contains_key(&models.current))
                    || self
                        .levels_of(&models.current, combined)
                        .iter()
                        .any(|offered| offered == level)
            })
    }

    pub fn offers_mode(&self, mode: &str) -> bool {
        self.modes()
            .is_some_and(|select| select.name_of(mode).is_some())
    }

    pub fn current_mode(&self) -> Option<&str> {
        self.modes().map(|select| select.current.as_str())
    }

    pub fn mode_ids(&self) -> Vec<String> {
        self.modes().map(Select::values).unwrap_or_default()
    }

    pub fn model_selection(&self) -> bool {
        self.models()
            .is_some_and(|select| !select.choices.is_empty())
    }

    pub fn reasoning_selection(&self) -> bool {
        self.reasoning
            .as_ref()
            .is_some_and(|select| !select.choices.is_empty())
    }

    /// The events that publish the model choice, the active model and the
    /// mode as they now stand.
    pub fn events(&self) -> Vec<Event> {
        let combined = self.combined_models();
        let mut events = Vec::with_capacity(3);
        if let Some(models) = self.models() {
            // A model that offers no level runs at none, whatever the
            // option still says.
            let current_reasoning = self
                .reasoning
                .as_ref()
                .map(|select| select.current.clone())
                .filter(|level| self.offers_reasoning(level));
            events.push(Event::Models {
                models: models
                    .choices
                    .iter()
                    .map(|choice| Model {
                        id: choice.value.clone(),
                        label: choice.name.clone(),
                        reasoning: self.levels_of(&choice.value, combined),
                    })
                    .collect(),
                selected: Some(models.current.clone()),
                reasoning: current_reasoning.clone(),
            });
            events.push(Event::ActiveModel(Some(ActiveModel {
                label: models
                    .name_of(&models.current)
                    .unwrap_or(&models.current)
                    .to_owned(),
                id: models.current.clone(),
                reasoning: current_reasoning,
                default_reasoning: None,
                aliases: Vec::new(),
            })));
        }
        events.push(Event::PermissionMode(
            self.current_mode().map(str::to_owned),
        ));
        events
    }
}

/// A `modes` or `models` state, read as the select it amounts to.
fn legacy(
    state: &Value,
    current: &str,
    available: &str,
    id: &str,
    name: &str,
    setter: &str,
) -> Option<Select> {
    Some(Select {
        id: setter.to_owned(),
        current: state[current].as_str()?.to_owned(),
        choices: state[available]
            .as_array()?
            .iter()
            .take(MAX_CHOICES)
            .filter_map(|entry| {
                let value = entry[id].as_str()?;
                Some(Choice {
                    value: value.to_owned(),
                    name: entry[name].as_str().unwrap_or(value).to_owned(),
                })
            })
            .collect(),
    })
}

/// Whether `combined` is `model` and `level` joined by a hyphen.
fn joins(combined: &str, model: &str, level: &str) -> bool {
    combined
        .strip_suffix(level)
        .and_then(|rest| rest.strip_suffix('-'))
        == Some(model)
}
