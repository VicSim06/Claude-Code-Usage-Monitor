//! Health of servers running on this machine, published to themes as
//! `services.<name>.*` bindings: the OmniRoute gateway and the DevReport
//! dashboard.
//!
//! The check is an HTTP call, so unlike the machine counters it never runs on
//! the window thread: a background thread per service probes on its own
//! schedule and the widget only reads the last answer. The thread starts on
//! the first read and stays idle whenever nothing has asked for a reading
//! recently, so a theme without the row, or the setting switched off, costs
//! nothing.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// URLs use the IPv4 literal rather than `localhost`: both servers listen on
// IPv4 only, and resolving `localhost` tries `::1` first, which turned a
// 10 ms check into more than a second.
pub static OMNIROUTE: LocalService = LocalService::new(
    "omniroute-health",
    "http://127.0.0.1:20128/api/health",
    omniroute_healthy,
);

/// DevReport has no health route: its dashboard page answering is the check.
/// Port from DevReport's `config.json`.
pub static DEVREPORT: LocalService =
    LocalService::new("devreport-health", "http://127.0.0.1:4310/", |code, _| {
        code == 200
    });

const PROBE_INTERVAL: Duration = Duration::from_secs(15);
/// Short because the server is on this machine: anything slower is down.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// Stop probing once nobody has read a result for this long.
const IDLE_AFTER: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServiceStatus {
    /// Not probed yet this session.
    #[default]
    Unknown,
    Up,
    /// Something answered, but not with a healthy status.
    Degraded,
    /// Nothing listening, or no answer within the timeout.
    Down,
}

impl ServiceStatus {
    /// Published as `services.<name>.status`.
    pub fn code(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Up => 1,
            Self::Degraded => 2,
            Self::Down => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServiceHealth {
    pub status: ServiceStatus,
    /// Round trip of the last answer; 0 when there was none.
    pub latency_ms: u32,
}

fn omniroute_healthy(code: u16, body: &str) -> bool {
    code == 200
        && serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|value| value.get("status")?.as_str().map(|s| s == "ok"))
            .unwrap_or(false)
}

/// Classifies one probe. `response` is the HTTP status and body, or `None`
/// when the connection failed or timed out.
pub fn health_from(
    response: Option<(u16, &str)>,
    latency: Duration,
    healthy: fn(u16, &str) -> bool,
) -> ServiceHealth {
    let Some((code, body)) = response else {
        return ServiceHealth {
            status: ServiceStatus::Down,
            latency_ms: 0,
        };
    };
    let healthy = healthy(code, body);
    ServiceHealth {
        status: if healthy {
            ServiceStatus::Up
        } else {
            ServiceStatus::Degraded
        },
        latency_ms: latency.as_millis().min(u128::from(u32::MAX)) as u32,
    }
}

struct Shared {
    latest: ServiceHealth,
    last_read: Instant,
}

pub struct LocalService {
    thread_name: &'static str,
    pub url: &'static str,
    healthy: fn(u16, &str) -> bool,
    shared: OnceLock<Mutex<Shared>>,
}

impl LocalService {
    const fn new(
        thread_name: &'static str,
        url: &'static str,
        healthy: fn(u16, &str) -> bool,
    ) -> Self {
        Self {
            thread_name,
            url,
            healthy,
            shared: OnceLock::new(),
        }
    }

    fn shared(&'static self) -> &'static Mutex<Shared> {
        self.shared.get_or_init(|| {
            std::thread::Builder::new()
                .name(self.thread_name.into())
                .spawn(|| self.probe_loop())
                .ok();
            Mutex::new(Shared {
                latest: ServiceHealth::default(),
                last_read: Instant::now(),
            })
        })
    }

    /// The last known health. The first call starts the probing thread and
    /// returns `Unknown` until its first answer lands.
    pub fn latest(&'static self) -> ServiceHealth {
        let mut guard = self
            .shared()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        guard.last_read = Instant::now();
        guard.latest
    }

    fn probe_loop(&'static self) {
        // No proxy: a system proxy would turn a local check into a remote one.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(PROBE_TIMEOUT))
            .http_status_as_error(false)
            .proxy(None)
            .build()
            .into();
        loop {
            let wanted = {
                let guard = self
                    .shared()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                guard.last_read.elapsed() <= IDLE_AFTER
            };
            if wanted {
                let health = self.probe(&agent);
                self.shared()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .latest = health;
            }
            std::thread::sleep(PROBE_INTERVAL);
        }
    }

    fn probe(&self, agent: &ureq::Agent) -> ServiceHealth {
        let started = Instant::now();
        let response = agent.get(self.url).call().ok().map(|mut response| {
            let code = response.status().as_u16();
            let body = response
                .body_mut()
                .with_config()
                .limit(4 * 1024)
                .read_to_string()
                .unwrap_or_default();
            (code, body)
        });
        health_from(
            response.as_ref().map(|(code, body)| (*code, body.as_str())),
            started.elapsed(),
            self.healthy,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: Duration = Duration::from_millis(4);

    #[test]
    fn a_healthy_answer_reads_up_with_its_latency() {
        let health = health_from(
            Some((200, r#"{"status":"ok","timestamp":"x"}"#)),
            FAST,
            omniroute_healthy,
        );
        assert_eq!(health.status, ServiceStatus::Up);
        assert_eq!(health.latency_ms, 4);
    }

    #[test]
    fn no_answer_reads_down() {
        let health = health_from(None, Duration::from_secs(2), omniroute_healthy);
        assert_eq!(health.status, ServiceStatus::Down);
        assert_eq!(health.latency_ms, 0);
    }

    #[test]
    fn an_error_status_or_unhealthy_body_reads_degraded() {
        for (code, body) in [
            (500, r#"{"status":"ok"}"#),
            (200, r#"{"status":"starting"}"#),
            (200, "<html>not json</html>"),
        ] {
            let health = health_from(Some((code, body)), FAST, omniroute_healthy);
            assert_eq!(health.status, ServiceStatus::Degraded, "{code} {body}");
        }
    }

    #[test]
    fn devreport_reads_up_on_its_page_and_degraded_on_an_error() {
        let healthy = DEVREPORT.healthy;
        let up = health_from(Some((200, "<!doctype html>")), FAST, healthy);
        assert_eq!(up.status, ServiceStatus::Up);
        let broken = health_from(Some((500, "")), FAST, healthy);
        assert_eq!(broken.status, ServiceStatus::Degraded);
    }

    #[test]
    fn statuses_publish_stable_codes() {
        assert_eq!(ServiceStatus::Unknown.code(), 0);
        assert_eq!(ServiceStatus::Up.code(), 1);
        assert_eq!(ServiceStatus::Degraded.code(), 2);
        assert_eq!(ServiceStatus::Down.code(), 3);
    }
}
