// Modified for Distill by Samuel Fajreldines, 2026.
use agent_client_protocol as acp;
use distill_sampling_types::{ReasoningEffort, ReasoningEffortOption};
use serde::Serialize;

use crate::session::unified_list::SessionKind;

pub(crate) const SELECTABLE_REASONING_EFFORTS: [ReasoningEffort; 5] = [
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::Xhigh,
];

pub(crate) const CONFIG_ID_MODEL: &str = "model";
pub(crate) const CONFIG_ID_REASONING_EFFORT: &str = "reasoning_effort";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionConfigOption {
    pub id: String,
    pub category: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GrokSessionDetail {
    pub session_id: String,
    pub kind: String,
    pub cwd: String,
    pub current_model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl GrokSessionDetail {
    pub(crate) fn build(
        session_id: String,
        cwd: String,
        current_model_id: String,
        title: Option<String>,
    ) -> Self {
        Self {
            session_id,
            kind: SessionKind::Build.as_ref().to_string(),
            cwd,
            current_model_id,
            title,
        }
    }
}

fn effort_label(effort: ReasoningEffort) -> String {
    match effort {
        ReasoningEffort::None => "None",
        ReasoningEffort::Minimal => "Minimal",
        ReasoningEffort::Low => "Low",
        ReasoningEffort::Medium => "Medium",
        ReasoningEffort::High => "High",
        ReasoningEffort::Xhigh => "X-High",
        ReasoningEffort::Max => "Max",
        ReasoningEffort::Ultra => "Ultra",
    }
    .to_string()
}

/// The built-in session-picker modes used when the model has no server list.
/// Reproduces the historical five rows and their labels.
pub(crate) fn legacy_session_effort_options() -> Vec<ReasoningEffortOption> {
    SELECTABLE_REASONING_EFFORTS
        .iter()
        .map(|&effort| ReasoningEffortOption {
            id: effort.as_ref().to_string(),
            value: effort,
            label: effort_label(effort),
            description: None,
            default: false,
        })
        .collect()
}

fn model_display_name(model: &acp::ModelInfo) -> String {
    if model.name.is_empty() {
        model.model_id.0.to_string()
    } else {
        model.name.clone()
    }
}

pub(crate) fn build_session_config_options(
    available_models: &[acp::ModelInfo],
    current_model_id: &acp::ModelId,
    effort_options: &[ReasoningEffortOption],
    current_effort: Option<ReasoningEffort>,
) -> Vec<SessionConfigOption> {
    let mut options = Vec::with_capacity(available_models.len() + effort_options.len());

    for model in available_models {
        options.push(SessionConfigOption {
            id: model.model_id.0.to_string(),
            category: "model".to_string(),
            label: model_display_name(model),
            description: None,
            selected: model.model_id == *current_model_id,
        });
    }

    for effort in effort_options {
        options.push(SessionConfigOption {
            id: effort.id.clone(),
            category: "mode".to_string(),
            label: effort.label.clone(),
            description: effort.description.clone(),
            selected: Some(effort.value) == current_effort,
        });
    }

    options
}

pub(crate) fn build_acp_config_options(
    available_models: &[acp::ModelInfo],
    current_model_id: &acp::ModelId,
    effort_options: &[ReasoningEffortOption],
    current_effort: Option<ReasoningEffort>,
) -> Vec<acp::SessionConfigOption> {
    let mut options = Vec::new();

    if !available_models.is_empty() {
        let values: Vec<acp::SessionConfigSelectOption> = available_models
            .iter()
            .map(|model| {
                acp::SessionConfigSelectOption::new(
                    model.model_id.0.to_string(),
                    model_display_name(model),
                )
            })
            .collect();
        // Keep the real model even if the catalog doesn't list it.
        let current_value = current_model_id.0.to_string();
        options.push(
            acp::SessionConfigOption::select(CONFIG_ID_MODEL, "Model", current_value, values)
                .category(acp::SessionConfigOptionCategory::Model),
        );
    }

    if !effort_options.is_empty() {
        // Keep the real effort even if it isn't a listed option (e.g. none/max);
        // fall back to the model default when none is set.
        let current_value = match current_effort {
            Some(effort) => effort_options
                .iter()
                .find(|option| option.value == effort)
                .map(|option| option.id.clone())
                .unwrap_or_else(|| effort.as_ref().to_string()),
            None => {
                let Some(option) = effort_options
                    .iter()
                    .find(|option| option.default)
                    .or(effort_options.first())
                else {
                    return options;
                };
                option.id.clone()
            }
        };
        let values: Vec<acp::SessionConfigSelectOption> = effort_options
            .iter()
            .map(|option| {
                let mut value =
                    acp::SessionConfigSelectOption::new(option.id.clone(), option.label.clone());
                if let Some(description) = &option.description {
                    value = value.description(description.clone());
                }
                value
            })
            .collect();
        options.push(
            acp::SessionConfigOption::select(
                CONFIG_ID_REASONING_EFFORT,
                "Reasoning Effort",
                current_value,
                values,
            )
            .category(acp::SessionConfigOptionCategory::ThoughtLevel),
        );
    }

    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &'static str, name: &str) -> acp::ModelInfo {
        acp::ModelInfo::new(acp::ModelId::new(id), name.to_string())
    }

    #[test]
    fn options_have_one_selected_model_and_a_mode_per_effort() {
        let models = [model("Distill", "Distill"), model("grok-4.5", "Grok 4.5")];
        let current = acp::ModelId::from("Distill");
        let opts = build_session_config_options(
            &models,
            &current,
            &legacy_session_effort_options(),
            Some(ReasoningEffort::High),
        );

        let model_opts: Vec<_> = opts.iter().filter(|o| o.category == "model").collect();
        assert_eq!(model_opts.len(), 2);
        let selected_models: Vec<_> = model_opts.iter().filter(|o| o.selected).collect();
        assert_eq!(selected_models.len(), 1);
        let Some(selected_model) = selected_models.first() else {
            panic!("expected one selected model: {selected_models:?}");
        };
        assert_eq!(selected_model.id, "Distill");

        let mode_opts: Vec<_> = opts.iter().filter(|o| o.category == "mode").collect();
        assert_eq!(mode_opts.len(), SELECTABLE_REASONING_EFFORTS.len());
        let selected_modes: Vec<_> = mode_opts.iter().filter(|o| o.selected).collect();
        assert_eq!(selected_modes.len(), 1);
        let Some(selected_mode) = selected_modes.first() else {
            panic!("expected one selected mode: {selected_modes:?}");
        };
        assert_eq!(selected_mode.id, "high");
        assert_eq!(selected_mode.label, "High");
    }

    #[test]
    fn none_effort_is_not_a_user_selectable_mode() {
        assert!(!SELECTABLE_REASONING_EFFORTS.contains(&ReasoningEffort::None));
        let models = [model("Distill", "Distill")];
        let current = acp::ModelId::from("Distill");
        let opts = build_session_config_options(
            &models,
            &current,
            &legacy_session_effort_options(),
            Some(ReasoningEffort::None),
        );
        let modes: Vec<_> = opts.iter().filter(|o| o.category == "mode").collect();
        assert!(modes.iter().all(|o| o.id != "none"));
        assert!(modes.iter().all(|o| !o.selected));
    }

    #[test]
    fn no_mode_options_when_model_lacks_effort_support() {
        let models = [model("Distill", "Distill")];
        let current = acp::ModelId::from("Distill");
        let opts = build_session_config_options(&models, &current, &[], None);
        assert_eq!(opts.len(), 1);
        assert!(opts.iter().all(|o| o.category == "model"));
    }

    #[test]
    fn model_label_falls_back_to_id_when_name_empty() {
        let models = [model("Distill", "")];
        let current = acp::ModelId::from("Distill");
        let opts = build_session_config_options(&models, &current, &[], None);
        let Some(first) = opts.first() else {
            panic!("expected one option: {opts:?}");
        };
        assert_eq!(first.label, "Distill");
    }

    #[test]
    fn session_config_option_serializes_camel_case() {
        let opt = SessionConfigOption {
            id: "Distill".to_string(),
            category: "model".to_string(),
            label: "Distill".to_string(),
            description: None,
            selected: true,
        };
        let v = serde_json::to_value(&opt).expect("serialize");
        assert_eq!(v.get("id").and_then(|x| x.as_str()), Some("Distill"));
        assert_eq!(v.get("category").and_then(|x| x.as_str()), Some("model"));
        assert_eq!(v.get("label").and_then(|x| x.as_str()), Some("Distill"));
        assert_eq!(v.get("selected").and_then(|x| x.as_bool()), Some(true));
        assert!(v.get("description").is_none());
    }

    #[test]
    fn grok_session_detail_serializes_camel_case() {
        let detail = GrokSessionDetail::build(
            "sess-1".to_string(),
            "/Users/me/xai".to_string(),
            "Distill".to_string(),
            None,
        );
        let v = serde_json::to_value(&detail).expect("serialize");
        assert_eq!(v.get("sessionId").and_then(|x| x.as_str()), Some("sess-1"));
        assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("build"));
        assert_eq!(v.get("cwd").and_then(|x| x.as_str()), Some("/Users/me/xai"));
        assert_eq!(
            v.get("currentModelId").and_then(|x| x.as_str()),
            Some("Distill")
        );
        assert!(v.get("title").is_none());
    }

    #[test]
    fn acp_config_options_map_model_and_effort_selectors() {
        let models = [model("Distill", "Distill"), model("grok-4.5", "Grok 4.5")];
        let efforts = [ReasoningEffortOption {
            id: "high".to_string(),
            value: ReasoningEffort::High,
            label: "High".to_string(),
            description: None,
            default: false,
        }];

        let options = build_acp_config_options(
            &models,
            &acp::ModelId::from("grok-4.5"),
            &efforts,
            Some(ReasoningEffort::High),
        );

        let expected = vec![
            acp::SessionConfigOption::select(
                CONFIG_ID_MODEL,
                "Model",
                "grok-4.5",
                vec![
                    acp::SessionConfigSelectOption::new("Distill", "Distill"),
                    acp::SessionConfigSelectOption::new("grok-4.5", "Grok 4.5"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::Model),
            acp::SessionConfigOption::select(
                CONFIG_ID_REASONING_EFFORT,
                "Reasoning Effort",
                "high",
                vec![acp::SessionConfigSelectOption::new("high", "High")],
            )
            .category(acp::SessionConfigOptionCategory::ThoughtLevel),
        ];
        assert_eq!(options, expected);
    }

    #[test]
    fn acp_config_options_effort_current_preserves_unlisted_value() {
        let models = [model("grok-4.5", "Grok 4.5")];
        let efforts = [ReasoningEffortOption {
            id: "high".to_string(),
            value: ReasoningEffort::High,
            label: "High".to_string(),
            description: None,
            default: false,
        }];
        let options = build_acp_config_options(
            &models,
            &acp::ModelId::from("grok-4.5"),
            &efforts,
            Some(ReasoningEffort::Low),
        );
        let effort = options
            .iter()
            .find(|o| o.id.0.as_ref() == CONFIG_ID_REASONING_EFFORT)
            .expect("effort selector present when the model supports effort");
        match &effort.kind {
            acp::SessionConfigKind::Select(select) => {
                assert_eq!(select.current_value.0.as_ref(), "low");
            }
            _ => panic!("effort must be a select"),
        }
    }

    #[test]
    fn acp_config_options_model_current_preserves_unlisted_value() {
        let models = [model("Distill", "Distill"), model("grok-4.5", "Grok 4.5")];
        let options =
            build_acp_config_options(&models, &acp::ModelId::from("stale-model"), &[], None);
        let model = options
            .iter()
            .find(|o| o.id.0.as_ref() == CONFIG_ID_MODEL)
            .expect("model selector present");
        match &model.kind {
            acp::SessionConfigKind::Select(select) => {
                assert_eq!(select.current_value.0.as_ref(), "stale-model");
            }
            _ => panic!("model must be a select"),
        }
    }
}
