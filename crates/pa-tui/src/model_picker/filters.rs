//! The picker's filter chips and sort key: each chip cycles through its values on its key
//! (`app.model.filter*`, `app.model.sort`), and the chip row above the list shows them all.

use pa_types::ai::Model;

use super::facts::{has_vision, BillingClass};

/// The billing chip: which billing classes the list keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BillingFilter {
    Any,
    Plan,
    PayAsYouGo,
    Free,
}

impl BillingFilter {
    fn next(self) -> Self {
        match self {
            BillingFilter::Any => BillingFilter::Plan,
            BillingFilter::Plan => BillingFilter::PayAsYouGo,
            BillingFilter::PayAsYouGo => BillingFilter::Free,
            BillingFilter::Free => BillingFilter::Any,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            BillingFilter::Any => "any",
            BillingFilter::Plan => "plan",
            BillingFilter::PayAsYouGo => "pay-as-you-go",
            BillingFilter::Free => "free",
        }
    }

    fn admits(self, class: BillingClass) -> bool {
        match self {
            BillingFilter::Any => true,
            BillingFilter::Plan => class == BillingClass::Plan,
            BillingFilter::PayAsYouGo => class == BillingClass::PayAsYouGo,
            BillingFilter::Free => class == BillingClass::Free,
        }
    }
}

/// The capability chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilityFilter {
    Any,
    Vision,
    Reasoning,
}

impl CapabilityFilter {
    fn next(self) -> Self {
        match self {
            CapabilityFilter::Any => CapabilityFilter::Vision,
            CapabilityFilter::Vision => CapabilityFilter::Reasoning,
            CapabilityFilter::Reasoning => CapabilityFilter::Any,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            CapabilityFilter::Any => "any",
            CapabilityFilter::Vision => "vision",
            CapabilityFilter::Reasoning => "reasoning",
        }
    }

    fn admits(self, model: &Model) -> bool {
        match self {
            CapabilityFilter::Any => true,
            CapabilityFilter::Vision => has_vision(model),
            CapabilityFilter::Reasoning => model.reasoning,
        }
    }
}

/// The minimum-context chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextFilter {
    Any,
    AtLeast128k,
    AtLeast200k,
    AtLeast1m,
}

impl ContextFilter {
    fn next(self) -> Self {
        match self {
            ContextFilter::Any => ContextFilter::AtLeast128k,
            ContextFilter::AtLeast128k => ContextFilter::AtLeast200k,
            ContextFilter::AtLeast200k => ContextFilter::AtLeast1m,
            ContextFilter::AtLeast1m => ContextFilter::Any,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            ContextFilter::Any => "any",
            ContextFilter::AtLeast128k => "\u{2265}128k",
            ContextFilter::AtLeast200k => "\u{2265}200k",
            ContextFilter::AtLeast1m => "\u{2265}1M",
        }
    }

    fn minimum(self) -> u64 {
        match self {
            ContextFilter::Any => 0,
            ContextFilter::AtLeast128k => 128_000,
            ContextFilter::AtLeast200k => 200_000,
            ContextFilter::AtLeast1m => 1_000_000,
        }
    }
}

/// How the browse list orders: `Recent` is the grouped view (recent models on top, then one
/// group per provider); the others are one flat list across providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortKey {
    Recent,
    Name,
    Price,
    Context,
}

impl SortKey {
    pub(crate) fn next(self) -> Self {
        match self {
            SortKey::Recent => SortKey::Name,
            SortKey::Name => SortKey::Price,
            SortKey::Price => SortKey::Context,
            SortKey::Context => SortKey::Recent,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            SortKey::Recent => "recent",
            SortKey::Name => "name",
            SortKey::Price => "price",
            SortKey::Context => "context",
        }
    }
}

/// Which chip a key cycled (the adoption event counts each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chip {
    Provider,
    Billing,
    Capability,
    Context,
}

/// The active chips. The provider chip holds a provider id (`None`: every provider).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickerFilters {
    pub(crate) provider: Option<String>,
    pub(crate) billing: BillingFilter,
    pub(crate) capability: CapabilityFilter,
    pub(crate) context: ContextFilter,
}

impl Default for PickerFilters {
    fn default() -> Self {
        Self {
            provider: None,
            billing: BillingFilter::Any,
            capability: CapabilityFilter::Any,
            context: ContextFilter::Any,
        }
    }
}

impl PickerFilters {
    /// Whether one model passes every chip.
    pub(crate) fn admits(&self, model: &Model, class: BillingClass) -> bool {
        self.provider
            .as_ref()
            .is_none_or(|provider| *provider == model.provider)
            && self.billing.admits(class)
            && self.capability.admits(model)
            && model.context_window >= self.context.minimum()
    }

    /// Advance one chip. The provider chip steps through `providers` (the active list's
    /// providers in display order) and back to every provider.
    pub(crate) fn cycle(&mut self, chip: Chip, providers: &[String]) {
        match chip {
            Chip::Provider => {
                let next = match &self.provider {
                    None => providers.first().cloned(),
                    Some(current) => providers
                        .iter()
                        .position(|provider| provider == current)
                        .and_then(|position| providers.get(position + 1))
                        .cloned(),
                };
                self.provider = next;
            }
            Chip::Billing => self.billing = self.billing.next(),
            Chip::Capability => self.capability = self.capability.next(),
            Chip::Context => self.context = self.context.next(),
        }
    }
}
