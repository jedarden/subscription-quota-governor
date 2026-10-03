//! Fixture-safety scanner: plan.md section 14 requirement 9 requires every
//! committed fixture to be synthetic or irreversibly anonymized, with no
//! private infrastructure name or credential-shaped value.
//!
//! No fixtures are committed yet -- WP1's contract-test beads will add them
//! under `examples/` and `tests/fixtures/`. This module is the reusable
//! check that work runs against: `scan_repo_fixtures` walks those
//! directories, and the `real_fixtures_are_clean` test below fails the build
//! the moment an unsafe value lands in either one.
//!
//! The detectors are intentionally conservative heuristics, not a guarantee.
//! A fixture that must contain a token-shaped string should use an obvious
//! placeholder marker (`fake`, `test`, `placeholder`, `synthetic`, `xxx`, ...)
//! so it reads as safe to both this scanner and a human reviewer.

use std::fs;
use std::path::{Path, PathBuf};

/// Directories whose committed contents must be synthetic or anonymized.
const SCANNED_ROOTS: &[&str] = &["examples", "tests/fixtures"];

/// Extensions worth scanning as text; binary and lockfile noise is skipped.
const SCANNED_EXTENSIONS: &[&str] = &["json", "yaml", "yml", "jsonl", "txt"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) path: PathBuf,
    pub(crate) line: usize,
    pub(crate) rule: &'static str,
    pub(crate) excerpt: String,
}

/// Scans every configured fixture root under `repo_root`.
pub(crate) fn scan_repo_fixtures(repo_root: &Path) -> Vec<Finding> {
    SCANNED_ROOTS
        .iter()
        .flat_map(|relative| scan_directory(&repo_root.join(relative)))
        .collect()
}

/// Scans one directory tree. Missing directories yield no findings, since
/// `tests/fixtures` does not exist until a later bead creates it.
pub(crate) fn scan_directory(root: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    if !root.exists() {
        return findings;
    }
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let scanned = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| SCANNED_EXTENSIONS.contains(&ext));
            if !scanned {
                continue;
            }
            if let Ok(contents) = fs::read_to_string(&path) {
                findings.extend(scan_text(&path, &contents));
            }
        }
    }
    findings
}

pub(crate) fn scan_text(path: &Path, contents: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (index, line) in contents.lines().enumerate() {
        for detector in DETECTORS {
            if let Some(excerpt) = (detector.detect)(line) {
                findings.push(Finding {
                    path: path.to_owned(),
                    line: index + 1,
                    rule: detector.name,
                    excerpt,
                });
            }
        }
    }
    findings
}

struct Detector {
    name: &'static str,
    detect: fn(&str) -> Option<String>,
}

const DETECTORS: &[Detector] = &[
    Detector {
        name: "pem_private_key",
        detect: detect_pem_private_key,
    },
    Detector {
        name: "bearer_token",
        detect: detect_bearer_token,
    },
    Detector {
        name: "aws_access_key",
        detect: detect_aws_access_key,
    },
    Detector {
        name: "jwt",
        detect: detect_jwt,
    },
    Detector {
        name: "secret_shaped_field",
        detect: detect_secret_field,
    },
    Detector {
        name: "private_ip",
        detect: detect_private_ip,
    },
    Detector {
        name: "private_domain_suffix",
        detect: detect_private_domain_suffix,
    },
];

fn detect_pem_private_key(line: &str) -> Option<String> {
    let trimmed = line.trim();
    (trimmed.starts_with("-----BEGIN") && trimmed.contains("PRIVATE KEY"))
        .then(|| trimmed.to_owned())
}

fn detect_bearer_token(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let position = lower.find("bearer ")?;
    let token_start = position + "bearer ".len();
    let token: String = line[token_start..]
        .chars()
        .take_while(|character| is_token_char(*character))
        .collect();
    (token.len() >= 16).then(|| format!("Bearer {token}"))
}

fn is_token_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '+' | '/' | '=')
}

fn detect_aws_access_key(line: &str) -> Option<String> {
    for (index, _) in line.match_indices("AKIA") {
        let candidate = &line[index..];
        let key: String = candidate
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric())
            .collect();
        if key.len() == 20
            && key[4..]
                .chars()
                .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
        {
            return Some(key);
        }
    }
    None
}

