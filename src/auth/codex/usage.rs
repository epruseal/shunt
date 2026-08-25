//! ChatGPT/Codex `wham/usage` client.
//!
//! `GET {chatgpt_base_url}/wham/usage` reports a ChatGPT/Codex subscription
//! account's rate-limit utilization for the same 5-hour and weekly windows
//! shunt tracks from the proxied `x-codex-*` response headers (see
//! [`crate::accounts::AccountPool::note_codex_quota`]). Those headers only
//! update on traffic that actually flowed through shunt, so an account the
//! pool has excluded for being near quota never gets a fresh observation —
//! even after the upstream window has long since reset. Codex CLI itself
//! polls this endpoint every 60 seconds for the same reason: to know an
//! account's headroom without waiting on live traffic. The poller
//! ([`crate::usage_poll`]) reuses it to reconcile header-derived state the
//! same way [`crate::auth::claude::usage`] does for Claude.
//!
//! **This is an unofficial, private API.** It is not part of any published
//! OpenAI/ChatGPT contract — the evidence for its shape is the Codex CLI's
//! own observed polling behavior, not documentation. Its schema can drift or
//! disappear without notice, so parsing here is deliberately lenient and
//! fail-soft end to end: an unrecognized container or a response with no
//! identifiable window returns `Err` (and the poller only logs it at debug),
//! a single bad window is skipped rather than failing the whole response, and
//! no code path here can panic or mark an account unhealthy on a parse
//! failure. Losing this signal degrades the pool back to header-only
//! tracking; it must never take a proxied request down with it.
//!
//! The endpoint authenticates with the same ChatGPT OAuth bearer and CLI
//! identity headers as the Responses API's ChatGPT backend, plus the
//! `chatgpt-account-id` header — see [`fetch_usage`]. Like the Claude usage
//! API, only a refreshable imported login can call it; the poller restricts
//! itself to imported Codex accounts.

use serde_json::Value;

use crate::accounts::{codex_window_bucket, CodexWindow, UsageSnapshot, UsageWindow};
use crate::adapters::responses::request::{CODEX_CLIENT_VERSION, CODEX_USER_AGENT};
use crate::auth::claude::usage::parse_rfc3339_to_epoch_secs;

/// Path appended to a provider's base URL to reach the usage endpoint.
pub const USAGE_PATH: &str = "/wham/usage";

/// Fetch and parse the wham usage snapshot for one ChatGPT/Codex OAuth
/// account. `base_url` is the provider's ChatGPT backend base (e.g.
/// `https://chatgpt.com/backend-api`); `access_token` is a valid refreshable
/// ChatGPT login bearer and `account_id` is that account's ChatGPT account
/// id, exactly as sent on `/codex/responses`
/// (`adapters::responses::request::build_request`).
pub async fn fetch_usage(
    client: &reqwest::Client,
    base_url: &str,
    access_token: &str,
    account_id: &str,
) -> anyhow::Result<UsageSnapshot> {
    let url = format!("{}{USAGE_PATH}", base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .header("authorization", format!("Bearer {access_token}"))
        .header("chatgpt-account-id", account_id)
        .header("originator", "codex_cli_rs")
        .header("user-agent", CODEX_USER_AGENT)
        .header("version", CODEX_CLIENT_VERSION)
        // The shared client carries no default timeout; bound this background poll
        // so a hung connection can never stall the poller task indefinitely.
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail: String = text.chars().take(200).collect();
        anyhow::bail!("wham usage request failed ({status}): {detail}");
    }
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| anyhow::anyhow!("invalid wham usage response: {error}"))?;
    parse_usage(&value)
}

