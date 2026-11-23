//! The composer's recent-model cycle (`app.model.cycleRecent`, opencode's F2): each press
//! switches the session to the next model of the settings recent list without opening the
//! picker, and the `tui model recent used` adoption report. The list is the client's own
//! (`recentModels`, kept in step with each switch); the switch rides the picker's `set_model`
//! path.

use super::{AgentView, SessionUi, SetModelOutcome};
use crate::model_picker::ModelPicker;

/// One run of consecutive cycle presses: the order the run walks (the current model first, then
/// the usable recent models, newest first) and where it stands. A run's order is fixed while it
/// lasts, so each press goes one model further back instead of bouncing between the two newest
/// (every switch moves its model to the head of the recent list). A run ends when the session's
/// model is no longer the one the run last switched to: the next press starts over from the
/// fresh list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentCycle {
    order: Vec<String>,
    position: usize,
}

impl RecentCycle {
    /// The model (`provider/id`) the next press switches to, or `None` when no other usable
    /// recent model exists. `cycle` carries the run across presses; `usable` says whether a
    /// recent entry can serve now (listed in the catalog, its provider signed in).
    pub(crate) fn advance(
        cycle: &mut Option<RecentCycle>,
        recent: &[String],
        current: Option<&str>,
        usable: impl Fn(&str) -> bool,
    ) -> Option<String> {
        let continuing = cycle.as_ref().is_some_and(|run| {
            current.is_some_and(|current| {
                run.order.get(run.position).map(String::as_str) == Some(current)
            })
        });
        if !continuing {
            let mut order: Vec<String> = current.map(str::to_string).into_iter().collect();
            for key in recent {
                if !order.contains(key) && usable(key) {
                    order.push(key.clone());
                }
            }
            if order.len() <= usize::from(current.is_some()) {
                *cycle = None;
                return None;
            }
            // The run stands on the current model; without one, the first press lands on the
            // newest recent model.
            let position = match current {
                Some(_) => 0,
                None => order.len() - 1,
            };
            *cycle = Some(RecentCycle { order, position });
        }
        let run = cycle.as_mut()?;
        run.position = (run.position + 1) % run.order.len();
        run.order.get(run.position).cloned()
    }
}

/// Hand one recent-model action's report to the telemetry seam and return at once: the key
/// path never waits on the delivery.
pub(crate) fn report_recent_model_used(
    telemetry: Option<&std::sync::Arc<dyn crate::interactive::InteractionTelemetry>>,
    action: &'static str,
) {
    if let Some(telemetry) = telemetry.cloned() {
        tokio::spawn(async move {
            telemetry.model_recent_used(action).await;
        });
    }
}

impl SessionUi {
    /// The cycle key: switch to the next recent model through the picker's `set_model` path (the
    /// `Model: <id>` row); a list with no other usable model says so.
    pub(super) async fn cycle_recent_model(&mut self, view: &mut AgentView) {
        let current = self
            .current_model(view)
            .map(|model| ModelPicker::model_key_provider(&model.provider, &model.model_id));
        let catalog: std::collections::HashSet<String> = self
            .model_catalog
            .iter()
            .map(|model| ModelPicker::model_key_provider(&model.provider, &model.id))
            .collect();
        let configured = &self.model_configured_providers;
        let next = RecentCycle::advance(
            &mut self.recent_cycle,
            &self.model_recent_models,
            current.as_deref(),
            |key| {
                let provider = key.split_once('/').map_or(key, |(provider, _)| provider);
                catalog.contains(key) && configured.contains(provider)
            },
        );
        let Some(next) = next else {
            self.track_recent_model("cycle_none");
            self.note("No other recent model to switch to", view);
            return;
        };
        let Some((provider, model_id)) = next.split_once('/') else {
            return;
        };
        let (provider, model_id) = (provider.to_string(), model_id.to_string());
        // The switch re-heads the client's recent list; the cycle keeps its own order.
        match self.try_set_model(&provider, &model_id, view).await {
            SetModelOutcome::Switched => self.track_recent_model("cycle"),
            SetModelOutcome::NeedsSignIn => {
                self.recent_cycle = None;
                self.error_row(&format!("Sign in to {provider} to use {model_id}"), view);
            }
            SetModelOutcome::Failed => self.recent_cycle = None,
        }
    }

    /// Report a recent-model action the first time it happens in the run (`tui model recent
    /// used`), fire-and-forget so the key path never waits on the delivery.
    pub(super) fn track_recent_model(&mut self, action: &'static str) {
        if self.recent_model_reported.contains(&action) {
            return;
        }
        self.recent_model_reported.push(action);
        report_recent_model_used(self.telemetry.as_ref(), action);
    }
}

#[cfg(test)]
mod tests {
    use super::RecentCycle;

    fn keys(keys: &[&str]) -> Vec<String> {
        keys.iter().map(ToString::to_string).collect()
    }

    /// Consecutive presses walk the recent list one model further back each time and wrap to the
    /// model the run started on; entries that cannot serve are skipped.
    #[test]
    fn presses_walk_the_recent_list_and_wrap() {
        let recent = keys(&["p/a", "p/b", "q/gone", "p/c"]);
        let usable = |key: &str| key != "q/gone";
        let mut cycle = None;
        let mut current = "p/a".to_string();
        let mut picks = Vec::new();
        for _ in 0..4 {
            let next = RecentCycle::advance(&mut cycle, &recent, Some(&current), usable)
                .expect("another recent model");
            picks.push(next.clone());
            current = next;
        }
        assert_eq!(picks, keys(&["p/b", "p/c", "p/a", "p/b"]));
    }

    /// A switch made elsewhere ends the run: the next press starts from the fresh list, whose
    /// newest other entry is the model the session left.
    #[test]
    fn a_switch_elsewhere_starts_a_new_run() {
        let mut cycle = None;
        let first = RecentCycle::advance(
            &mut cycle,
            &keys(&["p/a", "p/b", "p/c"]),
            Some("p/a"),
            |_| true,
        );
        assert_eq!(first.as_deref(), Some("p/b"));
        // The picker switched to p/c: the recent list re-heads on it.
        let next = RecentCycle::advance(
            &mut cycle,
            &keys(&["p/c", "p/b", "p/a"]),
            Some("p/c"),
            |_| true,
        );
        assert_eq!(next.as_deref(), Some("p/b"));
    }

    /// With no other usable recent model there is nothing to switch to.
    #[test]
    fn a_list_without_another_model_offers_none() {
        let mut cycle = None;
        assert_eq!(
            RecentCycle::advance(&mut cycle, &keys(&["p/a", "q/x"]), Some("p/a"), |key| key
                == "p/a"),
            None
        );
        assert_eq!(cycle, None);
    }
}