fn detect_jwt(line: &str) -> Option<String> {
    line.split(|character: char| {
        character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ':' | '[' | ']')
    })
    .find(|candidate| is_jwt_shaped(candidate))
    .map(str::to_owned)
}

fn is_jwt_shaped(candidate: &str) -> bool {
    let segments: Vec<&str> = candidate.split('.').collect();
    segments.len() == 3
        && segments.iter().all(|segment| {
            segment.len() >= 10
                && segment.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
        })
}

const SECRET_FIELD_NAMES: &[&str] = &[
    "token",
    "secret",
    "password",
    "apikey",
    "accesstoken",
    "refreshtoken",
    "credential",
    "credentials",
    "clientsecret",
    "privatekey",
    "authorization",
];

const PLACEHOLDER_MARKERS: &[&str] = &[
    "redacted",
    "changeme",
    "change_me",
    "placeholder",
    "fake",
    "test",
    "example",
    "synthetic",
    "xxx",
    "***",
    "todo",
    "dummy",
];

/// Flags a `key: value`-shaped line whose key is a known secret-ish field
/// name and whose value neither looks like an obvious placeholder nor is
/// too short to plausibly be a real credential.
fn detect_secret_field(line: &str) -> Option<String> {
    let (key, value) = split_key_value(line)?;
    let normalized_key: String = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if !SECRET_FIELD_NAMES.contains(&normalized_key.as_str()) {
        return None;
    }
    let cleaned_value = value
        .trim()
        .trim_matches(|character| matches!(character, '"' | '\'' | ','));
    if cleaned_value.len() < 12 {
        return None;
    }
    let lower_value = cleaned_value.to_ascii_lowercase();
    if PLACEHOLDER_MARKERS
        .iter()
        .any(|marker| lower_value.contains(marker))
    {
        return None;
    }
    Some(format!("{key}: {cleaned_value}"))
}

fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let (key, rest) = line.split_at(colon);
    let key = key
        .trim()
        .trim_matches(|character| matches!(character, '"' | '\''));
    if key.is_empty()
        || !key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return None;
    }
    Some((key, &rest[1..]))
}

/// Flags an RFC 1918 or Tailscale/CGNAT (100.64.0.0/10) IPv4 literal.
/// Loopback and the RFC 5737 documentation ranges are deliberately not
/// flagged since they are safe to use in fixtures by design.
fn detect_private_ip(line: &str) -> Option<String> {
    for candidate in line.split(|character: char| !(character.is_ascii_digit() || character == '.'))
    {
        if let Some(octets) = parse_ipv4(candidate) {
            if is_private_ipv4(octets) {
                return Some(candidate.to_owned());
            }
        }
    }
    None
}

fn parse_ipv4(candidate: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = candidate.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
            return None;
        }
        octets[index] = part.parse().ok()?;
    }
    Some(octets)
}

fn is_private_ipv4(octets: [u8; 4]) -> bool {
    match octets {
        [10, ..] => true,
        [172, second, ..] if (16..=31).contains(&second) => true,
        [192, 168, ..] => true,
        [100, second, ..] if (64..=127).contains(&second) => true,
        _ => false,
    }
}

const PRIVATE_DOMAIN_SUFFIXES: &[&str] = &[
    ".internal",
    ".local",
    ".lan",
    ".corp",
    ".ts.net",
    ".tailscale.net",
    ".consul",
];

