use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct QuotaSnapshot {
    pub observed_at: DateTime<Utc>,
    #[serde(default = "default_true")]
    pub fresh: bool,
    pub windows: Vec<QuotaWindow>,
    /// Number of workers supported by currently eligible queued work, when
    /// the source can observe it. Banked-reset pacing never scales above this
    /// capacity; when absent, it holds the current worker count as the cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligible_backlog_capacity: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<ResetCreditsSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct QuotaWindow {
    pub id: String,
    pub used_fraction: f64,
    pub resets_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_minutes: Option<u64>,
    #[serde(default)]
    pub reached: bool,
}

/// Earned, one-shot rate-limit resets attached to an account.
///
/// `available_count` is authoritative. Providers may omit or cap `credits`, so
/// callers must not infer the total balance from the number of detail rows.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ResetCreditsSnapshot {
    pub available_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<Vec<ResetCredit>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ResetCredit {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_type: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The §22.4 normalized resource contract: structurally parallel to
/// QuotaSnapshot but without reset semantics, for resource-aware
/// multi-host placement rather than quota pacing.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ResourceSnapshot {
    pub observed_at: DateTime<Utc>,
    #[serde(default = "default_true")]
    pub fresh: bool,
    pub host_id: String,
    pub cpu_available_fraction: f64,
    pub mem_available_mb: u64,
    pub mem_total_mb: u64,
}

impl ResourceSnapshot {
    /// Enforces the §22.4 range and consistency rules that serde's own
    /// required-field/type checking cannot: a missing or malformed field
    /// already fails at deserialization, same as QuotaWindow.
    pub fn validate(&self) -> Result<()> {
        if self.host_id.is_empty() {
            bail!("resource snapshot has an empty host_id");
        }
        if !self.cpu_available_fraction.is_finite()
            || !(0.0..=1.0).contains(&self.cpu_available_fraction)
        {
            bail!(
                "resource snapshot {} cpu_available_fraction must be in [0, 1]",
                self.host_id
            );
        }
        if self.mem_available_mb > self.mem_total_mb {
            bail!(
                "resource snapshot {} mem_available_mb must be <= mem_total_mb",
                self.host_id
            );
        }
        Ok(())
    }
}

/// Two `resets_at` values that differ by no more than this are treated as
/// the same reset generation, not a new one.
///
/// The Anthropic usage API computes `resets_at` dynamically per request
/// rather than returning a fixed, stored absolute timestamp: polling the
/// *same* live five-hour window a few minutes apart returns values like
/// `2026-10-08T06:50:00.051247Z`, then `.130047Z`, then `.995687Z`, then
/// `.771238Z` -- sub-second jitter on every single call (confirmed live,
/// 2026-10-08, across ten consecutive real polls). Comparing `resets_at` for
/// exact equality -- as every cross-cycle "same generation" check in this
/// codebase originally did -- therefore never matches two real samples of
/// the same generation. That silently defeated burn-rate learning
/// (`controller::evaluate`'s `LinearToReset` pacing stayed on
/// `learning_burn_rate` forever against the live account), banked-reset
/// credit alert de-duplication (every poll re-evaluated as a fresh
/// generation), the long-baseline aggregate burn estimator (every interval
/// misclassified as spanning a reset, inflating the learned rate), and
/// bounded per-generation sample history (discarded and restarted every
/// single cycle). It went undetected because every existing unit test and
/// trace simulation constructs `resets_at` as a fixed `DateTime` literal
/// reused byte-for-byte across synthetic samples, which matches trivially
/// by construction -- this is the first time any of this logic ran against
/// the real, live API.
///
/// 60 seconds is generous relative to the observed sub-second jitter and
/// tiny relative to the shortest real reset interval this codebase handles
/// (`five_hour`, 18,000 seconds) -- nothing resembling a genuine reset can
/// occur within it, so it cannot cause two truly different generations to
/// be treated as one.
pub const RESET_GENERATION_JITTER_TOLERANCE_SECONDS: i64 = 60;

