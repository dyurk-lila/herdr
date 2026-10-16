//! Machine/editor learning without drafts, screen cells, or terminal coordinates.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::prediction_words::WordRules;

pub(crate) const MAX_PROFILE_BYTES: u64 = 256 * 1024;
const STORE_VERSION: u32 = 2;
const MAX_PROFILES: usize = 128;
const MAX_FINGERPRINTS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditorProfile {
    pub(crate) machine_key: String,
    pub(crate) agent: String,
    pub(crate) word_rules: [WordRules; 2],
    pub(crate) echo_trained: bool,
    #[serde(with = "fingerprints")]
    pub(crate) prompt_fingerprints: Vec<[u8; 32]>,
}

impl EditorProfile {
    pub(crate) fn is_valid(&self) -> bool {
        valid_identity(&self.machine_key, &self.agent)
            && self.word_rules.iter().all(WordRules::is_valid)
            && self.prompt_fingerprints.len() <= MAX_FINGERPRINTS
            && self
                .prompt_fingerprints
                .iter()
                .enumerate()
                .all(|(index, fingerprint)| {
                    !self.prompt_fingerprints[..index].contains(fingerprint)
                })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProfileUpdate {
    Observe {
        epoch: u64,
        profile: EditorProfile,
    },
    Invalidate {
        epoch: u64,
        machine_key: String,
        agent: String,
    },
}

impl ProfileUpdate {
    pub(crate) fn epoch(&self) -> u64 {
        match self {
            Self::Observe { epoch, .. } | Self::Invalidate { epoch, .. } => *epoch,
        }
    }

    pub(crate) fn is_valid(&self) -> bool {
        match self {
            Self::Observe { profile, .. } => profile.is_valid(),
            Self::Invalidate {
                machine_key, agent, ..
            } => valid_identity(machine_key, agent),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Profiles {
    invalidation_epoch: u64,
    records: Vec<EditorProfile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredProfiles {
    version: u32,
    invalidation_epoch: u64,
    profiles: Vec<EditorProfile>,
}

impl Profiles {
    pub(crate) fn epoch(&self) -> u64 {
        self.invalidation_epoch
    }

    pub(crate) fn profile(&self, machine_key: &str, agent: &str) -> Option<&EditorProfile> {
        self.records
            .iter()
            .find(|profile| profile.machine_key == machine_key && profile.agent == agent)
    }

    pub(crate) fn observe(&mut self, observation: EditorProfile) -> bool {
        if !observation.is_valid() {
            return false;
        }
        if let Some(index) = self.records.iter().position(|profile| {
            profile.machine_key == observation.machine_key && profile.agent == observation.agent
        }) {
            let mut merged = self.records[index].clone();
            for (rules, observed) in merged.word_rules.iter_mut().zip(&observation.word_rules) {
                rules.merge_observation(observed);
            }
            merged.echo_trained |= observation.echo_trained;
            for fingerprint in observation.prompt_fingerprints {
                if !merged.prompt_fingerprints.contains(&fingerprint) {
                    if merged.prompt_fingerprints.len() == MAX_FINGERPRINTS {
                        merged.prompt_fingerprints.remove(0);
                    }
                    merged.prompt_fingerprints.push(fingerprint);
                }
            }
            if merged == self.records[index] {
                return false;
            }
            self.records.remove(index);
            self.records.push(merged);
        } else {
            if self.records.len() == MAX_PROFILES {
                self.records.remove(0);
            }
            self.records.push(observation);
        }
        true
    }

    pub(crate) fn invalidate(&mut self, machine_key: &str, agent: &str) -> bool {
        if !valid_identity(machine_key, agent) {
            return false;
        }
        let Some(next_epoch) = self.invalidation_epoch.checked_add(1) else {
            return false;
        };
        self.records
            .retain(|profile| profile.machine_key != machine_key || profile.agent != agent);
        self.invalidation_epoch = next_epoch;
        true
    }

    pub(crate) fn reset_to_epoch(&mut self, minimum: u64) -> bool {
        let Some(next_epoch) = self.invalidation_epoch.checked_add(1) else {
            return false;
        };
        self.records.clear();
        self.invalidation_epoch = next_epoch.max(minimum);
        true
    }

    pub(crate) fn apply(&mut self, update: &ProfileUpdate) -> bool {
        match update {
            ProfileUpdate::Observe { epoch, profile } if *epoch == self.epoch() => {
                self.observe(profile.clone())
            }
            // Concurrent invalidations for other editors must not hide a mismatch.
            ProfileUpdate::Invalidate {
                epoch,
                machine_key,
                agent,
            } if *epoch <= self.epoch() => self.invalidate(machine_key, agent),
            _ => false,
        }
    }

    pub(crate) fn to_json(&self) -> Result<Vec<u8>, String> {
        let bytes = serde_json::to_vec(&StoredProfiles {
            version: STORE_VERSION,
            invalidation_epoch: self.invalidation_epoch,
            profiles: self.records.clone(),
        })
        .map_err(|_| "could not encode editor profiles".to_owned())?;
        if bytes.len() as u64 > MAX_PROFILE_BYTES {
            return Err("editor profiles exceed the storage limit".into());
        }
        Ok(bytes)
    }

    pub(crate) fn from_json(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() as u64 > MAX_PROFILE_BYTES {
            return Err("editor profiles exceed the storage limit".into());
        }
        let mut unknown_fields = false;
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let stored: StoredProfiles = serde_ignored::deserialize(&mut decoder, |_| {
            unknown_fields = true;
        })
        .map_err(|_| "invalid editor profile encoding".to_owned())?;
        decoder
            .end()
            .map_err(|_| "invalid editor profile encoding".to_owned())?;
        if unknown_fields || stored.version != STORE_VERSION || stored.profiles.len() > MAX_PROFILES
        {
            return Err("unsupported editor profile schema or record count".into());
        }
        for (index, profile) in stored.profiles.iter().enumerate() {
            if !profile.is_valid()
                || stored.profiles[..index].iter().any(|earlier| {
                    earlier.machine_key == profile.machine_key && earlier.agent == profile.agent
                })
            {
                return Err("invalid or duplicate editor profile".into());
            }
        }
        Ok(Self {
            invalidation_epoch: stored.invalidation_epoch,
            records: stored.profiles,
        })
    }
}

pub(crate) fn machine_key(target: &str) -> String {
    format!("{:x}", Sha256::digest(target.as_bytes()))
}

fn valid_identity(machine_key: &str, agent: &str) -> bool {
    valid_digest(machine_key) && crate::detect::parse_canonical_agent_label(agent).is_some()
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

mod fingerprints {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{valid_digest, MAX_FINGERPRINTS};

    pub(super) fn serialize<S: Serializer>(
        values: &[[u8; 32]],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        values
            .iter()
            .map(|digest| {
                digest
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<[u8; 32]>, D::Error> {
        let values = Vec::<String>::deserialize(deserializer)?;
        if values.len() > MAX_FINGERPRINTS {
            return Err(serde::de::Error::custom("too many prompt fingerprints"));
        }
        values
            .into_iter()
            .map(|value| {
                if !valid_digest(&value) {
                    return Err(serde::de::Error::custom("invalid prompt fingerprint"));
                }
                let mut digest = [0; 32];
                for (index, byte) in digest.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                        .map_err(|_| serde::de::Error::custom("invalid prompt fingerprint"))?;
                }
                Ok(digest)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(target: &str, agent: &str) -> EditorProfile {
        EditorProfile {
            machine_key: machine_key(target),
            agent: agent.into(),
            word_rules: std::array::from_fn(|_| WordRules::default()),
            echo_trained: true,
            prompt_fingerprints: vec![[1; 32]],
        }
    }

    #[test]
    fn merges_independent_gestures_and_keeps_machine_and_agent_scopes() {
        let mut bank = Profiles::default();
        let mut first = profile("host-a", "claude");
        assert!(first.word_rules[0].learn("prefix alpha", 7));
        assert!(bank.observe(first.clone()));
        assert!(!bank.observe(first));
        let mut second = profile("host-a", "claude");
        assert!(second.word_rules[1].learn("prefix beta", 7));
        second.prompt_fingerprints = vec![[2; 32]];
        assert!(bank.observe(second));
        let merged = bank.profile(&machine_key("host-a"), "claude").unwrap();
        assert!(merged.word_rules.iter().all(WordRules::is_trained));
        assert_eq!(merged.prompt_fingerprints, vec![[1; 32], [2; 32]]);
        assert!(bank.profile(&machine_key("host-b"), "claude").is_none());
        assert!(bank.profile(&machine_key("host-a"), "codex").is_none());
        assert!(bank.invalidate(&machine_key("host-a"), "claude"));
        assert!(bank.invalidate(&machine_key("host-a"), "claude"));
        assert_eq!(bank.epoch(), 2);
    }

    #[test]
    fn roundtrip_contains_only_hashed_identity_and_learning() {
        let mut bank = Profiles::default();
        let mut observed = profile("private-host.example", "codex");
        assert!(observed.word_rules[0].learn("private user draft", 13));
        assert!(bank.observe(observed));
        let bytes = bank.to_json().unwrap();
        assert_eq!(Profiles::from_json(&bytes).unwrap(), bank);
        let text = String::from_utf8(bytes).unwrap();
        for private in [
            "private-host",
            "private user draft",
            "cursor",
            "screen",
            "target",
        ] {
            assert!(!text.contains(private));
        }
    }

    #[test]
    fn invalidation_epoch_fences_stale_cross_client_observations() {
        let mut bank = Profiles::default();
        let stale = ProfileUpdate::Observe {
            epoch: bank.epoch(),
            profile: profile("host-a", "claude"),
        };
        assert!(bank.apply(&stale));
        assert!(bank.observe(profile("host-b", "codex")));
        assert!(bank.apply(&ProfileUpdate::Invalidate {
            epoch: 0,
            machine_key: machine_key("host-a"),
            agent: "claude".into(),
        }));
        assert_eq!(bank.epoch(), 1);
        assert!(!bank.apply(&stale));
        assert!(bank.profile(&machine_key("host-a"), "claude").is_none());
        assert!(bank.profile(&machine_key("host-b"), "codex").is_some());
        assert!(bank.apply(&ProfileUpdate::Observe {
            epoch: 1,
            profile: profile("host-a", "claude")
        }));
        let mut restored = Profiles::from_json(&bank.to_json().unwrap()).unwrap();
        assert_eq!(restored.epoch(), 1);
        assert!(restored.apply(&ProfileUpdate::Invalidate {
            epoch: 1,
            machine_key: machine_key("absent-host"),
            agent: "claude".into(),
        }));
        assert_eq!(restored.epoch(), 2);
        assert!(!restored.apply(&ProfileUpdate::Observe {
            epoch: 1,
            profile: profile("host-b", "codex")
        }));
    }

    #[test]
    fn reset_clears_all_learning_and_advances_to_requested_minimum() {
        let mut bank = Profiles::default();
        assert!(bank.observe(profile("host-a", "claude")));
        assert!(bank.observe(profile("host-b", "codex")));
        assert!(bank.reset_to_epoch(7));
        assert_eq!(bank.epoch(), 7);
        assert!(bank.records.is_empty());
        assert!(bank.reset_to_epoch(3));
        assert_eq!(bank.epoch(), 8);
    }

    #[test]
    fn concurrent_invalidations_remove_both_targets_and_fence_stale_observations() {
        let mut bank = Profiles::default();
        let first = profile("host-a", "claude");
        let second = profile("host-b", "codex");
        assert!(bank.observe(first.clone()));
        assert!(bank.observe(second.clone()));
        for profile in [&first, &second] {
            assert!(bank.apply(&ProfileUpdate::Invalidate {
                epoch: 0,
                machine_key: profile.machine_key.clone(),
                agent: profile.agent.clone(),
            }));
        }
        assert_eq!(bank.epoch(), 2);
        for profile in [first, second] {
            assert!(bank.profile(&profile.machine_key, &profile.agent).is_none());
            assert!(!bank.apply(&ProfileUpdate::Observe { epoch: 0, profile }));
        }
        let before = bank.clone();
        assert!(!bank.apply(&ProfileUpdate::Invalidate {
            epoch: 3,
            machine_key: machine_key("host-a"),
            agent: "claude".into(),
        }));
        assert_eq!(bank, before);
    }

    #[test]
    fn rejects_unknown_fields_invalid_evidence_and_noncanonical_identities() {
        let mut bank = Profiles::default();
        assert!(bank.observe(profile("host-a", "claude")));
        let valid: serde_json::Value = serde_json::from_slice(&bank.to_json().unwrap()).unwrap();
        for bad in [
            serde_json::json!({"version": 1, "invalidation_epoch": 0, "profiles": []}),
            serde_json::json!({"version": 2, "invalidation_epoch": 0, "profiles": [], "draft": "user text"}),
        ] {
            assert!(Profiles::from_json(&serde_json::to_vec(&bad).unwrap()).is_err());
        }
        let invalid_rules =
            vec![serde_json::json!({"candidates": vec![false; 8], "trained": false}); 2];
        for (field, value) in [
            ("machine_key", serde_json::json!("host-a")),
            ("agent", serde_json::json!("claude-code")),
            ("prompt_fingerprints", serde_json::json!(["draft text"])),
            ("word_rules", serde_json::json!(invalid_rules)),
            ("draft", serde_json::json!("user text")),
        ] {
            let mut bad = valid.clone();
            bad["profiles"][0][field] = value;
            assert!(Profiles::from_json(&serde_json::to_vec(&bad).unwrap()).is_err());
        }
        let mut bad = valid;
        bad["profiles"][0]["word_rules"][0]["draft"] = serde_json::json!("user text");
        assert!(Profiles::from_json(&serde_json::to_vec(&bad).unwrap()).is_err());
    }

    #[test]
    fn bounds_records_and_fingerprints_without_exceeding_byte_limit() {
        let mut bank = Profiles::default();
        for index in 0..=MAX_PROFILES {
            let mut observed = profile(&format!("host-{index}"), "claude");
            observed.prompt_fingerprints = (0..MAX_FINGERPRINTS)
                .map(|index| [index as u8; 32])
                .collect();
            assert!(bank.observe(observed));
        }
        assert_eq!(bank.records.len(), MAX_PROFILES);
        assert!(bank.profile(&machine_key("host-0"), "claude").is_none());
        let bytes = bank.to_json().unwrap();
        assert!((bytes.len() as u64) < MAX_PROFILE_BYTES);
        assert_eq!(Profiles::from_json(&bytes).unwrap(), bank);
        let mut newer = profile("host-1", "claude");
        newer.prompt_fingerprints = vec![[99; 32]];
        assert!(bank.observe(newer));
        let merged = bank.profile(&machine_key("host-1"), "claude").unwrap();
        assert_eq!(merged.prompt_fingerprints.len(), MAX_FINGERPRINTS);
        assert_eq!(merged.prompt_fingerprints.last(), Some(&[99; 32]));
    }
}
