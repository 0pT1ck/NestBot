use crate::paths::{Paths, atomic_write, read_limited};
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const MAX_RULES: usize = 4096;
const MAX_RULE_FILE_BYTES: usize = 512 * 1024;

pub struct Rules {
    paths: Paths,
    max: usize,
    entries: BTreeSet<(i64, i64)>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    watches: Vec<Watch>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Watch {
    chat_id: i64,
    user_id: i64,
}

impl Rules {
    pub fn load(paths: &Paths, max: usize) -> Result<Self> {
        ensure!(
            (1..=MAX_RULES).contains(&max),
            "rule capacity must be in 1..=4096"
        );
        let path = paths.file("data/watches.toml")?;
        let mut entries = BTreeSet::new();
        if let Some(bytes) = read_limited(&path, MAX_RULE_FILE_BYTES)? {
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| anyhow!("data/watches.toml is not UTF-8"))?;
            let file: RuleFile = toml::from_str(text)
                .map_err(|_| anyhow!("data/watches.toml is malformed or has unsupported fields"))?;
            ensure!(
                file.watches.len() <= max,
                "stored rules exceed configured capacity"
            );
            for watch in file.watches {
                validate_ids(watch.chat_id, watch.user_id)?;
                ensure!(
                    entries.insert((watch.chat_id, watch.user_id)),
                    "data/watches.toml contains duplicate rules"
                );
            }
        }
        Ok(Self {
            paths: paths.clone(),
            max,
            entries,
        })
    }

    pub fn contains(&self, chat_id: i64, user_id: i64) -> bool {
        self.entries.contains(&(chat_id, user_id))
    }

    /// Returns false when already present. Memory changes only after the file's
    /// atomic replacement commits successfully.
    pub fn add(&mut self, chat_id: i64, user_id: i64) -> Result<bool> {
        validate_ids(chat_id, user_id)?;
        let entry = (chat_id, user_id);
        if self.entries.contains(&entry) {
            return Ok(false);
        }
        ensure!(self.entries.len() < self.max, "rule capacity reached");
        let mut updated = self.entries.clone();
        updated.insert(entry);
        self.persist(&updated)?;
        self.entries = updated;
        Ok(true)
    }

    /// Returns false when absent; invalid ID domains always return an error.
    pub fn remove(&mut self, chat_id: i64, user_id: i64) -> Result<bool> {
        validate_ids(chat_id, user_id)?;
        let entry = (chat_id, user_id);
        if !self.entries.contains(&entry) {
            return Ok(false);
        }
        let mut updated = self.entries.clone();
        updated.remove(&entry);
        self.persist(&updated)?;
        self.entries = updated;
        Ok(true)
    }

    /// Stable order by raw MTProto channel ID, then user ID.
    pub fn entries(&self) -> Vec<(i64, i64)> {
        self.entries.iter().copied().collect()
    }

    fn persist(&self, entries: &BTreeSet<(i64, i64)>) -> Result<()> {
        let file = RuleFile {
            watches: entries
                .iter()
                .map(|&(chat_id, user_id)| Watch { chat_id, user_id })
                .collect(),
        };
        let text = toml::to_string(&file)?;
        ensure!(
            text.len() <= MAX_RULE_FILE_BYTES,
            "serialized rules exceed size limit"
        );
        atomic_write(&self.paths.file("data/watches.toml")?, text.as_bytes())
    }
}

fn validate_ids(chat_id: i64, user_id: i64) -> Result<()> {
    ensure!(
        chat_id > 0,
        "chat_id must be a positive raw MTProto channel ID"
    );
    ensure!(user_id > 0, "user_id must be a positive user ID");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_support::TestRoot;
    use std::fs;

    #[test]
    fn missing_file_is_empty_and_changes_round_trip_in_stable_order() {
        let root = TestRoot::new("rules-persist");
        let mut rules = Rules::load(&root.0, 3).unwrap();
        assert!(rules.entries().is_empty());
        assert!(rules.add(20, 3).unwrap());
        assert!(rules.add(10, 5).unwrap());
        assert!(rules.add(10, 2).unwrap());
        assert!(!rules.add(10, 2).unwrap());
        assert!(rules.contains(10, 2));
        assert_eq!(rules.entries(), vec![(10, 2), (10, 5), (20, 3)]);
        let mut loaded = Rules::load(&root.0, 3).unwrap();
        assert_eq!(rules.entries(), loaded.entries());
        assert!(loaded.remove(10, 2).unwrap());
        assert!(!loaded.remove(10, 2).unwrap());
        assert_eq!(
            Rules::load(&root.0, 3).unwrap().entries(),
            vec![(10, 5), (20, 3)]
        );
        loaded.remove(10, 5).unwrap();
        loaded.remove(20, 3).unwrap();
        assert!(Rules::load(&root.0, 3).unwrap().entries().is_empty());
    }

    #[test]
    fn capacity_and_id_bounds_are_enforced() {
        let root = TestRoot::new("rules-bounds");
        assert!(Rules::load(&root.0, 0).is_err());
        assert!(Rules::load(&root.0, MAX_RULES + 1).is_err());
        let mut rules = Rules::load(&root.0, 1).unwrap();
        for (chat, user) in [(0, 1), (-1001234567890, 1), (1, 0), (1, -1)] {
            assert!(rules.add(chat, user).is_err());
            assert!(rules.remove(chat, user).is_err());
        }
        rules.add(1, 2).unwrap();
        assert!(rules.add(2, 3).is_err());
        assert_eq!(rules.entries(), vec![(1, 2)]);
        assert_eq!(Rules::load(&root.0, 1).unwrap().entries(), vec![(1, 2)]);
    }

    #[test]
    fn malformed_oversized_duplicate_and_over_capacity_files_are_not_reset() {
        let root = TestRoot::new("rules-corruption");
        let path = root.0.file("data/watches.toml").unwrap();
        for text in [
            "",
            "watches = [",
            "unknown = []",
            "[[watches]]\nchat_id = -1\nuser_id = 2\n",
            "[[watches]]\nchat_id = 1\nuser_id = 2\nextra = true\n",
            "[[watches]]\nchat_id = 1\nuser_id = 2\n[[watches]]\nchat_id = 1\nuser_id = 2\n",
            "[[watches]]\nchat_id = 1\nuser_id = 2\n[[watches]]\nchat_id = 3\nuser_id = 4\n",
        ] {
            atomic_write(&path, text.as_bytes()).unwrap();
            assert!(Rules::load(&root.0, 1).is_err(), "accepted {text}");
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        atomic_write(&path, &vec![b' '; MAX_RULE_FILE_BYTES + 1]).unwrap();
        assert!(Rules::load(&root.0, MAX_RULES).is_err());
        atomic_write(&path, &[0xff]).unwrap();
        assert!(Rules::load(&root.0, 1).is_err());
    }

    #[test]
    fn failed_persistence_does_not_modify_memory() {
        let root = TestRoot::new("rules-rollback");
        let mut rules = Rules::load(&root.0, 3).unwrap();
        rules.add(1, 2).unwrap();
        let path = root.0.file("data/watches.toml").unwrap();
        let backup = root.0.file("data/original.toml").unwrap();
        fs::rename(&path, &backup).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(rules.add(3, 4).is_err());
        assert!(rules.remove(1, 2).is_err());
        assert_eq!(rules.entries(), vec![(1, 2)]);
        fs::remove_dir(&path).unwrap();
        fs::rename(&backup, &path).unwrap();
        assert_eq!(Rules::load(&root.0, 3).unwrap().entries(), rules.entries());
    }
}
