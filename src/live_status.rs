//! A snapshot of the live rows (machine load, local services, Firecrawl)
//! written to `live-status.json` beside the usage cache, so tools outside the
//! widget can show the same figures without sampling them a second time.
//!
//! Only the rows the widget itself samples are written: a row switched off,
//! or one the active theme does not read, is `null` in the file.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::app_settings;
use crate::firecrawl::CreditUsage;
use crate::local_services::{ServiceHealth, ServiceStatus};
use crate::system_metrics::SystemMetrics;

/// Readers poll every few seconds; writing more often only wears the disk.
const MIN_WRITE_INTERVAL: Duration = Duration::from_secs(5);

static LAST_WRITE: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LiveStatus {
    pub updated_unix: u64,
    pub system: Option<SystemSnapshot>,
    pub omniroute: Option<ServiceSnapshot>,
    pub claude_mem: Option<ServiceSnapshot>,
    pub devreport: Option<ServiceSnapshot>,
    pub firecrawl: Option<FirecrawlSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SystemSnapshot {
    pub cpu_percent: u8,
    pub cpu_count: u16,
    pub memory_percent: u8,
    pub memory_used_mb: u32,
    pub memory_total_mb: u32,
    pub network_down_kbps: u32,
    pub network_up_kbps: u32,
    /// `""` with no route out, else `Ethernet`, `Wi-Fi` or `Net`.
    pub network_type: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ServiceSnapshot {
    /// `unknown`, `up`, `degraded` or `down`.
    pub status: &'static str,
    pub latency_ms: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FirecrawlSnapshot {
    /// `pending` (not fetched yet), `ok` or `error`.
    pub status: &'static str,
    pub used: u32,
    pub plan: u32,
    pub remaining: u32,
    pub percentage: f64,
}

impl From<SystemMetrics> for SystemSnapshot {
    fn from(m: SystemMetrics) -> Self {
        Self {
            cpu_percent: m.cpu_percent,
            cpu_count: m.cpu_count,
            memory_percent: m.memory_percent,
            memory_used_mb: m.memory_used_mb,
            memory_total_mb: m.memory_total_mb,
            network_down_kbps: m.network_down_kbps,
            network_up_kbps: m.network_up_kbps,
            network_type: m.network_kind.label(),
        }
    }
}

impl From<ServiceHealth> for ServiceSnapshot {
    fn from(h: ServiceHealth) -> Self {
        Self {
            status: match h.status {
                ServiceStatus::Unknown => "unknown",
                ServiceStatus::Up => "up",
                ServiceStatus::Degraded => "degraded",
                ServiceStatus::Down => "down",
            },
            latency_ms: h.latency_ms,
        }
    }
}

impl From<CreditUsage> for FirecrawlSnapshot {
    fn from(c: CreditUsage) -> Self {
        Self {
            status: match c.status {
                1 => "ok",
                3 => "error",
                _ => "pending",
            },
            used: c.used(),
            plan: c.plan,
            remaining: c.remaining,
            percentage: c.percentage(),
        }
    }
}

pub fn live_status_path() -> PathBuf {
    app_settings::app_data_directory().join("live-status.json")
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Whether a write is due, given when the last one happened.
pub fn write_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= MIN_WRITE_INTERVAL)
}

/// Writes the snapshot unless one was written less than five seconds ago.
/// Quiet on failure, like the readings it carries: a reader treats an old
/// file as stale on its own.
pub fn publish(status: &LiveStatus) {
    let now = Instant::now();
    {
        let mut last = LAST_WRITE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !write_due(*last, now) {
            return;
        }
        *last = Some(now);
    }
    let _ = app_settings::write_json_atomic(&live_status_path(), status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_metrics::NetworkKind;

    #[test]
    fn snapshot_serializes_with_readable_statuses() {
        let status = LiveStatus {
            updated_unix: 1_791_577_236,
            system: Some(
                SystemMetrics {
                    cpu_percent: 12,
                    memory_percent: 48,
                    memory_used_mb: 15_600,
                    memory_total_mb: 32_500,
                    cpu_count: 16,
                    network_down_kbps: 820,
                    network_up_kbps: 64,
                    network_kind: NetworkKind::WiFi,
                }
                .into(),
            ),
            omniroute: Some(
                ServiceHealth {
                    status: ServiceStatus::Up,
                    latency_ms: 3,
                }
                .into(),
            ),
            claude_mem: Some(
                ServiceHealth {
                    status: ServiceStatus::Down,
                    latency_ms: 0,
                }
                .into(),
            ),
            devreport: None,
            firecrawl: Some(
                CreditUsage {
                    status: 1,
                    remaining: 400,
                    plan: 500,
                }
                .into(),
            ),
        };
        let json: serde_json::Value = serde_json::to_value(&status).unwrap();
        assert_eq!(json["system"]["network_type"], "Wi-Fi");
        assert_eq!(json["system"]["cpu_percent"], 12);
        assert_eq!(json["omniroute"]["status"], "up");
        assert_eq!(json["claude_mem"]["status"], "down");
        assert!(json["devreport"].is_null());
        assert_eq!(json["firecrawl"]["used"], 100);
        assert_eq!(json["firecrawl"]["percentage"], 20.0);
    }

    #[test]
    fn writes_at_most_every_five_seconds() {
        let start = Instant::now();
        assert!(write_due(None, start));
        assert!(!write_due(Some(start), start + Duration::from_secs(4)));
        assert!(write_due(Some(start), start + Duration::from_secs(5)));
    }
}
