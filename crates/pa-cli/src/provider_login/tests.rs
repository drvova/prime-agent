//! The provider-login unit battery (moved beside its module at the
//! 2026-09-30 file-size cap: the flows and their tests stay one tree).

use super::*;

#[test]
fn the_status_indicator_maps_the_ts_labels() {
    let stored_api_key = AuthCredential::ApiKey {
        key: "k".to_string(),
        prime_team: None,
    };
    // A matching stored credential is configured.
    assert_eq!(
        status_indicator(
            Some(&stored_api_key),
            &AuthStatus {
                source: Some(AuthSource::Stored),
                ..Default::default()
            },
            AuthType::ApiKey
        ),
        Some(AuthStatusIndicator {
            style: AuthStatusStyle::Success,
            label: "configured".to_string()
        })
    );
    // A subscription credential on an api-key row warns.
    assert_eq!(
        status_indicator(
            Some(&AuthCredential::Oauth {
                access: "a".to_string(),
                refresh: None,
                expires: 1,
                account_id: None,
                endpoint: None,
                token_endpoint: None,
                client_id: None,
                resource: None,
                issuer: None,
                audience_mode: None,
                enterprise_url: None,
            }),
            &AuthStatus::default(),
            AuthType::ApiKey
        ),
        Some(AuthStatusIndicator {
            style: AuthStatusStyle::Warning,
            label: "subscription configured".to_string()
        })
    );
    // An env key marks the api-key row configured.
    assert_eq!(
        status_indicator(
            None,
            &AuthStatus {
                source: Some(AuthSource::Environment),
                label: Some("OPENAI_API_KEY".to_string()),
                ..Default::default()
            },
            AuthType::ApiKey
        ),
        Some(AuthStatusIndicator {
            style: AuthStatusStyle::Success,
            label: "env: OPENAI_API_KEY".to_string()
        })
    );
    // An unconfigured subscription row stays muted.
    assert_eq!(
        status_indicator(None, &AuthStatus::default(), AuthType::Oauth),
        Some(AuthStatusIndicator {
            style: AuthStatusStyle::Muted,
            label: "unconfigured".to_string()
        })
    );
    // An unconfigured api-key row hides its meta (TS inline rule).
    assert_eq!(
        status_indicator(None, &AuthStatus::default(), AuthType::ApiKey),
        None
    );
    // A stale credential warns.
    assert_eq!(
        status_indicator(
            None,
            &AuthStatus {
                source: Some(AuthSource::Stale),
                label: Some("expired".to_string()),
                ..Default::default()
            },
            AuthType::Oauth
        ),
        Some(AuthStatusIndicator {
            style: AuthStatusStyle::Warning,
            label: "expired".to_string()
        })
    );
}

#[test]
fn the_display_names_match_the_ts_map() {
    assert_eq!(display_name("openai"), "OpenAI");
    assert_eq!(display_name("google"), "Google Gemini");
    assert_eq!(display_name("prime-inference"), "Prime Inference");
    assert_eq!(display_name("a-custom-provider"), "a-custom-provider");
}

#[test]
fn the_api_key_login_rule_matches_ts() {
    let mut built_in = std::collections::HashSet::new();
    built_in.insert("anthropic".to_string());
    // The display-name map marks api-key logins.
    assert!(is_api_key_login_provider("openai", &built_in));
    // A provider the built-in set does not know is a custom api-key
    // provider.
    assert!(is_api_key_login_provider("my-endpoint", &built_in));
    // A built-in provider without a display name is not an api-key
    // login (its subscription flows apply).
    assert!(!is_api_key_login_provider("faux", &{
        let mut set = std::collections::HashSet::new();
        set.insert("faux".to_string());
        set
    }));
}

#[tokio::test]
async fn login_options_list_providers_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::env::remove_var("PRIME_AGENT_TRACES_API_KEY");
    std::env::remove_var("PRIME_API_KEY");
    let auth = ProviderAuth::new(dir.path(), agent.clone());
    let rows = auth.login_options().await;
    // The TS OAuth registry rows render; every subscription flow is
    // ported, so every row selects (the flows answer through the
    // panel — never an after-selection error wall).
    for (id, name) in SUBSCRIPTION_PROVIDERS {
        let row = rows
            .iter()
            .find(|row| row.id == id && row.name == name)
            .unwrap_or_else(|| panic!("the {id} subscription row renders"));
        assert!(
            row.available,
            "the {id} row's flow is ported and selectable"
        );
    }
    // The operator's 2026-09-24 directive: /login is providers only —
    // the service rows (MCP OAuth integrations, the web search
    // credential) never appear; the /mcp view owns MCP logins.
    assert!(!rows.iter().any(|row| row.id == "serper"));
    assert!(!rows.iter().any(|row| row.id.starts_with("mcp:")));
    // Prime Inference sorts first among the api-key rows (TS rule).
    assert!(
        rows.iter()
            .position(|row| row.id == PRIME_INFERENCE_PROVIDER_ID)
            < rows.iter().position(|row| row.id == "anthropic"),
        "prime-inference sorts before the other api-key rows"
    );
}

