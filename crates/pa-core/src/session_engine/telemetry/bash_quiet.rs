//! `agent bash quiet` (v2): the `[bash-quiet]` notices the kernel sent about
//! background commands that had all gone silent, whether each woke the
//! session, the commands they named, and their back-off steps (counts only:
//! never a command, output, or a duration). The `bash` capability's rule
//! file names the event; session end emits it through the generic
//! capability counter.

use super::SessionCounters;
use crate::session_engine::bash_notices::{BashQuietFact, NoticeAdmission, QuietStep};

impl SessionCounters {
    /// One `[bash-quiet]` notice the session's handler answered.
    pub(crate) fn note_bash_quiet(&self, fact: BashQuietFact) {
        match fact.admission {
            NoticeAdmission::Taken => self.note_capability("bash_quiet_delivered_count", 1),
            NoticeAdmission::DroppedBusy => {
                self.note_capability("bash_quiet_dropped_busy_count", 1);
            }
            NoticeAdmission::NotTaken => {}
        }
        self.note_capability("bash_quiet_job_count", fact.jobs);
        if let Some(step) = fact.step {
            self.note_capability(
                match step {
                    QuietStep::First => "bash_quiet_step_1_count",
                    QuietStep::Second => "bash_quiet_step_2_count",
                    QuietStep::Third => "bash_quiet_step_3_count",
                },
                1,
            );
        }
    }
}
