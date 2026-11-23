//! `agent kernel call refusals` (v2): kernel calls refused before the
//! function ran, by what the call got wrong (counts only: never a function,
//! an option's name, or a value). The `cells` capability's rule file names
//! the event; session end emits it through the generic capability counter.

use super::SessionCounters;
use crate::kernel::call_refusals::CallRefusal;

impl SessionCounters {
    /// One refused kernel call.
    pub(crate) fn note_call_refusal(&self, refusal: CallRefusal) {
        self.note_capability(
            match refusal {
                CallRefusal::UnknownOption => "call_refused_unknown_option_count",
                CallRefusal::RepeatedOption => "call_refused_repeated_option_count",
                CallRefusal::OptionWithoutValue => "call_refused_option_without_value_count",
                CallRefusal::Arity => "call_refused_arity_count",
                CallRefusal::ToolArgument => "call_refused_tool_argument_count",
            },
            1,
        );
    }
}
