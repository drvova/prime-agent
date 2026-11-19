//! The billing-mode classifier: how one model call is paid for, from the serving endpoint, the
//! credential's kind, compiled provider metadata, and the call's recorded cost. Pure: the caller
//! resolves the credential to a [`Credential`] kind (the secret itself never enters here).

pub use pa_types::billing::{BillingMode, CostSource};

use crate::types::ModelCost;

/// The kind of credential a call is made with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// No credential: the endpoint takes calls without one.
    None,
    /// A pasted API key, by its shape.
    ApiKey { shape: KeyShape },
    /// A subscription sign-in (`/login`).
    OAuth,
    /// A token-exchange sign-in from a models.json `auth` descriptor.
    TokenExchange,
}

/// The shape of a pasted API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyShape {
    /// An ordinary metered API key.
    Metered,
    /// A Claude subscription token pasted as a key (`sk-ant-oat...`).
    AnthropicSubscriptionToken,
}

/// What [`classify`] decides from.
#[derive(Debug, Clone, Copy)]
pub struct BillingInputs<'a> {
    pub provider: &'a str,
    /// The serving model's base URL.
    pub base_url: &'a str,
    pub credential: Credential,
    /// The serving model's catalog rates.
    pub catalog_cost: &'a ModelCost,
    /// The call's recorded total cost (USD).
    pub recorded_cost: f64,
}

/// Providers whose subscription sign-in is a plan, and whether the provider serves usage
/// windows for it (the `/usage` quota cards).
const OAUTH_PLAN_PROVIDERS: &[(&str, bool)] = &[
    ("anthropic", true),
    ("openai-codex", true),
    ("github-copilot", true),
    ("xai", false),
];

/// Compiled provider metadata: providers that only sell plans (a coding or token plan), keyed by
/// their catalog id. The zai coding endpoint is marked by its base URL below.
const PLAN_PROVIDERS: &[&str] = &[
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-sgp",
    "kimi-coding",
    "opencode-go",
];

/// Providers that price every call themselves (`OpenRouter`'s `usage.cost` on completions).
const COST_REPORTING_PROVIDERS: &[&str] = &["openrouter"];

/// Host suffixes that name a machine on the local network.
const LOCAL_NAME_SUFFIXES: &[&str] = &[".local", ".lan", ".internal", ".home.arpa"];