/// Parse the wham usage JSON into a [`UsageSnapshot`]. Lenient about the
/// container and the window key names (private-API schema drift, see the
/// module doc), but returns `Err` when the response carries no recognizable
/// window at all rather than silently reporting an all-empty snapshot — that
/// distinguishes "the account genuinely has no data for this window right
/// now" (a per-window `None`, still `Ok`) from "this response doesn't look
/// like a wham/usage payload" (the whole poll should not be trusted). A
/// window that *is* structurally present but fails to parse (bad percent, for
/// instance) is skipped on its own and does not fail the response — the
/// counterpart window can still apply.
fn parse_usage(value: &serde_json::Value) -> anyhow::Result<UsageSnapshot> {
    let container = value
        .get("rate_limit")
        .or_else(|| value.get("rate_limits"))
        .unwrap_or(value);
    if !container.is_object() {
        anyhow::bail!("wham usage response is not a JSON object");
    }
    let primary = container
        .get("primary_window")
        .or_else(|| container.get("five_hour_limit"));
    let secondary = container
        .get("secondary_window")
        .or_else(|| container.get("weekly_limit"));
    if primary.is_none() && secondary.is_none() {
        anyhow::bail!("wham usage response carries no recognizable rate-limit window");
    }

    let mut five_hour = None;
    let mut seven_day = None;
    for (window, fallback) in [
        (primary, CodexWindow::FiveHour),
        (secondary, CodexWindow::Weekly),
    ] {
        let Some(window) = window else { continue };
        let Some(parsed) = parse_window(window) else {
            continue;
        };
        // `window_minutes`, when present, decides the bucket by duration —
        // mirrors `note_codex_quota`'s header parser, which does not trust the
        // primary/secondary position either. Absent or unrecognized, the
        // window keeps the lane it was found under.
        let bucket = window
            .get("window_minutes")
            .and_then(Value::as_i64)
            .and_then(codex_window_bucket)
            .unwrap_or(fallback);
        match bucket {
            CodexWindow::FiveHour => five_hour = Some(parsed),
            CodexWindow::Weekly => seven_day = Some(parsed),
        }
    }

    // The Fable-scoped weekly bucket (`7d_oi`) has no wham/usage equivalent:
    // that limit is an Anthropic/Claude concept the ChatGPT backend does not
    // report. Always `None` here, same as every other window this snapshot's
    // consumer (`AccountPool::note_usage`) leaves untouched when omitted.
    Ok(UsageSnapshot {
        five_hour,
        seven_day,
        seven_day_oi: None,
    })
}

/// Parse one window object: `{ "used_percent": <0-100>, "resets_at": ...,
/// "window_minutes": ... }`. `None` on any problem with this window alone
/// (missing/non-finite/out-of-range percent) — the caller skips it and still
/// applies whatever else the response reported.
fn parse_window(value: &serde_json::Value) -> Option<UsageWindow> {
    let percent = value.get("used_percent").and_then(Value::as_f64)?;
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return None;
    }
    Some(UsageWindow {
        utilization: percent / 100.0,
        resets_at: parse_resets_at(value.get("resets_at")),
    })
}

