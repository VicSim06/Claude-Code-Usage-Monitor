//! Health of servers running on this machine, published to themes as
//! `services.<name>.status`: the OmniRoute gateway, the NOBRAIN engine and the
//! claude-mem worker.
//!
//! The widget does not probe them itself: NOBRAIN's engine is the one source,
//! and its read-only `sante` action reports OmniRoute and claude-mem. NOBRAIN
//! is up when the engine answers. With the engine stopped, NOBRAIN reads down
//! and the other two unknown.
//!
//! The call takes seconds (the engine checks git and gh too), so it never runs
//! on the window thread: a background thread asks on its own schedule and the
//! widget only reads the last answer. The thread starts on the first read and
//! stays idle whenever nothing has asked for a reading recently, so a theme
//! without these rows, or the rows switched off, costs nothing.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The IPv4 literal rather than `localhost`: the server listens on IPv4 only,
/// and resolving `localhost` tries `::1` first, which costs more than a second.
const SANTE_URL: &str = "http://127.0.0.1:4310/api/action/sante";

pub static OMNIROUTE: LocalService = LocalService(|reading| reading.omniroute);
pub static NOBRAIN: LocalService = LocalService(|reading| reading.nobrain);
pub static CLAUDE_MEM: LocalService = LocalService(|reading| reading.claude_mem);

/// `sante` is not cheap for the engine: once a minute is enough.
const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// `sante` takes about 4 s; anything far slower is a stuck engine.
const POLL_TIMEOUT: Duration = Duration::from_secs(15);
/// Stop asking once nobody has read a result for this long.
const IDLE_AFTER: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServiceStatus {
    /// Not known: not read yet, or the engine cannot tell.
    #[default]
    Unknown,
    Up,
    /// Something answered, but not with a healthy status.
    Degraded,
    /// Not running, or not reachable.
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
}

impl From<ServiceStatus> for ServiceHealth {
    fn from(status: ServiceStatus) -> Self {
        Self { status }
    }
}

/// What one `sante` call says about the three services.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    pub nobrain: ServiceHealth,
    pub omniroute: ServiceHealth,
    pub claude_mem: ServiceHealth,
}

/// Reads one `sante` answer: the HTTP status and body, or `None` when the
/// engine did not answer.
pub fn reading_from(response: Option<(u16, &str)>) -> Reading {
    let Some((code, body)) = response else {
        return Reading {
            nobrain: ServiceStatus::Down.into(),
            ..Reading::default()
        };
    };
    let value = serde_json::from_str::<serde_json::Value>(body).ok();
    let Some(controls) = value
        .as_ref()
        .filter(|value| code == 200 && value["ok"] == true)
        .and_then(|value| value["resultat"]["controles"].as_array())
    else {
        return Reading {
            nobrain: ServiceStatus::Degraded.into(),
            ..Reading::default()
        };
    };
    let service = |id: &str| -> ServiceHealth {
        let state = controls
            .iter()
            .find(|control| control["groupe"] == "services" && control["id"] == id)
            .and_then(|control| control["etat"].as_str());
        match state {
            Some("ok") => ServiceStatus::Up,
            Some("alerte") => ServiceStatus::Down,
            _ => ServiceStatus::Unknown,
        }
        .into()
    };
    Reading {
        nobrain: ServiceStatus::Up.into(),
        omniroute: service("omniroute"),
        claude_mem: service("claude-mem"),
    }
}

/// One of the services, read from the shared engine reading.
pub struct LocalService(fn(&Reading) -> ServiceHealth);

impl LocalService {
    /// The last known health. The first call starts the reading thread and
    /// returns `Unknown` until the engine's first answer lands.
    pub fn latest(&'static self) -> ServiceHealth {
        let mut guard = shared().lock().unwrap_or_else(|error| error.into_inner());
        guard.last_read = Instant::now();
        (self.0)(&guard.latest)
    }
}

struct Shared {
    latest: Reading,
    last_read: Instant,
}

