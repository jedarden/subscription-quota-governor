//! Read-only inventory check for controller ownership across hosts.
//!
//! State-file locks only coordinate processes that use the same path. This
//! preflight compares stable account and fleet identities instead, and it
//! deliberately does not use `state_path` as an ownership key.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub version: u32,
    pub observed_at: String,
    pub controllers: Vec<Controller>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Controller {
    pub name: String,
    pub host: String,
    pub account_id: String,
    pub fleet_id: String,
    pub mode: Mode,
    pub config_path: Option<String>,
    pub state_path: Option<String>,
    pub actuator: Option<String>,
    pub evidence: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A live controller may write to the fleet.
    Actuating,
    /// A live process observes but cannot write to the fleet.
    ObserveOnly,
    /// A launch path was found and positively verified inactive.
    Disabled,
    /// A design exists but no installed launch path is present.
    Planned,
    /// No launch path exists on this host; any recorded state is historical.
    Absent,
    /// The launch path or its authority could not be classified safely.
    Unknown,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Conflict {
    pub left_record: usize,
    pub right_record: usize,
    pub shared_account: bool,
    pub shared_fleet: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Report {
    pub controller_count: usize,
    pub actuating_count: usize,
    pub conflicts: Vec<Conflict>,
}

impl Inventory {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read inventory {}", path.display()))?;
        let inventory: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse inventory {}", path.display()))?;
        inventory.validate()?;
        Ok(inventory)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported inventory version {}; expected 1", self.version);
        }
        if self.observed_at.trim().is_empty() {
            bail!("inventory observed_at must not be empty");
        }
        if self.controllers.is_empty() {
            bail!("inventory must include at least one controller record");
        }

        let mut names = HashSet::new();
        for (index, controller) in self.controllers.iter().enumerate() {
            let record = index + 1;
            for (field, value) in [
                ("name", controller.name.as_str()),
                ("host", controller.host.as_str()),
                ("account_id", controller.account_id.as_str()),
                ("fleet_id", controller.fleet_id.as_str()),
                ("evidence", controller.evidence.as_str()),
            ] {
                if value.trim().is_empty() {
                    bail!("controller record {record} has an empty {field}");
                }
            }
            for (field, value) in [
                ("name", controller.name.as_str()),
                ("host", controller.host.as_str()),
                ("account_id", controller.account_id.as_str()),
                ("fleet_id", controller.fleet_id.as_str()),
            ] {
                if value.trim() != value || value.chars().any(char::is_whitespace) {
                    bail!(
                        "controller record {record} has a non-canonical {field}; identifiers must not contain whitespace"
                    );
                }
            }
            if !names.insert(controller.name.to_ascii_lowercase()) {
                bail!("controller names must be unique");
            }
            if controller.mode == Mode::Unknown {
                bail!(
                    "controller record {record} has unknown ownership; resolve it before preflight"
                );
            }
            if controller.mode == Mode::Actuating
                && controller
                    .actuator
                    .as_deref()
                    .is_none_or(|actuator| actuator.trim().is_empty())
            {
                bail!(
                    "actuating controller record {record} must identify its actuator destination"
                );
            }
        }
        Ok(())
    }

    pub fn check(&self) -> Report {
        let actuating: Vec<_> = self
            .controllers
            .iter()
            .enumerate()
            .filter(|(_, controller)| controller.mode == Mode::Actuating)
            .collect();
        let mut conflicts = Vec::new();

        for left_index in 0..actuating.len() {
            for right_index in (left_index + 1)..actuating.len() {
                let (left_record, left) = actuating[left_index];
                let (right_record, right) = actuating[right_index];
                let shared_account = left.account_id.eq_ignore_ascii_case(&right.account_id);
                let shared_fleet = left.fleet_id.eq_ignore_ascii_case(&right.fleet_id);
                if shared_account || shared_fleet {
                    conflicts.push(Conflict {
                        left_record: left_record + 1,
                        right_record: right_record + 1,
                        shared_account,
                        shared_fleet,
                    });
                }
            }
        }

        Report {
            controller_count: self.controllers.len(),
            actuating_count: actuating.len(),
            conflicts,
        }
    }
}

