//! What one picker row shows beyond the model's name, from data the client already holds: the
//! billing class (the daemon's per-model mode from the catalog response, else the catalog
//! rates), the price cell, the context cell, and the capability glyphs. Pure: nothing here
//! fetches or reads the disk.

use pa_types::ai::{Model, ModelInput};
use pa_types::billing::BillingMode;

/// How a model's calls are paid for, as the picker groups them (the billing filter chip and the
/// price cell).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BillingClass {
    /// Covered by a subscription plan or a flat fee: no per-call charge.
    Plan,
    /// Metered per call.
    PayAsYouGo,
    /// A local or private endpoint: nothing is billed.
    Free,
    /// No price is known.
    Unpriced,
}

/// Whether the catalog lists any nonzero per-token rate for the model.
fn catalog_priced(model: &Model) -> bool {
    let cost = &model.cost;
    [cost.input, cost.output, cost.cache_read, cost.cache_write]
        .iter()
        .any(|rate| rate.as_f64() > 0.0)
}

/// The model's billing class: the daemon's classification when the catalog response carried one,
/// else the catalog's own rates (a priced model is metered, an unpriced one is unknown).
pub(crate) fn billing_class(mode: Option<&BillingMode>, model: &Model) -> BillingClass {
    match mode {
        Some(BillingMode::Plan { .. } | BillingMode::FlatRate) => BillingClass::Plan,
        Some(BillingMode::PayAsYouGo { .. }) => BillingClass::PayAsYouGo,
        Some(BillingMode::Free) => BillingClass::Free,
        Some(BillingMode::Unknown) => BillingClass::Unpriced,
        None if catalog_priced(model) => BillingClass::PayAsYouGo,
        None => BillingClass::Unpriced,
    }
}

/// One $/1M-token rate, compact: `$3`, `$0.25`, `$0.075`, `$0` for a free rate.
pub(crate) fn money(rate: f64) -> String {
    if !rate.is_finite() || rate <= 0.0 {
        return "$0".to_string();
    }
    if rate >= 100.0 {
        return format!("${rate:.0}");
    }
    let text = format!("{rate:.3}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("${text}")
}

/// The price cell: input/output per 1M tokens for a metered model, else what the billing class
/// says (`plan`, `free`, `—` for an unknown price).
pub(crate) fn price_cell(class: BillingClass, model: &Model) -> String {
    match class {
        BillingClass::Plan => "plan".to_string(),
        BillingClass::Free => "free".to_string(),
        BillingClass::Unpriced => "\u{2014}".to_string(),
        BillingClass::PayAsYouGo if catalog_priced(model) => format!(
            "{}/{}",
            money(model.cost.input.as_f64()),
            money(model.cost.output.as_f64())
        ),
        // Metered by the provider's own pricing, with no catalog rates to show.
        BillingClass::PayAsYouGo => "metered".to_string(),
    }
}

/// The price a price sort orders by: nothing for a plan or free model, the input plus output rate
/// for a metered one, and last (`None`) when no price is known.
pub(crate) fn sort_price(class: BillingClass, model: &Model) -> Option<f64> {
    match class {
        BillingClass::Plan | BillingClass::Free => Some(0.0),
        BillingClass::PayAsYouGo if catalog_priced(model) => {
            Some(model.cost.input.as_f64() + model.cost.output.as_f64())
        }
        BillingClass::PayAsYouGo | BillingClass::Unpriced => None,
    }
}

/// A context window in tokens, compact: `200k`, `128k`, `1M`, `1.5M`; an unknown window is `—`.
pub(crate) fn context_cell(tokens: u64) -> String {
    if tokens == 0 {
        return "\u{2014}".to_string();
    }
    if tokens >= 1_000_000 {
        let millions = tokens as f64 / 1_000_000.0;
        if (millions - millions.round()).abs() < 0.05 {
            return format!("{millions:.0}M");
        }
        return format!("{millions:.1}M");
    }
    format!("{}k", (tokens + 500) / 1000)
}

/// The vision glyph: the model reads images.
pub(crate) const VISION_GLYPH: &str = "\u{25c9}";
/// The reasoning glyph: the model has a thinking surface.
pub(crate) const REASONING_GLYPH: &str = "\u{25c6}";
/// The tools glyph: the model takes tool calls. Every catalog entry does (the catalog drops
/// routes without tool support at ingest), so the glyph is constant across rows.
pub(crate) const TOOLS_GLYPH: &str = "\u{192}";
/// A capability the model lacks.
const ABSENT_GLYPH: &str = "\u{b7}";

/// Whether the model reads images.
pub(crate) fn has_vision(model: &Model) -> bool {
    model.input.contains(&ModelInput::Image)
}

/// The capability cell: vision, reasoning, tools, one column each (`·` where absent).
pub(crate) fn capability_cell(model: &Model) -> String {
    let vision = if has_vision(model) {
        VISION_GLYPH
    } else {
        ABSENT_GLYPH
    };
    let reasoning = if model.reasoning {
        REASONING_GLYPH
    } else {
        ABSENT_GLYPH
    };
    format!("{vision}{reasoning}{TOOLS_GLYPH}")
}
