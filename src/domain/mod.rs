use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub file_ids: Vec<(String, i64)>,
}
impl ClaimRecord {
    pub fn complete(&self, expected: Option<u32>) -> bool {
        self.status == "done" && expected.is_none_or(|n| self.files >= n)
    }
    pub fn contains(&self, media: &str) -> bool {
        self.file_ids
            .iter()
            .any(|(kind, id)| media == format!("{kind}:{id}"))
    }
    pub fn add(&mut self, media: &str) {
        if !self.contains(media)
            && let Some((kind, id)) = media.split_once(':')
            && let Ok(id) = id.parse()
        {
            self.file_ids.push((kind.into(), id));
            self.files = self.file_ids.len() as u32;
        }
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
