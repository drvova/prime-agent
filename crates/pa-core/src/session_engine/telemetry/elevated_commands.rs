//! `agent elevated commands` (v2): `bash` commands the model asked to run
//! elevated (`'elevated #t`), by how their consent step ended and the route
//! that asked for it (counts only: never a command, output, or anything the
//! user typed). The `bash` capability's rule file names the event; session
//! end emits it through the generic capability counter.

use super::SessionCounters;
use crate::kernel::elevation_runs::{ConsentRoute, ElevationOutcome, ElevationRun};

impl SessionCounters {
    /// One elevated command's consent step.
    pub(crate) fn note_elevation(&self, run: ElevationRun) {
        let outcome = match run.outcome {
            ElevationOutcome::Approved => "elevated_approved_count",
            ElevationOutcome::Declined => "elevated_declined_count",
            ElevationOutcome::Unavailable => "elevated_unavailable_count",
        };
        let route = match run.route {
            ConsentRoute::Uac => "elevated_uac_count",
            ConsentRoute::SudoAskpass => "elevated_sudo_askpass_count",
            ConsentRoute::Pkexec => "elevated_pkexec_count",
            ConsentRoute::SudoNonInteractive => "elevated_sudo_noninteractive_count",
            ConsentRoute::AlreadyElevated => "elevated_already_elevated_count",
        };
        for property in ["elevated_command_count", outcome, route] {
            self.note_capability(property, 1);
        }
    }
}
