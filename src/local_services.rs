//! What the widget reads from the local NOBRAIN engine, its one source for
//! everything but usage quotas:
//! - the health of the OmniRoute gateway, the NOBRAIN engine and the
//!   claude-mem worker, published as `services.<name>.status`, from the
//!   engine's `sante` action. NOBRAIN is up when the engine answers;
//! - the open Claude Code consoles, from its `jour` action (see
//!   `claude_consoles` for what the widget adds to each).
//!
//! Both actions only read: the widget never writes to NOBRAIN. With the engine
//! stopped, NOBRAIN reads down, the other services unknown, and no console is
//! shown.
//!
//! `sante` takes seconds (the engine checks git and gh too), so nothing runs
//! on the window thread: a background thread asks on its own schedule and the
//! widget only reads the last answers. The thread starts on the first
//! `latest()` and stays idle whenever nothing has called it recently, so a
//! theme without these rows, or the rows switched off, costs nothing.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The IPv4 literal rather than `localhost`: the server listens on IPv4 only,
/// and resolving `localhost` tries `::1` first, which costs more than a second.
const ACTION_URL: &str = "http://127.0.0.1:4310/api/action/";

pub static OMNIROUTE: LocalService = LocalService(|reading| reading.omniroute);
pub static NOBRAIN: LocalService = LocalService(|reading| reading.nobrain);
pub static CLAUDE_MEM: LocalService = LocalService(|reading| reading.claude_mem);

/// Console slots a theme lays out; consoles past the last one are not shown.
pub const MAX_CONSOLES: usize = 6;

/// `jour` is cheap: consoles follow every 15 s.
const POLL_INTERVAL: Duration = Duration::from_secs(15);
/// `sante` is not: once a minute, every fourth round, while the engine is up.
const SANTE_EVERY: u32 = 4;
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

/// The `resultat` of a successful action, from its HTTP status and body.
fn result_of(code: u16, body: &str) -> Option<serde_json::Value> {
    let mut value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    (code == 200 && value["ok"] == true).then(|| value["resultat"].take())
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
    let result = result_of(code, body);
    let Some(controls) = result.as_ref().and_then(|r| r["controles"].as_array()) else {
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

/// A console NOBRAIN lists as open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenSession {
    /// Claude Code's session id.
    pub id: String,
    /// The project of the session, `-` for one without a project yet.
    pub projet: String,
}

/// The open consoles in one `jour` answer, oldest first as the engine lists
/// them.
pub fn consoles_from(response: Option<(u16, &str)>) -> Vec<OpenSession> {
    let result = response.and_then(|(code, body)| result_of(code, body));
    let Some(list) = result.as_ref().and_then(|r| r["sessions"]["liste"].as_array()) else {
        return Vec::new();
    };
    list.iter()
        .take(MAX_CONSOLES)
        .map(|session| OpenSession {
            id: session["id"].as_str().unwrap_or_default().to_owned(),
            projet: session["projet"].as_str().unwrap_or("-").to_owned(),
        })
        .collect()
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

/// The open consoles from the last `jour` answer. Does not start or wake the
/// reading thread: whoever shows them calls `latest()` for that.
pub fn consoles() -> Vec<OpenSession> {
    SHARED.get().map_or_else(Vec::new, |shared| {
        shared
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .consoles
            .clone()
    })
}

struct Shared {
    latest: Reading,
    consoles: Vec<OpenSession>,
    last_read: Instant,
}

static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();

fn shared() -> &'static Mutex<Shared> {
    SHARED.get_or_init(|| {
        std::thread::Builder::new()
            .name("nobrain-reader".into())
            .spawn(poll_loop)
            .ok();
        Mutex::new(Shared {
            latest: Reading::default(),
            consoles: Vec::new(),
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
    let mut round: u32 = 0;
    loop {
        let (wanted, engine_up) = {
            let guard = shared().lock().unwrap_or_else(|error| error.into_inner());
            (
                guard.last_read.elapsed() <= IDLE_AFTER,
                guard.latest.nobrain.status == ServiceStatus::Up,
            )
        };
        if wanted {
            let jour = ask(&agent, "jour");
            let consoles = consoles_from(jour.as_ref().map(|(c, b)| (*c, b.as_str())));
            // Every round while the engine is not up, so a restart shows at once.
            let reading = (round.is_multiple_of(SANTE_EVERY) || !engine_up).then(|| {
                let sante = ask(&agent, "sante");
                reading_from(sante.as_ref().map(|(c, b)| (*c, b.as_str())))
            });
            let mut guard = shared().lock().unwrap_or_else(|error| error.into_inner());
            guard.consoles = consoles;
            if let Some(reading) = reading {
                guard.latest = reading;
            } else if jour.is_none() {
                // Stopped between two `sante` calls.
                guard.latest = reading_from(None);
            }
            round = round.wrapping_add(1);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Calls a read-only action; `None` when the engine did not answer.
fn ask(agent: &ureq::Agent, action: &str) -> Option<(u16, String)> {
    let mut response = agent
        .post(format!("{ACTION_URL}{action}"))
        .header("X-NOBRAIN", "1")
        .header("Content-Type", "application/json")
        .send("{}")
        .ok()?;
    let code = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .unwrap_or_default();
    Some((code, body))
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
    fn a_stopped_engine_reads_down_and_shows_no_console() {
        let reading = reading_from(None);
        assert_eq!(reading.nobrain.status, ServiceStatus::Down);
        assert_eq!(reading.omniroute.status, ServiceStatus::Unknown);
        assert_eq!(reading.claude_mem.status, ServiceStatus::Unknown);
        assert!(consoles_from(None).is_empty());
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
            assert!(consoles_from(Some((code, body))).is_empty());
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
    fn consoles_are_the_engine_open_sessions_up_to_the_slot_count() {
        let session = |projet: &str| format!(r#"{{"id":"s1","projet":{projet},"debut":"t"}}"#);
        let mut list: Vec<String> = vec![session(r#""coeur-fou""#), session("null")];
        list.extend((0..6).map(|_| session(r#""nobrain""#)));
        let body = format!(
            r#"{{"ok":true,"resultat":{{"sessions":{{"ouvertes":8,"liste":[{}]}}}}}}"#,
            list.join(",")
        );
        let consoles = consoles_from(Some((200, &body)));
        assert_eq!(consoles.len(), MAX_CONSOLES);
        let projets: Vec<&str> = consoles.iter().map(|c| c.projet.as_str()).collect();
        assert_eq!(projets[..3], ["coeur-fou", "-", "nobrain"]);
        assert_eq!(consoles[0].id, "s1");
    }

    #[test]
    fn an_engine_without_the_list_shows_no_console() {
        let body = r#"{"ok":true,"resultat":{"sessions":{"ouvertes":6,"sansCloture":[]}}}"#;
        assert!(consoles_from(Some((200, body))).is_empty());
    }

    #[test]
    fn statuses_publish_stable_codes() {
        assert_eq!(ServiceStatus::Unknown.code(), 0);
        assert_eq!(ServiceStatus::Up.code(), 1);
        assert_eq!(ServiceStatus::Degraded.code(), 2);
        assert_eq!(ServiceStatus::Down.code(), 3);
    }
}
