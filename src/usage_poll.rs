//! Background poller for OAuth usage APIs — Claude's official one and Codex's
//! private `wham/usage` one.
//!
//! When `[server.pool] usage_refresh_seconds` is set, this spawns one task at
//! boot that periodically polls, for every imported (refreshable) account:
//! `GET /api/oauth/usage` across all `claude_oauth` providers, and the private
//! `GET /wham/usage` (see [`crate::auth::codex::usage`]) across all ChatGPT
//! backend `chatgpt_oauth` providers — applying the returned utilization to the
//! account pool via [`AccountPool::note_usage`].
//!
//! Why: the pool's primary quota signal is the response headers on proxied
//! traffic (`anthropic-ratelimit-unified-*` for Claude, `x-codex-*` for Codex),
//! which only reflect traffic that actually flowed through shunt. When the same
//! account is also used out-of-band (the operator's own CLI, another tool), or
//! is currently excluded from rotation on a near-quota mark, that account's
//! consumption — and its eventual window reset — is invisible to the headers
//! and the pool can undercount or stay stuck excluding a recovered account. The
//! usage APIs report ground-truth utilization, so a periodic poll reconciles
//! the header-derived state.
//!
//! Eligibility: only imported logins can call either endpoint. A long-lived
//! Claude `setup-token`, and a `token_env`-supplied credential of either
//! family, are treated as static and skipped, mirroring the adapters'
//! non-refreshable 401 handling. The Codex arm additionally only polls
//! providers on the ChatGPT backend ([`crate::config::Config::is_chatgpt_backend`])
//! — a `chatgpt_oauth` provider pointed at some other base is never polled.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;

use crate::{
    accounts::AccountPool,
    auth::{self, claude, codex, resolve_chatgpt_account, resolve_claude_account, Credential},
    config::{AccountConfig, AuthMode, Config},
    server::AppState,
};

/// Spawn the usage poller if `[server.pool] usage_refresh_seconds` enables it.
/// A no-op otherwise, so the default deployment adds no background work. Whether
/// the task exists is decided once from the boot config (like the admin and
/// codex surfaces); a reload does not start or stop it.
pub fn spawn_usage_poller(state: AppState) {
    let Some(pool) = state.config.server.pool.as_ref() else {
        return;
    };
    let Some(interval) = pool.usage_refresh_interval() else {
        return;
    };
    // The interval floor is applied silently in config; surface the clamp so an
    // operator who set e.g. 30 isn't left wondering why the log below shows 60.
    if let Some(configured) = pool.usage_refresh_seconds {
        if configured != interval {
            tracing::warn!(
                configured_seconds = configured,
                effective_seconds = interval,
                "usage_refresh_seconds is below the 60s floor; using 60"
            );
        }
    }
    tracing::info!(
        interval_seconds = interval,
        "starting OAuth usage-API poller (Claude, Codex)"
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval));
        // A poll that runs long (or a suspend/resume) must not make the ticker
        // fire a burst of catch-up ticks — that would hammer the usage API. Skip
        // missed ticks and resume on the regular cadence.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // `interval` fires its first tick immediately, so pool state is seeded at
        // startup and then refreshed every `interval` seconds.
        loop {
            ticker.tick().await;
            poll_all(&state).await;
        }
    });
}