pub fn run(path: &Path) -> Result<()> {
    let inventory = Inventory::load(path)?;
    let report = inventory.check();
    if report.conflicts.is_empty() {
        println!(
            "PASS: {} controller records, {} actuation-capable owner(s); no account or fleet overlap.",
            report.controller_count, report.actuating_count
        );
        return Ok(());
    }

    for conflict in &report.conflicts {
        let mut shared = Vec::new();
        if conflict.shared_account {
            shared.push("account");
        }
        if conflict.shared_fleet {
            shared.push("fleet");
        }
        eprintln!(
            "CONFLICT: inventory records {} and {} both actuate the same {}; separate state_path values do not establish ownership.",
            conflict.left_record,
            conflict.right_record,
            shared.join(" and ")
        );
    }
    bail!(
        "controller ownership preflight found {} conflict(s)",
        report.conflicts.len()
    )
}

#[cfg(test)]
mod tests {
    use super::{Inventory, Mode};

    fn fixture(name: &str) -> Inventory {
        // Cargo runs unit tests from the package root; resolve fixtures there
        // at runtime so cached test binaries work across clean extractions.
        let path = std::path::Path::new("tests/fixtures/controller-ownership").join(name);
        Inventory::load(&path).unwrap()
    }

    #[test]
    fn current_live_inventory_has_no_duplicate_controller() {
        let inventory = fixture("current-live.json");
        assert_eq!(inventory.check().conflicts.len(), 0);
    }

    #[test]
    fn single_subgov_owner_is_clean_before_codex_governor_handoff() {
        let inventory = fixture("subgov-single-owner.json");
        assert_eq!(inventory.check().actuating_count, 2);
        assert_eq!(inventory.check().conflicts.len(), 0);
    }

    #[test]
    fn same_account_conflicts_even_when_state_paths_differ() {
        let inventory = fixture("same-account-different-state.json");
        let report = inventory.check();
        assert_eq!(report.conflicts.len(), 1);
        assert!(report.conflicts[0].shared_account);
        assert!(!report.conflicts[0].shared_fleet);
    }

    #[test]
    fn same_fleet_conflicts_even_when_account_ids_differ() {
        let inventory = fixture("same-fleet-different-account.json");
        let report = inventory.check();
        assert_eq!(report.conflicts.len(), 1);
        assert!(!report.conflicts[0].shared_account);
        assert!(report.conflicts[0].shared_fleet);
    }

    #[test]
    fn shared_account_across_hosts_conflicts() {
        let inventory = fixture("two-host-shared-account.json");
        assert_eq!(inventory.check().conflicts.len(), 1);
    }

    #[test]
    fn unknown_ownership_fails_closed() {
        let raw = include_str!("../tests/fixtures/controller-ownership/unknown-owner.json");
        let inventory: Inventory = serde_json::from_str(raw).unwrap();
        assert!(inventory.validate().is_err());
        assert_eq!(inventory.controllers[0].mode, Mode::Unknown);
    }

    #[test]
    fn account_and_fleet_ids_reject_whitespace_aliases() {
        let raw = r#"{
            "version": 1,
            "observed_at": "fixture",
            "controllers": [{
                "name": "subgov-codex",
                "host": "codinghome",
                "account_id": "codex:local profile",
                "fleet_id": "needle:agent=codex",
                "mode": "actuating",
                "config_path": "config.yaml",
                "state_path": "state.json",
                "actuator": "target file",
                "evidence": "canonical-id validation fixture"
            }]
        }"#;
        let inventory: Inventory = serde_json::from_str(raw).unwrap();
        assert!(inventory.validate().is_err());
    }
}