/// Flags a hostname-shaped occurrence of a private-infrastructure domain
/// suffix. Requires a label character immediately before the suffix so
/// unrelated paths like `~/.local/state/...` are not mistaken for a hostname
/// ending in `.local`.
fn detect_private_domain_suffix(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for suffix in PRIVATE_DOMAIN_SUFFIXES {
        let mut search_from = 0;
        while let Some(relative) = lower[search_from..].find(suffix) {
            let position = search_from + relative;
            let end = position + suffix.len();
            let preceded_by_label = position > 0 && bytes[position - 1].is_ascii_alphanumeric();
            let followed_by_boundary = lower[end..]
                .chars()
                .next()
                .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '-');
            if preceded_by_label && followed_by_boundary {
                let start = lower[..position]
                    .rfind(|character: char| {
                        character.is_whitespace()
                            || matches!(character, '"' | '\'' | '/' | ':' | ',' | '=')
                    })
                    .map(|index| index + 1)
                    .unwrap_or(0);
                return Some(line[start..end].to_owned());
            }
            search_from = end;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings_for(line: &str) -> Vec<Finding> {
        scan_text(Path::new("fixture.json"), line)
    }

    #[test]
    fn flags_a_pem_private_key_block() {
        // Synthetic PEM header for the detector under test, not a real key.
        let findings = findings_for("-----BEGIN RSA PRIVATE KEY-----"); // gitleaks:allow
        assert!(findings
            .iter()
            .any(|finding| finding.rule == "pem_private_key"));
    }

    #[test]
    fn flags_a_bearer_token() {
        let findings = findings_for(r#""Authorization": "Bearer sk-live-abcdef1234567890""#);
        assert!(findings
            .iter()
            .any(|finding| finding.rule == "bearer_token"));
    }

    #[test]
    fn flags_an_aws_access_key() {
        // Synthetic key shape for the detector under test, not a real credential.
        let findings = findings_for("aws_key = AKIAABCDEFGHIJKLMNOP"); // gitleaks:allow
        assert!(findings
            .iter()
            .any(|finding| finding.rule == "aws_access_key"));
    }

    #[test]
    fn flags_a_jwt_shaped_value() {
        // Synthetic JWT shape for the detector under test, not a real token.
        let findings = findings_for(
            r#"{"id_token": "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"}"#, // gitleaks:allow
        );
        assert!(findings.iter().any(|finding| finding.rule == "jwt"));
    }

    #[test]
    fn flags_a_realistic_looking_secret_field() {
        // Synthetic secret-shaped value for the detector under test, not a real credential.
        let findings = findings_for(r#""accessToken": "a1b2c3d4e5f6g7h8i9j0k1l2m3""#); // gitleaks:allow
        assert!(findings
            .iter()
            .any(|finding| finding.rule == "secret_shaped_field"));
    }

    #[test]
    fn does_not_flag_an_obviously_placeholder_secret_value() {
        let findings = findings_for(r#""accessToken": "fake-access-token-placeholder""#);
        assert!(findings.is_empty());
    }

    #[test]
    fn does_not_flag_an_unrelated_field_that_merely_contains_credential_as_a_substring() {
        let findings = findings_for(r#"credentials_path: ~/.claude/.credentials.json"#);
        assert!(findings.is_empty());
    }

    #[test]
    fn flags_rfc1918_and_tailscale_cgnat_addresses() {
        for address in ["10.20.30.40", "192.168.1.5", "172.20.5.5", "100.100.5.5"] {
            let findings = findings_for(address);
            assert!(
                findings.iter().any(|finding| finding.rule == "private_ip"),
                "expected {address} to be flagged as a private IP"
            );
        }
    }

    #[test]
    fn does_not_flag_public_or_documentation_addresses() {
        for address in ["8.8.8.8", "203.0.113.5", "127.0.0.1", "172.5.5.5"] {
            let findings = findings_for(address);
            assert!(
                findings.is_empty(),
                "expected {address} not to be flagged, got {findings:?}"
            );
        }
    }

    #[test]
    fn flags_a_private_domain_suffix() {
        let findings = findings_for(r#""usage_url": "https://quota.example-corp.internal/v1""#);
        assert!(findings
            .iter()
            .any(|finding| finding.rule == "private_domain_suffix"));
    }

    #[test]
    fn does_not_flag_a_local_state_path() {
        let findings = findings_for("state_path: ~/.local/state/subscription-governor/codex.json");
        assert!(findings.is_empty(), "unexpected findings: {findings:?}");
    }

    #[test]
    fn does_not_flag_ordinary_synthetic_quota_json() {
        let synthetic = r#"{
  "observed_at": "2026-09-12T12:00:00Z",
  "fresh": true,
  "windows": [
    {"id":"five_hour","used_fraction":0.35,"resets_at":"2030-09-12T17:00:00Z","duration_minutes":300}
  ]
}"#;
        assert!(findings_for(synthetic).is_empty());
    }

    #[test]
    fn real_fixtures_are_clean() {
        // Resolve at runtime because shared Cargo test binaries can run from
        // different clean archive extractions than the one that built them.
        let repo_root = std::env::current_dir().expect("Cargo tests run from the package root");
        let findings = scan_repo_fixtures(&repo_root);
        assert!(
            findings.is_empty(),
            "fixture-safety scan found unsafe content: {findings:#?}"
        );
    }
}
