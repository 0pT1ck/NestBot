use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferMode {
    #[default]
    Copy,
    Deep,
}

impl TransferMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Deep => "deep",
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Entry {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub file_count: Option<u32>,
    #[serde(default)]
    pub page: Option<u32>,
}

impl Entry {
    pub fn payload(&self) -> String {
        if let Ok(url) = reqwest::Url::parse(&self.link)
            && let Some((_, value)) = url
                .query_pairs()
                .find(|(name, value)| name == "start" && !value.trim().is_empty())
        {
            return value.trim().to_owned();
        }
        self.key.trim().to_owned()
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ClaimRecord {
    pub status: String,
    #[serde(default)]
    pub files: u32,
    #[serde(default)]
    pub failed: u32,
    #[serde(default, with = "media_ids_serde")]
    pub file_ids: MediaIds,
}
impl ClaimRecord {
    pub fn complete(&self, expected: Option<u32>) -> bool {
        self.status == "done" && expected.is_none_or(|n| self.files >= n)
    }
    pub fn contains(&self, media: &str) -> bool {
        let Some((kind, text)) = media.rsplit_once(':') else {
            return false;
        };
        let Ok(id) = text.parse::<i64>() else {
            return false;
        };
        // Match the canonical spelling previously produced by formatting the ID.
        let digits = text.strip_prefix('-').unwrap_or(text);
        if text.starts_with('+') || text == "-0" || (digits.len() > 1 && digits.starts_with('0')) {
            return false;
        }
        self.file_ids
            .by_kind
            .get(kind)
            .is_some_and(|ids| ids.contains(&id))
    }
    pub fn add(&mut self, media: &str) {
        if let Some((kind, id)) = media.split_once(':')
            && let Ok(id) = id.parse()
            && self.file_ids.insert(kind, id)
        {
            self.files = self.file_ids.len() as u32;
        }
    }
}

/// Distinct media pairs; the count always equals the total set cardinality.
/// ClaimRecord's `files` metadata remains independent until a new pair is added.
#[derive(Clone, Default)]
pub struct MediaIds {
    by_kind: BTreeMap<String, BTreeSet<i64>>,
    count: usize,
}

impl MediaIds {
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn insert(&mut self, kind: &str, id: i64) -> bool {
        let inserted = if let Some(ids) = self.by_kind.get_mut(kind) {
            ids.insert(id)
        } else {
            self.by_kind.insert(kind.to_owned(), BTreeSet::from([id]));
            true
        };
        self.count += usize::from(inserted);
        inserted
    }

    fn insert_owned(&mut self, kind: String, id: i64) {
        if self.by_kind.entry(kind).or_default().insert(id) {
            self.count += 1;
        }
    }
}

impl FromIterator<(String, i64)> for MediaIds {
    fn from_iter<T: IntoIterator<Item = (String, i64)>>(pairs: T) -> Self {
        let mut ids = Self::default();
        for (kind, id) in pairs {
            ids.insert_owned(kind, id);
        }
        ids
    }
}

mod media_ids_serde {
    use super::MediaIds;
    use serde::{
        Deserializer, Serializer,
        de::{SeqAccess, Visitor},
        ser::SerializeSeq,
    };
    use std::fmt;

    pub fn serialize<S: Serializer>(ids: &MediaIds, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(ids.len()))?;
        for (kind, values) in &ids.by_kind {
            for id in values {
                sequence.serialize_element(&(kind, id))?;
            }
        }
        sequence.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<MediaIds, D::Error> {
        struct MediaIdsVisitor;

        impl<'de> Visitor<'de> for MediaIdsVisitor {
            type Value = MediaIds;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an array of [kind, id] media pairs")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<MediaIds, A::Error> {
                let mut ids = MediaIds::default();
                while let Some((kind, id)) = sequence.next_element::<(String, i64)>()? {
                    ids.insert_owned(kind, id);
                }
                Ok(ids)
            }
        }

        deserializer.deserialize_seq(MediaIdsVisitor)
    }
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JobReport {
    pub pages: u32,
    pub files: u32,
    pub failed_keys: u32,
    pub failed_files: u32,
    pub skipped_keys: u32,
    pub skipped_files: u32,
    pub warnings: Vec<String>,
    pub search_retry: u32,
    pub search_retry_at: Option<i64>,
    pub search_page: Option<u32>,
    pub search_total_pages: Option<u32>,
    pub transfer_key: Option<u32>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct JobWait {
    pub seconds: u64,
    pub retry_at: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferOptions {
    pub confirmed: bool,
    pub keep_caption: bool,
    pub tag_key: bool,
    pub use_key: bool,
    pub limit: Option<u32>,
    pub selection: Option<String>,
    pub keyword: Option<String>,
    pub pages: Option<u32>,
    pub sort: Option<String>,
}
impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            confirmed: false,
            keep_caption: true,
            tag_key: false,
            use_key: false,
            limit: None,
            selection: None,
            keyword: None,
            pages: Some(1),
            sort: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobPayload {
    Search {
        keyword: String,
        pages: Option<u32>,
        sort: Option<String>,
        resume: bool,
    },
    Transfer {
        #[serde(default)]
        options: TransferOptions,
        keys: Vec<String>,
        batch: Option<String>,
        start: u32,
        end: Option<u32>,
        mode: TransferMode,
        target: String,
        redo: bool,
        dry_run: bool,
    },
    Incoming {
        source_chat: i64,
        message_ids: Vec<i32>,
        mode: TransferMode,
        target: String,
        caption: Option<String>,
        #[serde(default)]
        caption_message: Option<i32>,
    },
}

impl JobPayload {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Search { .. } => "search",
            Self::Transfer { .. } => "transfer",
            Self::Incoming {
                mode: TransferMode::Copy,
                ..
            } => "incoming_copy",
            Self::Incoming { .. } => "incoming",
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Search {
                keyword,
                pages,
                sort,
                ..
            } => {
                anyhow::ensure!(
                    !keyword.trim().is_empty() && keyword.len() <= 1024,
                    "invalid_keyword"
                );
                anyhow::ensure!(!matches!(pages, Some(0)), "invalid_pages");
                anyhow::ensure!(
                    sort.as_deref()
                        .is_none_or(|s| ["time", "hot", "count"].contains(&s)),
                    "invalid_sort"
                );
            }
            Self::Transfer {
                keys,
                batch,
                start,
                end,
                target,
                options,
                ..
            } => {
                anyhow::ensure!(!target.is_empty() && target.len() <= 256, "invalid_target");
                anyhow::ensure!(
                    keys.len() <= 1000 && keys.iter().all(|k| !k.is_empty() && k.len() <= 1024),
                    "invalid_keys"
                );
                anyhow::ensure!(
                    !keys.is_empty() || batch.is_some() || options.keyword.is_some(),
                    "missing_keys"
                );
                if let Some(keyword) = &options.keyword {
                    JobPayload::Search {
                        keyword: keyword.clone(),
                        pages: options.pages,
                        sort: options.sort.clone(),
                        resume: false,
                    }
                    .validate()?;
                }
                anyhow::ensure!(
                    *start >= 1 && end.is_none_or(|n| n >= *start),
                    "invalid_range"
                );
            }
            Self::Incoming {
                message_ids,
                target,
                ..
            } => {
                anyhow::ensure!(
                    !target.is_empty() && message_ids.len() <= 10 && !message_ids.is_empty(),
                    "invalid_media"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed: u64,
    pub total: Option<u64>,
    pub phase: String,
    pub error_code: Option<String>,
    pub retry_at: Option<i64>,
}

pub struct Job {
    pub summary: JobSummary,
    pub payload: JobPayload,
    pub reply_chat: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchSummary {
    pub id: String,
    pub entries: u64,
    pub page: u32,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceEvent {
    pub event: String,
    pub job: Option<JobSummary>,
}

pub fn unix_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