fn shared() -> &'static Mutex<Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| {
        std::thread::Builder::new()
            .name("nobrain-sante".into())
            .spawn(poll_loop)
            .ok();
        Mutex::new(Shared {
            latest: Reading::default(),
            last_read: Instant::now(),
        })
    })
}

fn poll_loop() {
    // No proxy: a system proxy would turn a local call into a remote one.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(POLL_TIMEOUT))
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .into();
    loop {
        let wanted = {
            let guard = shared().lock().unwrap_or_else(|error| error.into_inner());
            guard.last_read.elapsed() <= IDLE_AFTER
        };
        if wanted {
            let reading = ask_sante(&agent);
            shared()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .latest = reading;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Read-only: `sante` changes nothing in NOBRAIN.
fn ask_sante(agent: &ureq::Agent) -> Reading {
    let response = agent
        .post(SANTE_URL)
        .header("X-NOBRAIN", "1")
        .header("Content-Type", "application/json")
        .send("{}")
        .ok()
        .map(|mut response| {
            let code = response.status().as_u16();
            let body = response
                .body_mut()
                .with_config()
                .limit(256 * 1024)
                .read_to_string()
                .unwrap_or_default();
            (code, body)
        });
    reading_from(response.as_ref().map(|(code, body)| (*code, body.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SANTE: &str = r#"{"ok":true,"resultat":{"le":"x","alertes":1,"controles":[
        {"groupe":"moteur","id":"registre","etat":"ok","detail":"valide"},
        {"groupe":"services","id":"omniroute","etat":"alerte","detail":"injoignable"},
        {"groupe":"services","id":"claude-mem","etat":"ok","detail":"worker joignable"},
        {"groupe":"confidentialite","id":"claude-mem","etat":"alerte","detail":"x"}]}}"#;

    #[test]
    fn services_come_from_the_engine_health_report() {
        let reading = reading_from(Some((200, SANTE)));
        assert_eq!(reading.nobrain.status, ServiceStatus::Up);
        assert_eq!(reading.omniroute.status, ServiceStatus::Down);
        assert_eq!(reading.claude_mem.status, ServiceStatus::Up);
    }

    #[test]
    fn a_stopped_engine_reads_down_and_leaves_the_others_unknown() {
        let reading = reading_from(None);
        assert_eq!(reading.nobrain.status, ServiceStatus::Down);
        assert_eq!(reading.omniroute.status, ServiceStatus::Unknown);
        assert_eq!(reading.claude_mem.status, ServiceStatus::Unknown);
    }

    #[test]
    fn an_engine_without_the_action_reads_degraded() {
        // An older server answers 404 `{"erreur":"Introuvable"}`.
        for (code, body) in [
            (404, r#"{"erreur":"Introuvable"}"#),
            (200, r#"{"ok":false,"erreur":"x"}"#),
            (200, "<html>"),
        ] {
            let reading = reading_from(Some((code, body)));
            assert_eq!(reading.nobrain.status, ServiceStatus::Degraded, "{code} {body}");
            assert_eq!(reading.omniroute.status, ServiceStatus::Unknown);
        }
    }

    #[test]
    fn a_service_the_engine_cannot_check_reads_unknown() {
        let body = r#"{"ok":true,"resultat":{"controles":[
            {"groupe":"services","id":"omniroute","etat":"inconnu","detail":""}]}}"#;
        let reading = reading_from(Some((200, body)));
        assert_eq!(reading.omniroute.status, ServiceStatus::Unknown);
        assert_eq!(reading.claude_mem.status, ServiceStatus::Unknown);
    }

    #[test]
    fn statuses_publish_stable_codes() {
        assert_eq!(ServiceStatus::Unknown.code(), 0);
        assert_eq!(ServiceStatus::Up.code(), 1);
        assert_eq!(ServiceStatus::Degraded.code(), 2);
        assert_eq!(ServiceStatus::Down.code(), 3);
    }
}
