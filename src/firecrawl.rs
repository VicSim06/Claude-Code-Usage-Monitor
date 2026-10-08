//! Firecrawl credit usage for the current billing period, published to themes
//! as `services.firecrawl.*` bindings.
//!
//! Same shape as the OmniRoute check: a background thread asks the API on its
//! own schedule and the widget only reads the last answer. The API key comes
//! from the `FIRECRAWL_API_KEY` environment variable, read once at startup;
//! without it nothing is ever requested.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const CREDIT_USAGE_URL: &str = "https://api.firecrawl.dev/v2/team/credit-usage";
const API_KEY_VAR: &str = "FIRECRAWL_API_KEY";
/// Credits move with each scrape, but nobody needs them to the second.
const PROBE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Stop asking once nobody has read a result for this long.
const IDLE_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CreditUsage {
    /// 0 not fetched yet, 1 fetched, 3 the last request failed.
    pub status: u8,
    pub remaining: u32,
    pub plan: u32,
}

impl CreditUsage {
    /// Credits spent this period. Purchased top-ups can push `remaining`
    /// above the plan allowance, so this never goes below zero.
    pub fn used(&self) -> u32 {
        self.plan.saturating_sub(self.remaining)
    }

    pub fn percentage(&self) -> f64 {
        if self.plan > 0 {
            (f64::from(self.used()) / f64::from(self.plan) * 100.0).min(100.0)
        } else {
            0.0
        }
    }
}

/// Parses one answer. `body` is `None` when the request itself failed.
pub fn usage_from(code: u16, body: Option<&str>) -> CreditUsage {
    let failed = CreditUsage {
        status: 3,
        ..Default::default()
    };
    if code != 200 {
        return failed;
    }
    let Some(data) = body
        .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .and_then(|value| value.get("data").cloned())
    else {
        return failed;
    };
    match (
        data.get("remainingCredits").and_then(credits),
        data.get("planCredits").and_then(credits),
    ) {
        (Some(remaining), Some(plan)) => CreditUsage {
            status: 1,
            remaining,
            plan,
        },
        _ => failed,
    }
}

/// Credits as a whole count; the API sends integers, but a float would not hurt.
fn credits(value: &serde_json::Value) -> Option<u32> {
    value
        .as_f64()
        .map(|credits| credits.clamp(0.0, f64::from(u32::MAX)) as u32)
}

fn api_key() -> Option<&'static str> {
    static KEY: OnceLock<Option<String>> = OnceLock::new();
    KEY.get_or_init(|| {
        std::env::var(API_KEY_VAR)
            .ok()
            .map(|key| key.trim().to_owned())
            .filter(|key| !key.is_empty())
    })
    .as_deref()
}

/// Whether a key is set, so themes can hide the row when there is nothing to show.
pub fn configured() -> bool {
    api_key().is_some()
}

struct Shared {
    latest: CreditUsage,
    last_read: Instant,
}

fn shared() -> &'static Mutex<Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| {
        if let Some(key) = api_key() {
            std::thread::Builder::new()
                .name("firecrawl-credits".into())
                .spawn(move || probe_loop(key))
                .ok();
        }
        Mutex::new(Shared {
            latest: CreditUsage::default(),
            last_read: Instant::now(),
        })
    })
}

/// The last known usage. The first call starts the request thread and
/// returns status 0 until its first answer lands.
pub fn latest() -> CreditUsage {
    let mut guard = shared().lock().unwrap_or_else(|error| error.into_inner());
    guard.last_read = Instant::now();
    guard.latest
}

fn probe_loop(key: &'static str) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(PROBE_TIMEOUT))
        .http_status_as_error(false)
        // Same TLS as the usage pollers: only native-tls is compiled in, and
        // ureq's default provider (Rustls) panics on the first https call.
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into();
    loop {
        let wanted = {
            let guard = shared().lock().unwrap_or_else(|error| error.into_inner());
            guard.last_read.elapsed() <= IDLE_AFTER
        };
        if wanted {
            let usage = probe(&agent, key);
            shared()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .latest = usage;
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

fn probe(agent: &ureq::Agent, key: &str) -> CreditUsage {
    match agent
        .get(CREDIT_USAGE_URL)
        .header("Authorization", &format!("Bearer {key}"))
        .call()
    {
        Ok(mut response) => {
            let code = response.status().as_u16();
            let body = response
                .body_mut()
                .with_config()
                .limit(16 * 1024)
                .read_to_string()
                .ok();
            usage_from(code, body.as_deref())
        }
        Err(_) => usage_from(0, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_answer_reads_used_against_the_plan() {
        let usage = usage_from(
            200,
            Some(r#"{"success":true,"data":{"remainingCredits":1760,"planCredits":3000}}"#),
        );
        assert_eq!(usage.status, 1);
        assert_eq!(usage.used(), 1240);
        assert!((usage.percentage() - 41.333).abs() < 0.01);
    }

    #[test]
    fn top_up_credits_never_read_as_negative_usage() {
        let usage = usage_from(
            200,
            Some(r#"{"data":{"remainingCredits":4000,"planCredits":3000}}"#),
        );
        assert_eq!(usage.used(), 0);
        assert_eq!(usage.percentage(), 0.0);
    }

    #[test]
    fn errors_and_unexpected_bodies_read_as_failed() {
        for (code, body) in [
            (401, Some(r#"{"success":false,"error":"Unauthorized"}"#)),
            (200, Some("<html></html>")),
            (200, Some(r#"{"success":true,"data":{}}"#)),
            (0, None),
        ] {
            assert_eq!(usage_from(code, body).status, 3, "{code} {body:?}");
        }
    }
}