/// Port of the TS #2645 warning detection: the ban-risk warning is
/// reported exactly when the active Anthropic credential is the
/// subscription (a stored OAuth login or an `sk-ant-oat` key), and
/// its text names the risk.
#[tokio::test]
async fn the_anthropic_subscription_warning_matches_the_active_credential() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::env::remove_var("ANTHROPIC_API_KEY");
    let auth = ProviderAuth::new(dir.path(), agent.clone());

    // No credential: no warning.
    assert_eq!(auth.anthropic_subscription_warning().await, None);

    // A plain API key: no warning (an API key avoids the risk).
    let mut storage = pa_core::auth::AuthStorage::create(&agent);
    storage.set(
        "anthropic",
        AuthCredential::ApiKey {
            key: "sk-ant-api03-plain".to_string(),
            prime_team: None,
        },
    );
    assert_eq!(auth.anthropic_subscription_warning().await, None);

    // A subscription token key (TS `isAnthropicSubscriptionAuthKey`).
    storage.set(
        "anthropic",
        AuthCredential::ApiKey {
            key: "sk-ant-oat-subscription".to_string(),
            prime_team: None,
        },
    );
    let warning = auth
        .anthropic_subscription_warning()
        .await
        .expect("a sk-ant-oat key is the subscription");
    assert!(warning.contains("identifies as Claude Code"));
    assert!(warning.contains("restricted or banned"));
    assert!(warning.contains("An Anthropic API key avoids the risk"));

    // A stored OAuth login is the subscription too.
    storage.set(
        "anthropic",
        AuthCredential::Oauth {
            access: "a".to_string(),
            refresh: None,
            expires: i64::MAX,
            endpoint: None,
            token_endpoint: None,
            client_id: None,
            resource: None,
            issuer: None,
            audience_mode: None,
            account_id: None,
            enterprise_url: None,
        },
    );
    assert!(auth.anthropic_subscription_warning().await.is_some());

    // An env subscription key resolves active with no stored credential.
    storage.remove("anthropic");
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-oat-env");
    assert!(auth.anthropic_subscription_warning().await.is_some());
    std::env::remove_var("ANTHROPIC_API_KEY");
}

#[tokio::test]
async fn login_stores_an_api_key_and_logout_removes_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    let auth = ProviderAuth::new(dir.path(), agent);
    let row = ProviderRow {
        id: "openai".to_string(),
        name: "OpenAI".to_string(),
        auth_type: AuthType::ApiKey,
        status: None,
        flow: AuthFlow::ApiKeyPrompt,
        configured: false,
        available: true,
        credential: RowCredential::Default,
    };
    match auth
        .login(&row, Some("sk-test"), &LoginTarget::ReplaceDefault)
        .await
    {
        ProviderAuthOutcome::Status(message) => {
            assert!(
                message.starts_with("Saved API key for OpenAI. Credentials saved to "),
                "the store status names the credential path: {message}"
            );
        }
        other => panic!("expected the store status, got {other:?}"),
    }
    // An empty key answers the TS error.
    assert_eq!(
        auth.login(&row, Some(""), &LoginTarget::ReplaceDefault)
            .await,
        ProviderAuthOutcome::Error(
            "Failed to save API key for OpenAI: API key cannot be empty.".to_string()
        )
    );
    // The logout lists the stored credential and removes it.
    let stored = auth.logout_options().await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, "openai");
    assert_eq!(
            auth.logout(&stored[0]).await,
            ProviderAuthOutcome::Status(
                "Removed stored API key for OpenAI. Environment variables and models.json config are unchanged."
                    .to_string()
            )
        );
    assert!(auth.logout_options().await.is_empty());
}

#[tokio::test]
async fn a_stored_web_search_key_pairs_with_the_logout_row() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    let auth = ProviderAuth::new(dir.path(), agent);
    // A key stored at the runtime's slot (the `/mcp` view's key flow
    // writes the same store through the same form).
    let row = ProviderRow {
        id: "serper".to_string(),
        name: "Serper (web search)".to_string(),
        auth_type: AuthType::ApiKey,
        status: None,
        flow: AuthFlow::ApiKeyPrompt,
        configured: false,
        available: true,
        credential: RowCredential::Default,
    };
    match auth
        .login(&row, Some("the-key"), &LoginTarget::ReplaceDefault)
        .await
    {
        ProviderAuthOutcome::Status(message) => {
            assert!(
                message.starts_with("Saved API key for Serper (web search)."),
                "the store status: {message}"
            );
        }
        other => panic!("expected the store status, got {other:?}"),
    }
    // The logout row pairs with the stored key: the `/logout` surface
    // lists it by its credential name and removes it.
    let stored = auth.logout_options().await;
    let row = stored
        .iter()
        .find(|row| row.id == "serper")
        .expect("the web-search credential's logout row");
    assert_eq!(row.name, "Serper (web search)");
    assert_eq!(
            auth.logout(row).await,
            ProviderAuthOutcome::Status(
                "Removed stored API key for Serper (web search). Environment variables and models.json config are unchanged."
                    .to_string()
            )
        );
    assert!(auth.logout_options().await.is_empty());
}