/// `resets_at` on this endpoint has been observed as either a Unix epoch
/// integer or an RFC 3339 string; accept both rather than assuming one shape.
fn parse_resets_at(value: Option<&serde_json::Value>) -> Option<u64> {
    let value = value?;
    if let Some(epoch) = value.as_u64() {
        return Some(epoch);
    }
    value.as_str().and_then(parse_rfc3339_to_epoch_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 32 (parser unit, part 1): `window_minutes` decides the bucket by
    /// duration even when it disagrees with the key the window was found
    /// under — mirrors `note_codex_quota`'s header parser, which the wham
    /// parser must not drift from.
    #[test]
    fn window_minutes_decides_the_bucket_over_key_position() {
        // `primary_window` (the 5h lane's key), but `window_minutes` says
        // weekly — the bucket follows the duration, landing in `seven_day`.
        let value = serde_json::json!({
            "primary_window": { "used_percent": 40.0, "window_minutes": 10_080 },
        });
        let snapshot = parse_usage(&value).expect("recognizable window");
        assert!(snapshot.five_hour.is_none());
        let seven_day = snapshot.seven_day.expect("window_minutes routed to weekly");
        assert!((seven_day.utilization - 0.40).abs() < 1e-9);
    }

    /// Test 32 (parser unit, part 2): a `window_minutes` value that matches
    /// neither known duration falls back to the key's own lane rather than
    /// being dropped.
    #[test]
    fn unrecognized_window_minutes_falls_back_to_key_lane() {
        let value = serde_json::json!({
            "secondary_window": { "used_percent": 12.0, "window_minutes": 42 },
        });
        let snapshot = parse_usage(&value).expect("recognizable window");
        assert!(snapshot.five_hour.is_none());
        let seven_day = snapshot
            .seven_day
            .expect("falls back to the secondary lane");
        assert!((seven_day.utilization - 0.12).abs() < 1e-9);
    }

    /// Test 32 (parser unit, part 3): a percent outside `0..=100` (and a
    /// non-finite one) is rejected, skipping just that window.
    #[test]
    fn out_of_range_or_non_finite_percent_is_rejected() {
        for bad in [-1.0, 100.5, f64::NAN, f64::INFINITY] {
            let value = serde_json::json!({
                "primary_window": { "used_percent": bad },
            });
            let snapshot = parse_usage(&value).expect("primary_window key is recognized");
            assert!(
                snapshot.five_hour.is_none(),
                "percent {bad} should have been rejected"
            );
        }
    }

    /// Test 32 (parser unit, part 4): `additional_rate_limits` is ignored —
    /// present or absent changes nothing about the parsed windows.
    #[test]
    fn additional_rate_limits_is_ignored() {
        let value = serde_json::json!({
            "primary_window": { "used_percent": 5.0 },
            "additional_rate_limits": [
                { "kind": "something-shunt-does-not-model", "used_percent": 99.0 }
            ],
        });
        let snapshot = parse_usage(&value).expect("recognizable window");
        let five_hour = snapshot.five_hour.expect("primary_window present");
        assert!((five_hour.utilization - 0.05).abs() < 1e-9);
    }

    /// Test 32 (parser unit, part 5): a response with no recognizable
    /// container or window at all is "unreadable" — `Err`, not an
    /// all-`None` `Ok` snapshot.
    #[test]
    fn unrecognizable_response_shape_is_an_error() {
        for value in [
            serde_json::json!({ "totally_unexpected": true }),
            serde_json::json!([1, 2, 3]),
            serde_json::json!("just a string"),
            serde_json::json!({ "rate_limit": "not an object" }),
        ] {
            assert!(
                parse_usage(&value).is_err(),
                "expected an error for {value:?}"
            );
        }
    }

    #[test]
    fn parses_epoch_and_rfc3339_resets() {
        let value = serde_json::json!({
            "primary_window": { "used_percent": 10.0, "resets_at": 1_700_000_000u64 },
            "secondary_window": {
                "used_percent": 20.0,
                "resets_at": "2026-07-14T17:30:00Z"
            },
        });
        let snapshot = parse_usage(&value).expect("recognizable windows");
        assert_eq!(snapshot.five_hour.unwrap().resets_at, Some(1_700_000_000));
        assert!(snapshot.seven_day.unwrap().resets_at.is_some());
    }

    #[test]
    fn tolerates_alternate_field_names() {
        // `five_hour_limit`/`weekly_limit` instead of
        // `primary_window`/`secondary_window`, nested under `rate_limits`
        // (the plural alternate container name).
        let value = serde_json::json!({
            "rate_limits": {
                "five_hour_limit": { "used_percent": 25.0 },
                "weekly_limit": { "used_percent": 75.0 },
            }
        });
        let snapshot = parse_usage(&value).expect("alternate names recognized");
        assert!((snapshot.five_hour.unwrap().utilization - 0.25).abs() < 1e-9);
        assert!((snapshot.seven_day.unwrap().utilization - 0.75).abs() < 1e-9);
    }

    #[tokio::test]
    async fn fetch_usage_applies_wham_snapshot_and_sends_identity_headers() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Split the scheme from the token so the matcher carries no contiguous
        // `Bearer <token>` literal (a credential-scanner false positive).
        let token = "imported-chatgpt-token";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wham/usage"))
            .and(header("authorization", format!("Bearer {token}")))
            .and(header("chatgpt-account-id", "acct-123"))
            .and(header("originator", "codex_cli_rs"))
            .and(header("user-agent", CODEX_USER_AGENT))
            .and(header("version", CODEX_CLIENT_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 33.0,
                        "window_minutes": 300,
                        "resets_at": "2026-07-14T17:30:00+00:00"
                    },
                    "secondary_window": {
                        "used_percent": 88.0,
                        "window_minutes": 10_080
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let snapshot = fetch_usage(&reqwest::Client::new(), &server.uri(), token, "acct-123")
            .await
            .expect("usage fetch succeeds");
        let five_hour = snapshot.five_hour.expect("primary_window applied");
        assert!((five_hour.utilization - 0.33).abs() < 1e-9);
        assert!(five_hour.resets_at.is_some());
        let seven_day = snapshot.seven_day.expect("secondary_window applied");
        assert!((seven_day.utilization - 0.88).abs() < 1e-9);
        assert!(snapshot.seven_day_oi.is_none());
    }

    #[tokio::test]
    async fn fetch_usage_errors_on_non_success() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let error = fetch_usage(&reqwest::Client::new(), &server.uri(), "bad-token", "acct")
            .await
            .expect_err("a 500 must surface as an error");
        assert!(error.to_string().contains("500"), "got: {error}");
    }

    #[tokio::test]
    async fn fetch_usage_errors_on_unrecognizable_body() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "totally_unexpected": true })),
            )
            .mount(&server)
            .await;

        let error = fetch_usage(&reqwest::Client::new(), &server.uri(), "token", "acct")
            .await
            .expect_err("an unrecognizable 200 body must still surface as an error");
        assert!(error.to_string().contains("wham"), "got: {error}");
    }
}
