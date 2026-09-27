use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct QuotaSnapshot {
    pub observed_at: DateTime<Utc>,
    #[serde(default = "default_true")]
    pub fresh: bool,
    pub windows: Vec<QuotaWindow>,
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

fn default_true() -> bool {
    true
}