/// A models.json provider whose `auth` descriptor declares `oauth` lists as
/// a browser sign-in row through the panel, never as an API-key prompt.
#[tokio::test]
async fn an_oauth_descriptor_provider_lists_as_a_sign_in_row() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    std::fs::write(
        agent.join("models.json"),
        serde_json::json!({ "providers": { "acme": {
            "baseUrl": "https://api.acme.test/v1",
            "api": "openai-completions",
            "auth": {
                "strategy": "oauth",
                "authorizationUrl": "https://auth.acme.test/authorize",
                "tokenUrl": "https://auth.acme.test/token",
                "clientId": "cli"
            },
            "models": [ { "id": "acme-chat" } ]
        } } })
        .to_string(),
    )
    .expect("models.json");
    let rows = ProviderAuth::new(dir.path(), agent).login_options().await;
    let acme: Vec<&ProviderRow> = rows.iter().filter(|row| row.id == "acme").collect();
    assert_eq!(
        acme,
        vec![&ProviderRow {
            id: "acme".to_string(),
            name: "acme".to_string(),
            auth_type: AuthType::Oauth,
            status: Some(AuthStatusIndicator {
                style: AuthStatusStyle::Muted,
                label: "unconfigured".to_string(),
            }),
            flow: AuthFlow::TerminalFlow,
            configured: false,
            available: true,
            credential: RowCredential::Default,
        }]
    );
}

/// "Add another" stores a named credential beside the default slot and leaves `auth.json`
/// byte-identical; `/logout` lists both and removes exactly the one picked.
#[tokio::test]
async fn add_another_keeps_the_default_slot_and_logout_removes_each_credential() {
    let dir = tempfile::tempdir().expect("temp dir");
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).expect("agent dir");
    let auth_json = r#"{"openai":{"type":"api_key","key":"sk-default"}}"#;
    std::fs::write(agent.join("auth.json"), auth_json).unwrap();
    let auth = ProviderAuth::new(dir.path(), agent.clone());
    // The stored default slot makes the login row offer "Add another".
    assert_eq!(
        auth.login_options()
            .await
            .into_iter()
            .find(|row| row.id == "openai")
            .map(|row| row.credential),
        Some(RowCredential::DefaultWithAddAnother)
    );
    let row = ProviderRow {
        id: "openai".to_string(),
        name: "OpenAI".to_string(),
        auth_type: AuthType::ApiKey,
        status: None,
        flow: AuthFlow::ApiKeyPrompt,
        configured: true,
        available: true,
        credential: RowCredential::DefaultWithAddAnother,
    };
    assert_eq!(
        auth.login(
            &row,
            Some("sk-work"),
            &LoginTarget::AddNamed {
                label: "work".to_string()
            }
        )
        .await,
        ProviderAuthOutcome::Status(format!(
            "Saved API key for OpenAI. Added as \"work\". Credentials saved to {}",
            agent.join("auth-accounts.json").display()
        ))
    );
    assert_eq!(
        std::fs::read_to_string(agent.join("auth.json")).unwrap(),
        auth_json
    );
    let accounts = pa_core::auth::AuthAccounts::create(&agent)
        .list("openai")
        .unwrap();
    assert_eq!(accounts.len(), 1);
    let work_id = accounts[0].id.clone();
    let configured = Some(AuthStatusIndicator {
        style: AuthStatusStyle::Success,
        label: "configured".to_string(),
    });
    let default_row = ProviderRow {
        id: "openai".to_string(),
        name: "OpenAI".to_string(),
        auth_type: AuthType::ApiKey,
        status: configured.clone(),
        flow: AuthFlow::TerminalFlow,
        configured: true,
        available: true,
        credential: RowCredential::Default,
    };
    let work_row = ProviderRow {
        name: "OpenAI \u{b7} work".to_string(),
        credential: RowCredential::Named {
            id: work_id,
            label: "work".to_string(),
        },
        ..default_row.clone()
    };
    assert_eq!(
        auth.logout_options().await,
        vec![default_row.clone(), work_row.clone()]
    );
    assert_eq!(
        auth.logout(&work_row).await,
        ProviderAuthOutcome::Status("Removed credential \"work\" for OpenAI.".to_string())
    );
    assert_eq!(auth.logout_options().await, vec![default_row]);
    assert_eq!(
        std::fs::read_to_string(agent.join("auth.json")).unwrap(),
        auth_json
    );
}
