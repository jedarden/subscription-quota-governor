use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct QuotaSnapshot {
    pub observed_at: DateTime<Utc>,
    #[serde(default = "default_true")]
    pub fresh: bool,
    pub windows: Vec<QuotaWindow>,
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

fn default_true() -> bool {
    true
}