/// Whether `a` and `b` identify the same reset generation, tolerant of the
/// live API's sub-second `resets_at` jitter (see
/// [`RESET_GENERATION_JITTER_TOLERANCE_SECONDS`]). Use this everywhere two
/// `resets_at` values from separate polls are compared; exact `DateTime`
/// equality will not reliably match two samples of the same real-world
/// generation.
pub fn same_reset_generation(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    a.signed_duration_since(b).num_seconds().abs() <= RESET_GENERATION_JITTER_TOLERANCE_SECONDS
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> ResourceSnapshot {
        ResourceSnapshot {
            observed_at: "2026-09-28T12:00:00Z".parse().unwrap(),
            fresh: true,
            host_id: "lab".to_string(),
            cpu_available_fraction: 0.42,
            mem_available_mb: 12288,
            mem_total_mb: 65536,
        }
    }

    #[test]
    fn quota_snapshot_accepts_optional_eligible_backlog_capacity() {
        let json = r#"{
            "observed_at": "2026-09-28T12:00:00Z",
            "windows": [{
                "id": "weekly",
                "used_fraction": 0.42,
                "resets_at": "2026-10-03T00:00:00Z"
            }],
            "eligible_backlog_capacity": 3
        }"#;
        let snapshot: QuotaSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.eligible_backlog_capacity, Some(3));

        let without_capacity = json.replace(",\n            \"eligible_backlog_capacity\": 3", "");
        let snapshot: QuotaSnapshot = serde_json::from_str(&without_capacity).unwrap();
        assert_eq!(snapshot.eligible_backlog_capacity, None);
    }

    #[test]
    fn accepts_a_valid_resource_snapshot() {
        assert!(valid().validate().is_ok());
    }

    #[test]
    fn deserializes_the_plan_example() {
        let json = r#"{
            "observed_at": "2026-09-28T12:00:00Z",
            "fresh": true,
            "host_id": "lab",
            "cpu_available_fraction": 0.42,
            "mem_available_mb": 12288,
            "mem_total_mb": 65536
        }"#;
        let snapshot: ResourceSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot, valid());
    }

    #[test]
    fn fresh_defaults_to_true_when_omitted() {
        let json = r#"{
            "observed_at": "2026-09-28T12:00:00Z",
            "host_id": "lab",
            "cpu_available_fraction": 0.42,
            "mem_available_mb": 12288,
            "mem_total_mb": 65536
        }"#;
        let snapshot: ResourceSnapshot = serde_json::from_str(json).unwrap();
        assert!(snapshot.fresh);
    }

    #[test]
    fn rejects_an_empty_host_id() {
        let snapshot = ResourceSnapshot {
            host_id: String::new(),
            ..valid()
        };
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn rejects_cpu_available_fraction_above_one() {
        let snapshot = ResourceSnapshot {
            cpu_available_fraction: 1.1,
            ..valid()
        };
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn rejects_a_negative_cpu_available_fraction() {
        let snapshot = ResourceSnapshot {
            cpu_available_fraction: -0.1,
            ..valid()
        };
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn rejects_a_non_finite_cpu_available_fraction() {
        let snapshot = ResourceSnapshot {
            cpu_available_fraction: f64::NAN,
            ..valid()
        };
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn rejects_mem_available_above_mem_total() {
        let snapshot = ResourceSnapshot {
            mem_available_mb: 100,
            mem_total_mb: 50,
            ..valid()
        };
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn accepts_mem_available_equal_to_mem_total() {
        let snapshot = ResourceSnapshot {
            mem_available_mb: 50,
            mem_total_mb: 50,
            ..valid()
        };
        assert!(snapshot.validate().is_ok());
    }

    #[test]
    fn missing_required_field_fails_deserialization() {
        let json = r#"{
            "observed_at": "2026-09-28T12:00:00Z",
            "host_id": "lab",
            "cpu_available_fraction": 0.42,
            "mem_available_mb": 12288
        }"#;
        let result: std::result::Result<ResourceSnapshot, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