/// Poll every imported account of every `claude_oauth` and ChatGPT-backend
/// `chatgpt_oauth` provider once. Re-snapshots the live shared state so a
/// reloaded provider list / account set is picked up. The physical-account
/// dedup set (`polled`) is shared across both families: a Claude and a Codex
/// upstream can never collide on it (they resolve from disjoint stores), but
/// sharing it here mirrors how a single family already dedups aliases of the
/// same account across multiple upstream names.
async fn poll_all(state: &AppState) {
    let state = state.refreshed();
    let mut polled = HashSet::new();
    for (name, provider) in &state.config.providers {
        match provider.auth {
            AuthMode::ClaudeOauth => {
                let accounts = match auth::shared::resolve_pool_accounts(
                    "Claude",
                    &provider.accounts,
                    &provider.account_scope,
                    crate::accounts::StoreFamily::Claude,
                    claude::store::default_accounts_dir(),
                    claude::store::scan_accounts,
                )
                .await
                {
                    Ok(accounts) => accounts,
                    Err(error) => {
                        tracing::debug!(provider = %name, %error, "usage poller: failed to resolve accounts");
                        continue;
                    }
                };
                state.accounts.sync_enabled_accounts(name, &accounts);
                for account in &accounts {
                    if !account_is_refreshable(account).await {
                        continue;
                    }
                    let key = crate::accounts::account_key(name, account);
                    if polled.contains(&key) {
                        continue;
                    }
                    // Mark the physical identity polled only after a snapshot is applied,
                    // so a later valid alias for the same account still reconciles when an
                    // earlier alias fails to resolve its credential or fetch usage.
                    if poll_account(
                        &state.http_client,
                        &state.accounts,
                        name,
                        &provider.base_url,
                        account,
                    )
                    .await
                    {
                        polled.insert(key);
                    }
                }
            }
            AuthMode::ChatgptOauth if state.config.is_chatgpt_backend(name) => {
                let accounts = match auth::shared::resolve_pool_accounts(
                    "Codex",
                    &provider.accounts,
                    &provider.account_scope,
                    crate::accounts::StoreFamily::Chatgpt,
                    codex::store::default_accounts_dir(),
                    codex::store::scan_accounts,
                )
                .await
                {
                    Ok(accounts) => accounts,
                    Err(error) => {
                        tracing::debug!(provider = %name, %error, "usage poller: failed to resolve codex accounts");
                        continue;
                    }
                };
                state.accounts.sync_enabled_accounts(name, &accounts);
                for account in &accounts {
                    if !codex_account_is_refreshable(account).await {
                        continue;
                    }
                    let key = crate::accounts::account_key(name, account);
                    if polled.contains(&key) {
                        continue;
                    }
                    if poll_codex_account(
                        &state.http_client,
                        &state.accounts,
                        &state.config,
                        name,
                        &provider.base_url,
                        account,
                    )
                    .await
                    {
                        polled.insert(key);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Poll one Claude account: skip non-refreshable credentials, resolve a valid
/// access token, fetch its usage, and apply it to the pool. Every failure
/// degrades quietly to a debug log — a missing snapshot just leaves the
/// header-derived state in place until the next tick. Returns `true` only when
/// a usage snapshot was applied, so the caller can leave the physical identity
/// un-deduplicated and let a later alias retry when this one fails to resolve
/// or fetch.
async fn poll_account(
    client: &reqwest::Client,
    pool: &AccountPool,
    provider: &str,
    base_url: &str,
    account: &AccountConfig,
) -> bool {
    if !account_is_refreshable(account).await {
        return false;
    }
    let credential = match resolve_claude_account(account, client).await {
        Ok(credential) => credential,
        Err(error) => {
            tracing::debug!(provider, account = %account.name, error = %error.message, "usage poller: failed to resolve account credential");
            return false;
        }
    };
    let Credential::ClaudeOauth { access_token, .. } = credential else {
        return false;
    };
    match claude::usage::fetch_usage(client, base_url, &access_token).await {
        Ok(snapshot) => {
            // The Claude parser intentionally accepts partial responses, but
            // an entirely unrecognizable 200 body also becomes an all-None
            // snapshot. Applying that would mark the account observed and let
            // `poll_all` dedup every other alias without any quota signal.
            if snapshot.is_empty() {
                tracing::debug!(provider, account = %account.name, "usage poller: claude usage snapshot reported no windows, skipping");
                return false;
            }
            pool.note_usage(provider, account, &snapshot);
            tracing::debug!(provider, account = %account.name, "usage poller: applied usage snapshot");
            true
        }
        Err(error) => {
            tracing::debug!(provider, account = %account.name, %error, "usage poller: usage fetch failed");
            false
        }
    }
}

/// Poll one Codex (ChatGPT) account's private `wham/usage` endpoint: skip
/// non-refreshable credentials, resolve a valid ChatGPT access token, fetch its
/// usage, and apply it to the pool. Mirrors [`poll_account`], with one addition:
/// it re-derives [`Config::is_chatgpt_backend`] itself rather than trusting the
/// caller's own dispatch already filtered on it — the same defense-in-depth
/// `adapters::responses::request::responses_url` uses before choosing
/// `/codex/responses` — so a future caller of this function directly can never
/// leak a wham request to a `chatgpt_oauth` provider pointed at a different
/// base. Every failure degrades quietly to a debug log.
async fn poll_codex_account(
    client: &reqwest::Client,
    pool: &AccountPool,
    config: &Config,
    provider: &str,
    base_url: &str,
    account: &AccountConfig,
) -> bool {
    if !config.is_chatgpt_backend(provider) {
        return false;
    }
    if !codex_account_is_refreshable(account).await {
        return false;
    }
    let credential = match resolve_chatgpt_account(account, client).await {
        Ok(credential) => credential,
        Err(error) => {
            tracing::debug!(provider, account = %account.name, error = %error.message, "usage poller: failed to resolve codex account credential");
            return false;
        }
    };
    let Credential::ChatGptOAuth {
        access_token,
        account_id,
    } = credential
    else {
        return false;
    };
    match codex::usage::fetch_usage(client, base_url, &access_token, &account_id).await {
        Ok(snapshot) => {
            // A recognizable-but-empty response (e.g. `{"primary_window":{}}`
            // for a brand-new account with no consumption yet) parses as `Ok`
            // with every window `None` -- see the parser's own doc comment.
            // Applying that to the pool would mark the account observed and
            // let `poll_all`'s physical-account dedup skip every other alias,
            // permanently starving this account of a real observation.
            if snapshot.is_empty() {
                tracing::debug!(provider, account = %account.name, "usage poller: codex wham usage snapshot reported no windows, skipping");
                return false;
            }
            pool.note_usage(provider, account, &snapshot);
            tracing::debug!(provider, account = %account.name, "usage poller: applied codex wham usage snapshot");
            true
        }
        Err(error) => {
            tracing::debug!(provider, account = %account.name, %error, "usage poller: codex wham usage fetch failed");
            false
        }
    }
}

/// Whether an account's credential is a refreshable imported login — the only
/// kind the usage API accepts. `token_env` credentials are treated as static.
/// The credential file (an explicit `credentials` path, or the store path for a
/// name-only account) is read on the blocking pool.
async fn account_is_refreshable(account: &AccountConfig) -> bool {
    if account.token_env.is_some() {
        return false;
    }
    let path = account
        .credentials
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| claude::store::account_path(&account.name));
    tokio::task::spawn_blocking(move || credential_file_has_refresh_token(&path))
        .await
        .unwrap_or(false)
}

/// True when the credential file holds a non-empty `claudeAiOauth.refreshToken`
/// — the signal the store uses to classify an imported login (vs a setup token).
fn credential_file_has_refresh_token(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| {
            value
                .pointer("/claudeAiOauth/refreshToken")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|token| !token.is_empty())
}

/// Whether a Codex account's credential is a refreshable imported login — the
/// only kind the wham/usage endpoint accepts. `token_env` credentials are
/// treated as static. The credential file (an explicit `credentials` path, or
/// the store path for a name-only account) is read on the blocking pool.
async fn codex_account_is_refreshable(account: &AccountConfig) -> bool {
    if account.token_env.is_some() {
        return false;
    }
    let path = account
        .credentials
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| codex::store::account_path(&account.name));
    tokio::task::spawn_blocking(move || codex_credential_file_has_refresh_token(&path))
        .await
        .unwrap_or(false)
}

/// True when the credential file holds a non-empty `tokens.refresh_token` — the
/// signal a `codex login` import carries, unlike a static or non-refreshable
/// credential.
fn codex_credential_file_has_refresh_token(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| {
            value
                .pointer("/tokens/refresh_token")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "shunt-usage-poll-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn account_with_credentials(path: &Path) -> AccountConfig {
        AccountConfig {
            name: "acct".to_string(),
            credentials: Some(path.to_string_lossy().into_owned()),
            token_env: None,
            uuid: None,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn refreshable_only_for_imported_credential_files() {
        // Imported login: has a non-empty refreshToken -> eligible.
        let imported = write_temp(
            "imported",
            r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        assert!(account_is_refreshable(&account_with_credentials(&imported)).await);

        // Setup token: no refreshToken -> not eligible.
        let setup = write_temp(
            "setup",
            r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":4000000000000,"shuntCredentialKind":"setup_token"}}"#,
        );
        assert!(!account_is_refreshable(&account_with_credentials(&setup)).await);

        // token_env credential is static regardless of any file.
        let env_account = AccountConfig {
            name: "env".to_string(),
            credentials: None,
            token_env: Some("SOME_ENV".to_string()),
            uuid: None,
            ..Default::default()
        };
        assert!(!account_is_refreshable(&env_account).await);

        // Missing file -> not eligible (no panic).
        let missing = AccountConfig {
            name: "nope".to_string(),
            credentials: Some("/no/such/shunt/usage/file.json".to_string()),
            token_env: None,
            uuid: None,
            ..Default::default()
        };
        assert!(!account_is_refreshable(&missing).await);

        for path in [imported, setup] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[tokio::test]
    async fn poll_account_fetches_and_applies_snapshot() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // An imported credential whose access token is far from expiry, so
        // resolve_claude_account returns it without hitting the token endpoint.
        let creds = write_temp(
            "poll",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        let account = account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": { "utilization": 20.0 },
                "seven_day": { "utilization": 75.0 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        poll_account(
            &reqwest::Client::new(),
            &pool,
            "anthropic",
            &server.uri(),
            &account,
        )
        .await;

        let snap = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert_eq!(snap.len(), 1);
        assert!(snap[0].has_state, "the poll must have recorded state");
        assert_eq!(snap[0].utilization_5h, Some(0.20));
        assert_eq!(snap[0].utilization_7d, Some(0.75));

        let _ = std::fs::remove_file(creds);
    }

    #[tokio::test]
    async fn poll_account_records_no_state_on_fetch_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A refreshable credential whose usage fetch fails (500): the poller must
        // degrade quietly, leaving the account with no recorded state.
        let creds = write_temp(
            "fetch-error",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        let account = account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        poll_account(
            &reqwest::Client::new(),
            &pool,
            "anthropic",
            &server.uri(),
            &account,
        )
        .await;

        let snap = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert!(
            !snap[0].has_state,
            "a failed usage fetch must not record state"
        );

        let _ = std::fs::remove_file(creds);
    }

    /// A successful HTTP response whose JSON shape the Claude usage parser
    /// does not recognize must not mark the account observed or dedup its
    /// other aliases. The tolerant parser returns an all-None snapshot here,
    /// so the poller owns this guard rather than turning the response into a
    /// parser error.
    #[tokio::test]
    async fn poll_account_skips_note_usage_on_unrecognizable_200_body() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let creds = write_temp(
            "claude-empty",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        let account = account_with_credentials(&creds);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "garbage": true })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        let applied = poll_account(
            &reqwest::Client::new(),
            &pool,
            "anthropic",
            &server.uri(),
            &account,
        )
        .await;
        assert!(!applied, "an empty snapshot must not count as applied");
        let snap = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert!(
            !snap[0].has_state,
            "an empty snapshot must not mark the account observed"
        );

        let _ = std::fs::remove_file(creds);
    }

    /// A partial Claude response remains applicable when one malformed window
    /// is accompanied by a valid Fable-scoped weekly window. This pins the
    /// third `UsageSnapshot` field as a sufficient signal on its own.
    #[tokio::test]
    async fn poll_account_applies_partial_snapshot_with_only_fable_window() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let creds = write_temp(
            "claude-partial",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        let account = account_with_credentials(&creds);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": { "utilization": "malformed" },
                "limits": [{
                    "kind": "weekly_scoped",
                    "scope": { "model": { "display_name": "Fable" } },
                    "percent": 42.0
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        let applied = poll_account(
            &reqwest::Client::new(),
            &pool,
            "anthropic",
            &server.uri(),
            &account,
        )
        .await;
        assert!(applied, "a valid Fable window must make the snapshot apply");
        let snap = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert!(snap[0].has_state);
        assert_eq!(snap[0].utilization_5h, None);
        assert_eq!(snap[0].utilization_7d, None);
        assert_eq!(snap[0].utilization_7d_oi, Some(0.42));

        let _ = std::fs::remove_file(creds);
    }

    #[tokio::test]
    async fn poll_all_polls_only_claude_oauth_providers() {
        use crate::config::{ApiKeyHeader, Config, CountTokens, ProviderConfig, ProviderKind};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let creds = write_temp(
            "poll-all",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        let account = account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": { "utilization": 12.0 },
                "seven_day": { "utilization": 34.0 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Start from the default config (its `anthropic` provider is passthrough,
        // so `poll_all` must skip it) and add one `claude_oauth` provider pointed
        // at the mock usage server with an explicit imported account.
        let mut config = Config::default();
        config.providers.insert(
            "claude-pool".to_string(),
            ProviderConfig {
                kind: ProviderKind::Anthropic,
                base_url: server.uri(),
                auth: AuthMode::ClaudeOauth,
                api_key_env: None,
                api_key_header: ApiKeyHeader::Bearer,
                effort: None,
                service_tier: None,
                count_tokens: CountTokens::default(),
                accounts: vec![account.clone()],
                account_scope: Vec::new(),
                websocket: false,
                tool_search: None,
                request_compression: true,
                retry: Default::default(),
                workspace_roots: Vec::new(),
                sandbox: true,
            },
        );
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        poll_all(&state).await;

        let snap =
            state
                .accounts
                .snapshot("claude-pool", std::slice::from_ref(&account), None, None);
        assert_eq!(snap.len(), 1);
        assert!(snap[0].has_state, "poll_all must apply the usage snapshot");
        assert_eq!(snap[0].utilization_5h, Some(0.12));
        assert_eq!(snap[0].utilization_7d, Some(0.34));

        let _ = std::fs::remove_file(creds);
    }

    #[tokio::test]
    async fn poll_all_deduplicates_physical_accounts_across_upstreams() {
        use crate::config::{ApiKeyHeader, Config, CountTokens, ProviderConfig, ProviderKind};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let creds = write_temp(
            "poll-dedup",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000},"shuntAccountUuid":"shared-uuid"}"#,
        );
        let mut account = account_with_credentials(&creds);
        account.uuid = Some("shared-uuid".to_string());
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": { "utilization": 10.0 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let mut config = Config::default();
        for name in ["primary", "secondary"] {
            config.providers.insert(
                name.to_string(),
                ProviderConfig {
                    kind: ProviderKind::Anthropic,
                    base_url: server.uri(),
                    auth: AuthMode::ClaudeOauth,
                    api_key_env: None,
                    api_key_header: ApiKeyHeader::Bearer,
                    effort: None,
                    service_tier: None,
                    count_tokens: CountTokens::default(),
                    accounts: vec![account.clone()],
                    account_scope: Vec::new(),
                    websocket: false,
                    tool_search: None,
                    request_compression: true,
                    retry: Default::default(),
                    workspace_roots: Vec::new(),
                    sandbox: true,
                },
            );
        }
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        poll_all(&state).await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let primary =
            state
                .accounts
                .snapshot("primary", std::slice::from_ref(&account), None, None);
        let secondary =
            state
                .accounts
                .snapshot("secondary", std::slice::from_ref(&account), None, None);
        assert_eq!(primary[0].utilization_5h, Some(0.10));
        assert_eq!(secondary[0].utilization_5h, Some(0.10));
        let _ = std::fs::remove_file(creds);
    }

    #[tokio::test]
    async fn non_refreshable_alias_does_not_block_refreshable_alias() {
        use crate::config::{ApiKeyHeader, Config, CountTokens, ProviderConfig, ProviderKind};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let creds = write_temp(
            "refreshable-alias",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        let static_alias = AccountConfig {
            name: "static-alias".to_string(),
            token_env: Some("SHUNT_TEST_STATIC_ALIAS".to_string()),
            uuid: Some("shared-uuid".to_string()),
            ..Default::default()
        };
        let mut refreshable_alias = account_with_credentials(&creds);
        refreshable_alias.name = "refreshable-alias".to_string();
        refreshable_alias.uuid = Some("shared-uuid".to_string());

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": { "utilization": 42.0 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = |account| ProviderConfig {
            kind: ProviderKind::Anthropic,
            base_url: server.uri(),
            auth: AuthMode::ClaudeOauth,
            api_key_env: None,
            api_key_header: ApiKeyHeader::Bearer,
            effort: None,
            service_tier: None,
            count_tokens: CountTokens::default(),
            accounts: vec![account],
            account_scope: Vec::new(),
            websocket: false,
            tool_search: None,
            request_compression: true,
            retry: Default::default(),
            workspace_roots: Vec::new(),
            sandbox: true,
        };
        let mut config = Config::default();
        config
            .providers
            .insert("a-static".to_string(), provider(static_alias.clone()));
        config.providers.insert(
            "b-refreshable".to_string(),
            provider(refreshable_alias.clone()),
        );
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        poll_all(&state).await;

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "the refreshable alias must be polled");
        let snapshot = state.accounts.snapshot(
            "b-refreshable",
            std::slice::from_ref(&refreshable_alias),
            None,
            None,
        );
        assert_eq!(snapshot[0].utilization_5h, Some(0.42));

        let _ = std::fs::remove_file(creds);
    }

    #[tokio::test]
    async fn poll_account_skips_non_refreshable_without_fetching() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Setup-token file: the poller must not call the usage endpoint at all.
        let creds = write_temp(
            "skip",
            r#"{"claudeAiOauth":{"accessToken":"a","expiresAt":4000000000000,"shuntCredentialKind":"setup_token"}}"#,
        );
        let account = account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        poll_account(
            &reqwest::Client::new(),
            &pool,
            "anthropic",
            &server.uri(),
            &account,
        )
        .await;

        let snap = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert!(!snap[0].has_state, "a skipped account records no state");

        let _ = std::fs::remove_file(creds);
    }

    #[tokio::test]
    async fn spawn_usage_poller_is_noop_without_pool_config() {
        // The default config has no `[server.pool] usage_refresh_seconds`, so the
        // spawn helper must take its guard path and start no background task.
        let state =
            AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();
        assert!(state.config.server.pool.is_none());
        spawn_usage_poller(state);
    }

    fn codex_account_with_credentials(path: &Path) -> AccountConfig {
        AccountConfig {
            name: "codex-acct".to_string(),
            credentials: Some(path.to_string_lossy().into_owned()),
            token_env: None,
            uuid: None,
            ..Default::default()
        }
    }

    /// Build a fake ChatGPT access token carrying the `chatgpt_account_id`
    /// claim `codex::auth::jwt_account_id` reads. Mirrors the same-named
    /// helper in `auth::mod::tests`.
    fn chatgpt_access_token(account_id: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let payload = serde_json::json!({
            "exp": 2_000_000_000,
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
        });
        format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    /// Build a `codex login`-shaped credential file body. `refresh_token: None`
    /// omits the field entirely, producing a non-refreshable credential.
    fn codex_credential_json(
        access_token: &str,
        refresh_token: Option<&str>,
        account_id: &str,
    ) -> String {
        let mut tokens = serde_json::json!({
            "access_token": access_token,
            "account_id": account_id,
        });
        if let Some(refresh_token) = refresh_token {
            tokens["refresh_token"] = serde_json::json!(refresh_token);
        }
        serde_json::json!({
            "auth_mode": "ChatGPT",
            "tokens": tokens,
        })
        .to_string()
    }

    /// A `Config` whose built-in `codex` provider (already `chatgpt_oauth`, see
    /// `Config::default`) points at `base_url` and carries `account` as its
    /// sole configured account.
    fn codex_backend_config(base_url: &str, account: AccountConfig) -> Config {
        let mut config = Config::default();
        let provider = config
            .providers
            .get_mut("codex")
            .expect("the default config always has a built-in codex provider");
        provider.base_url = base_url.to_string();
        provider.accounts = vec![account];
        config
    }

    /// Test 24: `poll_codex_account` applies both windows from a `wham/usage`
    /// response and sends the ChatGPT bearer, the three CLI identity headers,
    /// and `chatgpt-account-id` on the request.
    #[tokio::test]
    async fn codex_poll_applies_wham_snapshot() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let access_token = chatgpt_access_token("acct-apply");
        let creds = write_temp(
            "codex-apply",
            &codex_credential_json(&access_token, Some("refresh"), "acct-apply"),
        );
        let account = codex_account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wham/usage"))
            .and(header("authorization", format!("Bearer {access_token}")))
            .and(header("chatgpt-account-id", "acct-apply"))
            .and(header("originator", "codex_cli_rs"))
            .and(header(
                "user-agent",
                crate::adapters::responses::request::CODEX_USER_AGENT,
            ))
            .and(header(
                "version",
                crate::adapters::responses::request::CODEX_CLIENT_VERSION,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "rate_limit": {
                    "primary_window": { "used_percent": 18.0 },
                    "secondary_window": { "used_percent": 64.0 }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        let config = codex_backend_config(&server.uri(), account.clone());
        let applied = poll_codex_account(
            &reqwest::Client::new(),
            &pool,
            &config,
            "codex",
            &server.uri(),
            &account,
        )
        .await;
        assert!(applied, "a valid snapshot must be applied");

        let snap = pool.snapshot("codex", std::slice::from_ref(&account), None, None);
        assert!(snap[0].has_state);
        assert_eq!(snap[0].utilization_5h, Some(0.18));
        assert_eq!(snap[0].utilization_7d, Some(0.64));

        let _ = std::fs::remove_file(creds);
    }

    /// Test 25: alternate window key names (`five_hour_limit`/`weekly_limit`
    /// under the plural `rate_limits` container) are tolerated.
    #[tokio::test]
    async fn codex_poll_tolerates_alternate_field_names() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let access_token = chatgpt_access_token("acct-alt");
        let creds = write_temp(
            "codex-alt",
            &codex_credential_json(&access_token, Some("refresh"), "acct-alt"),
        );
        let account = codex_account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wham/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "rate_limits": {
                    "five_hour_limit": { "used_percent": 22.0 },
                    "weekly_limit": { "used_percent": 91.0 }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        let config = codex_backend_config(&server.uri(), account.clone());
        let applied = poll_codex_account(
            &reqwest::Client::new(),
            &pool,
            &config,
            "codex",
            &server.uri(),
            &account,
        )
        .await;
        assert!(applied);

        let snap = pool.snapshot("codex", std::slice::from_ref(&account), None, None);
        assert_eq!(snap[0].utilization_5h, Some(0.22));
        assert_eq!(snap[0].utilization_7d, Some(0.91));

        let _ = std::fs::remove_file(creds);
    }

    /// Test 26: `resets_at` is accepted as either a Unix epoch integer or an
    /// RFC 3339 string, per window, in the same response.
    #[tokio::test]
    async fn codex_poll_accepts_epoch_and_rfc3339_resets() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let access_token = chatgpt_access_token("acct-resets");
        let creds = write_temp(
            "codex-resets",
            &codex_credential_json(&access_token, Some("refresh"), "acct-resets"),
        );
        let account = codex_account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wham/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "primary_window": { "used_percent": 5.0, "resets_at": 1_800_000_000u64 },
                "secondary_window": { "used_percent": 9.0, "resets_at": "2026-09-01T00:00:00Z" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        let config = codex_backend_config(&server.uri(), account.clone());
        let applied = poll_codex_account(
            &reqwest::Client::new(),
            &pool,
            &config,
            "codex",
            &server.uri(),
            &account,
        )
        .await;
        assert!(applied);

        let snap = pool.snapshot("codex", std::slice::from_ref(&account), None, None);
        assert_eq!(snap[0].reset_5h, Some(1_800_000_000));
        assert!(snap[0].reset_7d.is_some());

        let _ = std::fs::remove_file(creds);
    }

    /// Test 27: a `wham/usage` poll whose primary window carries no
    /// `resets_at` must not erase a reset previously recorded from proxied
    /// `x-codex-*` response headers (the near-quota persistence bug this PR
    /// stack fixes) — it must still apply the fresh utilization.
    #[tokio::test]
    async fn reset_less_wham_poll_does_not_erase_header_reset() {
        use reqwest::header::{HeaderMap, HeaderValue};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let access_token = chatgpt_access_token("acct-reset-preserve");
        let creds = write_temp(
            "codex-reset-preserve",
            &codex_credential_json(&access_token, Some("refresh"), "acct-reset-preserve"),
        );
        let account = codex_account_with_credentials(&creds);

        let pool = AccountPool::new();

        // Stage 1: a proxied response's x-codex-* headers record a future
        // reset, exactly as `note_codex_quota` would from live traffic.
        let future_reset = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-primary-window-minutes",
            HeaderValue::from_static("300"),
        );
        headers.insert(
            "x-codex-primary-used-percent",
            HeaderValue::from_static("10"),
        );
        headers.insert(
            "x-codex-primary-reset-at",
            HeaderValue::from_str(&future_reset.to_string()).unwrap(),
        );
        pool.note_codex_quota("codex", &account, &headers);
        let before = pool.snapshot("codex", std::slice::from_ref(&account), None, None);
        assert_eq!(before[0].reset_5h, Some(future_reset));

        // Stage 2: a wham/usage poll with no `resets_at` on its primary window
        // must update utilization but leave that header-derived reset alone.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wham/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "primary_window": { "used_percent": 55.0 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let config = codex_backend_config(&server.uri(), account.clone());
        let applied = poll_codex_account(
            &reqwest::Client::new(),
            &pool,
            &config,
            "codex",
            &server.uri(),
            &account,
        )
        .await;
        assert!(applied);

        let after = pool.snapshot("codex", std::slice::from_ref(&account), None, None);
        assert_eq!(after[0].utilization_5h, Some(0.55));
        assert_eq!(
            after[0].reset_5h,
            Some(future_reset),
            "a reset-less poll must not erase the header-derived reset"
        );

        let _ = std::fs::remove_file(creds);
    }

    /// Test 28: a 500 response and an unrecognizable 200 body both leave the
    /// account with no recorded state — the poller degrades quietly rather
    /// than marking the account unhealthy on a parse failure.
    #[tokio::test]
    async fn codex_poll_records_no_state_on_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let access_token = chatgpt_access_token("acct-error");

        // Sub-case 1: the endpoint returns 500.
        let creds_500 = write_temp(
            "codex-error-500",
            &codex_credential_json(&access_token, Some("refresh"), "acct-error"),
        );
        let account_500 = codex_account_with_credentials(&creds_500);
        let server_500 = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(1)
            .mount(&server_500)
            .await;
        let pool = AccountPool::new();
        let config_500 = codex_backend_config(&server_500.uri(), account_500.clone());
        let applied_500 = poll_codex_account(
            &reqwest::Client::new(),
            &pool,
            &config_500,
            "codex",
            &server_500.uri(),
            &account_500,
        )
        .await;
        assert!(!applied_500);
        let snap_500 = pool.snapshot("codex", std::slice::from_ref(&account_500), None, None);
        assert!(!snap_500[0].has_state, "a 500 must not record state");

        // Sub-case 2: the endpoint returns 200 with an unrecognizable body.
        let creds_bad = write_temp(
            "codex-error-badjson",
            &codex_credential_json(&access_token, Some("refresh"), "acct-error"),
        );
        let account_bad = codex_account_with_credentials(&creds_bad);
        let server_bad = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "totally_unexpected": true })),
            )
            .expect(1)
            .mount(&server_bad)
            .await;
        let pool_bad = AccountPool::new();
        let config_bad = codex_backend_config(&server_bad.uri(), account_bad.clone());
        let applied_bad = poll_codex_account(
            &reqwest::Client::new(),
            &pool_bad,
            &config_bad,
            "codex",
            &server_bad.uri(),
            &account_bad,
        )
        .await;
        assert!(!applied_bad);
        let snap_bad = pool_bad.snapshot("codex", std::slice::from_ref(&account_bad), None, None);
        assert!(
            !snap_bad[0].has_state,
            "an unrecognizable body must not record state"
        );

        let _ = std::fs::remove_file(creds_500);
        let _ = std::fs::remove_file(creds_bad);
    }

    /// X1 (verification-FAIL fix): a 200 response that parses successfully
    /// but carries no recognizable window at all (e.g. `{"primary_window":
    /// {}}`, a legitimate response for a brand-new account with no
    /// consumption yet) must not be applied to the pool — `note_usage` would
    /// mark the account observed and let `poll_all`'s physical-account dedup
    /// permanently skip its other aliases. This is the `Ok` path, distinct
    /// from `codex_poll_records_no_state_on_error`'s sub-case 2 (an
    /// unrecognizable body, which the parser itself rejects with `Err`).
    #[tokio::test]
    async fn codex_poll_skips_note_usage_on_empty_snapshot() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let access_token = chatgpt_access_token("acct-empty");
        let creds = write_temp(
            "codex-empty",
            &codex_credential_json(&access_token, Some("refresh"), "acct-empty"),
        );
        let account = codex_account_with_credentials(&creds);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "primary_window": {} })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let pool = AccountPool::new();
        let config = codex_backend_config(&server.uri(), account.clone());
        let applied = poll_codex_account(
            &reqwest::Client::new(),
            &pool,
            &config,
            "codex",
            &server.uri(),
            &account,
        )
        .await;
        assert!(!applied, "an empty snapshot must not count as applied");
        let snap = pool.snapshot("codex", std::slice::from_ref(&account), None, None);
        assert!(
            !snap[0].has_state,
            "an empty snapshot must not mark the account observed"
        );

        let _ = std::fs::remove_file(creds);
    }

    /// X1/N5 boundary: a response where one window parses and the other is
    /// structurally malformed is *not* empty — it must still apply and
    /// report `true`. This is the boundary the whole is_empty() guard rests
    /// on: it must trigger only when *every* window is `None`, never when
    /// just one is.
    #[tokio::test]
    async fn codex_poll_applies_partial_snapshot_when_one_window_is_malformed() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let access_token = chatgpt_access_token("acct-partial");
        let creds = write_temp(
            "codex-partial",
            &codex_credential_json(&access_token, Some("refresh"), "acct-partial"),
        );
        let account = codex_account_with_credentials(&creds);
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                // primary_window parses; secondary_window's used_percent is
                // out of range (>100) so parse_window skips it alone.
                "primary_window": { "used_percent": 12.0 },
                "secondary_window": { "used_percent": 150.0 }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let pool = AccountPool::new();
        let config = codex_backend_config(&server.uri(), account.clone());
        let applied = poll_codex_account(
            &reqwest::Client::new(),
            &pool,
            &config,
            "codex",
            &server.uri(),
            &account,
        )
        .await;
        assert!(
            applied,
            "a snapshot with at least one valid window must still apply"
        );
        let snap = pool.snapshot("codex", std::slice::from_ref(&account), None, None);
        assert!(snap[0].has_state);
        assert_eq!(snap[0].utilization_5h, Some(0.12));
        assert_eq!(snap[0].utilization_7d, None);

        let _ = std::fs::remove_file(creds);
    }

    /// Test 29: a credential file with no `tokens.refresh_token` (not an
    /// imported login) is skipped — zero HTTP calls.
    #[tokio::test]
    async fn codex_poll_skips_non_refreshable() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let access_token = chatgpt_access_token("acct-static");
        let creds = write_temp(
            "codex-static",
            &codex_credential_json(&access_token, None, "acct-static"),
        );
        let account = codex_account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let pool = AccountPool::new();
        let config = codex_backend_config(&server.uri(), account.clone());
        let applied = poll_codex_account(
            &reqwest::Client::new(),
            &pool,
            &config,
            "codex",
            &server.uri(),
            &account,
        )
        .await;
        assert!(!applied, "a non-refreshable account must be skipped");

        let _ = std::fs::remove_file(creds);
    }

    /// Test 30: a `chatgpt_oauth` credential polled under a provider name that
    /// does not resolve to a ChatGPT-backend provider in `Config` (here, a name
    /// absent from `config.providers` entirely, so `is_chatgpt_backend` takes
    /// its `unwrap_or(false)` path) must not be polled — zero HTTP calls, even
    /// though the account and its real backing provider are genuinely
    /// ChatGPT-OAuth. Mirrors the defense-in-depth check
    /// `adapters::responses::request::responses_url` runs before routing to
    /// `/codex/responses`.
    #[tokio::test]
    async fn codex_poll_gated_on_chatgpt_backend() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let access_token = chatgpt_access_token("acct-gate");
        let creds = write_temp(
            "codex-gate",
            &codex_credential_json(&access_token, Some("refresh"), "acct-gate"),
        );
        let account = codex_account_with_credentials(&creds);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let config = codex_backend_config(&server.uri(), account.clone());
        let pool = AccountPool::new();
        let applied = poll_codex_account(
            &reqwest::Client::new(),
            &pool,
            &config,
            "not-configured",
            &server.uri(),
            &account,
        )
        .await;
        assert!(!applied, "a gated poll must not apply a snapshot");

        let _ = std::fs::remove_file(creds);
    }

    /// Test 31: `poll_all` polls both families in one pass — a `claude_oauth`
    /// provider and the built-in ChatGPT-backend `codex` provider both get
    /// their usage applied, and neither family's dispatch interferes with the
    /// other's.
    #[tokio::test]
    async fn poll_all_polls_both_families() {
        use crate::config::{ApiKeyHeader, CountTokens, ProviderConfig, ProviderKind};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let claude_creds = write_temp(
            "both-families-claude",
            r#"{"claudeAiOauth":{"accessToken":"live-token","refreshToken":"r","expiresAt":4000000000000}}"#,
        );
        let claude_account = account_with_credentials(&claude_creds);
        let claude_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": { "utilization": 15.0 }
            })))
            .expect(1)
            .mount(&claude_server)
            .await;

        let access_token = chatgpt_access_token("acct-both");
        let codex_creds = write_temp(
            "both-families-codex",
            &codex_credential_json(&access_token, Some("refresh"), "acct-both"),
        );
        // `resolve_pool_accounts` resolves this account's uuid inline from its
        // credential file's `tokens.account_id` (`acct-both`, matching the JWT
        // claim above) before `poll_all` ever calls `note_usage`. Setting it
        // here up front — mirroring `poll_all_deduplicates_physical_accounts_across_upstreams`'s
        // `shuntAccountUuid` pattern on the Claude side — keeps this test's own
        // post-poll snapshot query keyed identically to what `poll_all` wrote,
        // instead of racing a since-enriched identity under a stale key.
        let mut codex_account = codex_account_with_credentials(&codex_creds);
        codex_account.uuid = Some("acct-both".to_string());
        let codex_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wham/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "primary_window": { "used_percent": 66.0 }
            })))
            .expect(1)
            .mount(&codex_server)
            .await;

        let mut config = Config::default();
        config.providers.insert(
            "claude-pool".to_string(),
            ProviderConfig {
                kind: ProviderKind::Anthropic,
                base_url: claude_server.uri(),
                auth: AuthMode::ClaudeOauth,
                api_key_env: None,
                api_key_header: ApiKeyHeader::Bearer,
                effort: None,
                service_tier: None,
                count_tokens: CountTokens::default(),
                accounts: vec![claude_account.clone()],
                account_scope: Vec::new(),
                websocket: false,
                tool_search: None,
                request_compression: true,
                retry: Default::default(),
                workspace_roots: Vec::new(),
                sandbox: true,
            },
        );
        {
            let codex_provider = config
                .providers
                .get_mut("codex")
                .expect("default config has a codex provider");
            codex_provider.base_url = codex_server.uri();
            codex_provider.accounts = vec![codex_account.clone()];
        }
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        poll_all(&state).await;

        let claude_snap = state.accounts.snapshot(
            "claude-pool",
            std::slice::from_ref(&claude_account),
            None,
            None,
        );
        assert!(claude_snap[0].has_state);
        assert_eq!(claude_snap[0].utilization_5h, Some(0.15));

        let codex_snap =
            state
                .accounts
                .snapshot("codex", std::slice::from_ref(&codex_account), None, None);
        assert!(codex_snap[0].has_state);
        assert_eq!(codex_snap[0].utilization_5h, Some(0.66));

        let _ = std::fs::remove_file(claude_creds);
        let _ = std::fs::remove_file(codex_creds);
    }
}