/// Classify one call. The rules apply in order:
/// 1. a loopback or private base URL, or a keyless provider on a local host name, is `Free`;
/// 2. a subscription sign-in on anthropic, openai-codex, github-copilot or xai (or a Claude
///    subscription token pasted as a key) is a `Plan`;
/// 3. a provider compiled as a plan provider is a `Plan`;
/// 4. a provider that prices calls itself (or a priced call without catalog rates) is
///    `PayAsYouGo { ProviderReported }`;
/// 5. a nonzero catalog rate is `PayAsYouGo { Catalog }`;
/// 6. otherwise `Unknown`: no price is known.
#[must_use]
pub fn classify(inputs: &BillingInputs<'_>) -> BillingMode {
    let host = url::Url::parse(inputs.base_url)
        .ok()
        .and_then(|url| url.host().map(|host| host.to_owned()));
    let private_endpoint = host.as_ref().is_some_and(|host| match host {
        url::Host::Domain(domain) => {
            let domain = domain.to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost")
        }
        url::Host::Ipv4(address) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
        }
        url::Host::Ipv6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                // fc00::/7 unique-local and fe80::/10 link-local.
                || (address.segments()[0] & 0xfe00) == 0xfc00
                || (address.segments()[0] & 0xffc0) == 0xfe80
        }
    });
    let keyless_local = inputs.credential == Credential::None
        && host.as_ref().is_some_and(|host| match host {
            url::Host::Domain(domain) => {
                let domain = domain.to_ascii_lowercase();
                !domain.contains('.')
                    || LOCAL_NAME_SUFFIXES
                        .iter()
                        .any(|suffix| domain.ends_with(suffix))
            }
            url::Host::Ipv4(_) | url::Host::Ipv6(_) => false,
        });
    if private_endpoint || keyless_local {
        return BillingMode::Free;
    }
    let oauth_plan = OAUTH_PLAN_PROVIDERS
        .iter()
        .find(|(provider, _)| *provider == inputs.provider)
        .map(|(_, provider_quota)| *provider_quota);
    let signed_in_plan = match inputs.credential {
        Credential::OAuth => oauth_plan,
        Credential::ApiKey {
            shape: KeyShape::AnthropicSubscriptionToken,
        } if inputs.provider == "anthropic" => oauth_plan,
        Credential::ApiKey { .. } | Credential::None | Credential::TokenExchange => None,
    };
    if let Some(provider_quota) = signed_in_plan {
        return BillingMode::Plan { provider_quota };
    }
    if PLAN_PROVIDERS.contains(&inputs.provider)
        || (inputs.provider == "zai" && inputs.base_url.contains("/api/coding/"))
    {
        return BillingMode::Plan {
            provider_quota: false,
        };
    }
    let cost = inputs.catalog_cost;
    let catalog_priced = [cost.input, cost.output, cost.cache_read, cost.cache_write]
        .iter()
        .any(|rate| rate.as_f64() != 0.0);
    if COST_REPORTING_PROVIDERS.contains(&inputs.provider)
        || (!catalog_priced && inputs.recorded_cost > 0.0)
    {
        return BillingMode::PayAsYouGo {
            source: CostSource::ProviderReported,
        };
    }
    if catalog_priced {
        return BillingMode::PayAsYouGo {
            source: CostSource::Catalog,
        };
    }
    BillingMode::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::zero_model_cost;

    #[test]
    fn each_rule_classifies_its_call() {
        let priced = ModelCost {
            input: 3.0.into(),
            output: 15.0.into(),
            cache_read: 0.3.into(),
            cache_write: 3.75.into(),
        };
        let unpriced = zero_model_cost();
        let metered = Credential::ApiKey {
            shape: KeyShape::Metered,
        };
        let call = |provider, base_url, credential, catalog_cost, recorded_cost| {
            classify(&BillingInputs {
                provider,
                base_url,
                credential,
                catalog_cost,
                recorded_cost,
            })
        };
        let subscription_token = Credential::ApiKey {
            shape: KeyShape::AnthropicSubscriptionToken,
        };
        let anthropic = "https://api.anthropic.com";
        assert_eq!(
            [
                // 1. A private endpoint, and a keyless provider on a local host name.
                call("vllm", "http://192.168.1.20/v1", metered, &priced, 0.5),
                call(
                    "ollama",
                    "http://gpu-box/v1",
                    Credential::None,
                    &unpriced,
                    0.0
                ),
                // 2. A subscription sign-in, and a Claude subscription token pasted as a key.
                call(
                    "openai-codex",
                    "https://chatgpt.com/x",
                    Credential::OAuth,
                    &priced,
                    0.2
                ),
                call("anthropic", anthropic, subscription_token, &priced, 0.2),
                // 3. A compiled plan provider (the zai coding endpoint).
                call(
                    "zai",
                    "https://api.z.ai/api/coding/paas/v4",
                    metered,
                    &priced,
                    0.1
                ),
                // 4. A provider that prices its own calls.
                call(
                    "openrouter",
                    "https://openrouter.ai/api/v1",
                    metered,
                    &priced,
                    0.1
                ),
                // 5. Catalog rates.
                call("anthropic", anthropic, metered, &priced, 0.1),
                // 6. No price known.
                call(
                    "acme",
                    "https://api.acme.example/v1",
                    metered,
                    &unpriced,
                    0.0
                ),
            ],
            [
                BillingMode::Free,
                BillingMode::Free,
                BillingMode::Plan {
                    provider_quota: true
                },
                BillingMode::Plan {
                    provider_quota: true
                },
                BillingMode::Plan {
                    provider_quota: false
                },
                BillingMode::PayAsYouGo {
                    source: CostSource::ProviderReported
                },
                BillingMode::PayAsYouGo {
                    source: CostSource::Catalog
                },
                BillingMode::Unknown,
            ]
        );
    }
}
