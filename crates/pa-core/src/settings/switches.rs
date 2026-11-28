//! The rules a session's live setting switches follow, shared by the old
//! daemon's worker and the runtime daemon: the queue modes and transport a
//! switch persists, the scoped model list it accepts, and the next model or
//! thinking level a cycle lands on. Each daemon applies the result to its
//! own session; nothing here writes.

use pa_types::ai::{Model, Transport};
use serde_json::Value;

use super::{QueueModeSetting, TransportSetting};

/// `set_steering_mode` / `set_follow_up_mode`'s `mode` (`command` names the
/// switch), or the refusal the switch answers.
///
/// # Errors
///
/// The mode is missing or is not `all` or `one-at-a-time`.
pub fn queue_mode(command: &str, mode: Option<&str>) -> Result<QueueModeSetting, String> {
    match mode {
        Some("all") => Ok(QueueModeSetting::All),
        Some("one-at-a-time") => Ok(QueueModeSetting::OneAtATime),
        _ => Err(format!(
            "{command} requires mode \"all\" or \"one-at-a-time\""
        )),
    }
}

/// The setting `set_transport` persists for `transport`: both websocket
/// transports persist as `websocket`.
#[must_use]
pub fn transport_setting(transport: Transport) -> TransportSetting {
    match transport {
        Transport::Auto => TransportSetting::Auto,
        Transport::Sse => TransportSetting::Sse,
        Transport::Websocket | Transport::WebsocketCached => TransportSetting::WebSocket,
    }
}

/// `set_scoped_models`'s `scopedModels`: a list of entries that each name a
/// model (`{"model": {"provider", "id"}}`) and may pin a valid
/// `thinkingLevel`.
///
/// # Errors
///
/// The refusal the switch answers: the list is missing, an entry names no
/// model, or an entry's level is not a thinking level.
pub fn scoped_models(scoped: Option<&Value>) -> Result<&[Value], &'static str> {
    let scoped = scoped
        .and_then(Value::as_array)
        .ok_or("set_scoped_models requires a scopedModels array")?;
    for entry in scoped {
        let names_model = entry
            .get("model")
            .and_then(Value::as_object)
            .is_some_and(|model| {
                model.get("provider").and_then(Value::as_str).is_some()
                    && model.get("id").and_then(Value::as_str).is_some()
            });
        if !names_model {
            return Err("set_scoped_models requires scopedModels entries with a model");
        }
        if let Some(level) = entry.get("thinkingLevel") {
            if level
                .as_str()
                .and_then(pa_types::ai::thinking_levels::thinking_level_from_str)
                .is_none()
            {
                return Err("Invalid thinking level: expected one of off, minimal, low, medium, high, xhigh, max");
            }
        }
    }
    Ok(scoped)
}

/// One model a `cycle_model` may land on, with the level a scoped entry pins
/// for it.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleCandidate {
    pub model: Model,
    pub thinking_level: Option<String>,
}

/// The models `cycle_model` steps through: the scoped entries among the
/// `available` models when a scoped list is set, else every available model.
#[must_use]
pub fn cycle_candidates(scoped: &[Value], available: &[Model]) -> Vec<CycleCandidate> {
    if scoped.is_empty() {
        return available
            .iter()
            .map(|model| CycleCandidate {
                model: model.clone(),
                thinking_level: None,
            })
            .collect();
    }
    scoped
        .iter()
        .filter_map(|entry| {
            let model = entry.get("model")?;
            let provider = model.get("provider")?.as_str()?;
            let id = model.get("id")?.as_str()?;
            let listed = available
                .iter()
                .find(|candidate| candidate.provider == provider && candidate.id == id)?;
            Some(CycleCandidate {
                model: listed.clone(),
                thinking_level: entry
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

/// Which way a cycle steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleDirection {
    Forward,
    Backward,
}

/// The candidate a cycle from the running model (`current`, its provider and
/// id) lands on: the next one in `direction`, counted from the first when the
/// running model is not a candidate; `None` with fewer than two candidates.
#[must_use]
pub fn next_candidate<'a>(
    candidates: &'a [CycleCandidate],
    current: Option<(&str, &str)>,
    direction: CycleDirection,
) -> Option<&'a CycleCandidate> {
    let len = candidates.len();
    if len <= 1 {
        return None;
    }
    let index = current
        .and_then(|(provider, id)| {
            candidates.iter().position(|candidate| {
                candidate.model.provider == provider && candidate.model.id == id
            })
        })
        .unwrap_or(0);
    let next = match direction {
        CycleDirection::Forward => (index + 1) % len,
        CycleDirection::Backward => (index + len - 1) % len,
    };
    candidates.get(next)
}

/// The level `cycle_thinking_level` lands on: the one after `current` among
/// the model's supported `levels`, the first when `current` is not one;
/// `None` when the model supports none.
#[must_use]
pub fn next_thinking_level<'a, T: PartialEq>(levels: &'a [T], current: &T) -> Option<&'a T> {
    match levels.iter().position(|level| level == current) {
        Some(index) => levels.get((index + 1) % levels.len()),
        None => levels.first(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(provider: &str, id: &str) -> Model {
        serde_json::from_value(json!({
            "id": id, "name": id, "api": "openai-completions", "provider": provider,
            "baseUrl": "http://127.0.0.1:9/v1", "reasoning": true, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100,
        }))
        .unwrap()
    }

    #[test]
    fn a_scoped_cycle_steps_through_the_available_entries_with_their_levels() {
        let available = [model("p", "a"), model("p", "b"), model("p", "c")];
        let scoped = [
            json!({ "model": { "provider": "p", "id": "c" }, "thinkingLevel": "high" }),
            json!({ "model": { "provider": "p", "id": "gone" } }),
            json!({ "model": { "provider": "p", "id": "a" } }),
        ];
        let candidates = cycle_candidates(&scoped, &available);
        let landed = |current, direction| {
            next_candidate(&candidates, current, direction)
                .map(|candidate| (candidate.model.id.clone(), candidate.thinking_level.clone()))
        };
        assert_eq!(
            [
                landed(Some(("p", "c")), CycleDirection::Forward),
                landed(Some(("p", "b")), CycleDirection::Forward),
                landed(Some(("p", "c")), CycleDirection::Backward),
            ],
            [
                Some(("a".to_string(), None)),
                Some(("a".to_string(), None)),
                Some(("a".to_string(), None)),
            ]
        );
        assert_eq!(
            landed(Some(("p", "a")), CycleDirection::Forward),
            Some(("c".to_string(), Some("high".to_string())))
        );
        assert_eq!(
            next_candidate(&candidates[..1], None, CycleDirection::Forward),
            None
        );
    }
}
