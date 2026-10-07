use crate::domain::Entry;
use grammers_tl_types as tl;
use regex::Regex;
use std::{collections::HashMap, sync::LazyLock};

static PAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"第\s*(\d+)\s*页").unwrap());
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)\s*[（(]\s*(https?://[^)）]+)\s*[)）]\s*$").unwrap());
static WAIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*(秒|分钟|分)").unwrap());

#[derive(Default, serde::Serialize)]
pub struct SearchPage {
    pub keyword: String,
    pub page: Option<u32>,
    pub total_pages: Option<u32>,
    pub hot_searches: Vec<String>,
    pub entries: Vec<Entry>,
}
impl SearchPage {
    pub fn is_result(&self) -> bool {
        !self.entries.is_empty() || (!self.keyword.is_empty() && self.page.is_some())
    }
}
static TOTAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"共\s*(\d+)\s*页").unwrap());

fn label<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.match_indices(name).find_map(|(pos, _)| {
        let value = &line[pos + name.len()..];
        value
            .strip_prefix(':')
            .or_else(|| value.strip_prefix('：'))
            .map(str::trim)
    })
}

pub fn line_links(text: &str, entities: &[tl::enums::MessageEntity]) -> HashMap<usize, String> {
    let mut links = HashMap::new();
    for entity in entities {
        if let tl::enums::MessageEntity::TextUrl(entity) = entity {
            if entity.offset < 0 || entity.length <= 0 {
                continue;
            }
            let mut utf16 = 0;
            let mut line = 0;
            for ch in text.chars() {
                if utf16 >= entity.offset as usize {
                    break;
                }
                utf16 += ch.len_utf16();
                if ch == '\n' {
                    line += 1;
                }
            }
            links.entry(line).or_insert_with(|| entity.url.clone());
        }
    }
    links
}

pub fn parse_search(text: &str, links: &HashMap<usize, String>) -> SearchPage {
    let mut result = SearchPage::default();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.contains("搜索词")
            && (line.contains('🔎') || (result.entries.is_empty() && result.keyword.is_empty()))
        {
            if let Some(keyword) = label(line, "搜索词").filter(|s| !s.is_empty()) {
                result.keyword = keyword.into();
            }
            continue;
        }
        if line.contains("表示") && line.contains("包含") {
            continue;
        }
        if let Some(key) = label(line, "密钥").filter(|s| !s.is_empty()) {
            result.entries.push(Entry {
                key: key.into(),
                page: result.page,
                ..Default::default()
            });
            continue;
        } else if line.contains("密钥") {
            continue;
        } else if let Some(description) = label(line, "描述") {
            if let Some(entry) = result.entries.last_mut() {
                if let Some(c) = LINK.captures(description) {
                    entry.description = c[1].trim().into();
                    entry.link = c[2].into();
                } else {
                    entry.description = description.into();
                    entry.link = links.get(&number).cloned().unwrap_or_default();
                }
            }
            continue;
        } else if let Some(count) = label(line, "文件个数")
            && let Some(entry) = result.entries.last_mut()
        {
            if !count
                .chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
                .is_empty()
            {
                entry.file_count = count
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .ok();
            }
            continue;
        }
        if let Some(items) = label(line, "热门搜索") {
            result.hot_searches = items
                .split(['|', '｜'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            continue;
        }
        if let Some(c) = PAGE.captures(line) {
            result.page = c[1].parse().ok();
            if let Some(c) = TOTAL.captures(line) {
                result.total_pages = c[1].parse().ok();
            }
        }
    }
    for entry in &mut result.entries {
        if entry.page.is_none() {
            entry.page = result.page;
        }
    }
    result
}

pub fn rate_wait(text: &str) -> Option<u64> {
    // Processing placeholders are not throttling notices. Require either an
    // explicit rate-limit signal or a timed instruction to retry later.
    let lower = text.to_lowercase();
    let explicit = [
        "频繁",
        "限流",
        "次数限制",
        "速率限制",
        "请求限制",
        "暂时无法",
        "稍后再试",
        "稍后重试",
        "稍候重试",
        "too many requests",
        "flood_wait",
        "rate limit",
    ]
    .iter()
    .any(|s| lower.contains(s));
    let timed_retry = WAIT.is_match(text)
        && ["重试", "再试", "后再", "稍后", "等待"]
            .iter()
            .any(|s| text.contains(s))
        && !["处理中", "正在搜索", "正在处理", "正在获取", "正在发送"]
            .iter()
            .any(|s| text.contains(s));
    if !explicit && !timed_retry {
        return None;
    }
    Some(
        WAIT.captures(text)
            .and_then(|c| {
                c[1].parse::<u64>()
                    .ok()
                    .map(|v| v.saturating_mul(if &c[2] == "秒" { 1 } else { 60 }))
            })
            .unwrap_or(60)
            .clamp(1, 3600),
    )
}

pub fn claim_rate_wait(text: &str) -> Option<u64> {
    if !["暂时", "稍后", "稍候", "请重试", "频繁", "限制", "请稍"]
        .iter()
        .any(|s| text.contains(s))
    {
        return None;
    }
    Some(
        WAIT.captures(text)
            .and_then(|c| {
                c[1].parse::<u64>()
                    .ok()
                    .map(|n| n.saturating_mul(if &c[2] == "秒" { 1 } else { 60 }))
            })
            .unwrap_or(60)
            .clamp(1, u32::MAX as u64),
    )
}

pub fn no_search_results(text: &str) -> bool {
    [
        "未找到相关结果",
        "没有找到相关结果",
        "未找到相关文件",
        "没有搜索结果",
        "暂无搜索结果",
        "没有相关结果",
    ]
    .iter()
    .any(|s| text.contains(s))
}

pub fn search_end(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "已是最后一页",
        "已经是最后一页",
        "已经到最后一页",
        "已到最后一页",
        "已到达最后一页",
        "已经到达最后一页",
        "当前是最后一页",
        "最后一页了",
        "最后一页啦",
        "已经是末页",
        "已是末页",
        "没有下一页",
        "没有更多结果",
        "没有更多搜索结果",
        "暂无更多结果",
        "没有更多了",
        "no more results",
        "already on the last page",
    ]
    .iter()
    .any(|s| lower.contains(s))
}

pub fn callback(message: &grammers_client::message::Message, needles: &[&str]) -> Option<Vec<u8>> {
    callback_button(message, needles).map(|(_, data)| data)
}

pub fn callback_button(
    message: &grammers_client::message::Message,
    needles: &[&str],
) -> Option<(String, Vec<u8>)> {
    if let Some(tl::enums::ReplyMarkup::ReplyInlineMarkup(markup)) = message.reply_markup() {
        for row in markup.rows {
            let tl::enums::KeyboardButtonRow::Row(row) = row;
            for button in row.buttons {
                if let tl::enums::KeyboardButton::Callback(button) = button
                    && needles.iter().any(|s| button.text.contains(s))
                {
                    return Some((button.text, button.data));
                }
            }
        }
    }
    None
}
