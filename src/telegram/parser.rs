use crate::domain::Entry;
use grammers_tl_types as tl;
use regex::Regex;
use std::{collections::HashMap, sync::LazyLock};

static PAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"第\s*(\d+)\s*页").unwrap());
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)\s*[（(]\s*(https?://[^)）]+)\s*[)）]\s*$").unwrap());
static WAIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)\s*(秒|分钟|分)").unwrap());

#[derive(Default)]
pub struct SearchPage {
    pub page: Option<u32>,
    pub entries: Vec<Entry>,
}

fn label<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let pos = line.find(name)? + name.len();
    let value = line[pos..].trim_start();
    value
        .strip_prefix(':')
        .or_else(|| value.strip_prefix('：'))
        .map(str::trim)
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
        if let Some(key) = label(line, "密钥").filter(|s| !s.is_empty()) {
            result.entries.push(Entry {
                key: key.into(),
                ..Default::default()
            });
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
        } else if let Some(count) = label(line, "文件个数")
            && let Some(entry) = result.entries.last_mut()
        {
            entry.file_count = count
                .chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok();
        }
        if let Some(c) = PAGE.captures(line) {
            result.page = c[1].parse().ok();
        }
    }
    for entry in &mut result.entries {
        entry.page = result.page;
    }
    result
}

pub fn rate_wait(text: &str) -> Option<u64> {
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
                    .map(|v| v.saturating_mul(if &c[2] == "秒" { 1 } else { 60 }))
            })
            .unwrap_or(60)
            .clamp(1, 3600),
    )
}

pub fn callback(message: &grammers_client::message::Message, needles: &[&str]) -> Option<Vec<u8>> {
    if let Some(tl::enums::ReplyMarkup::ReplyInlineMarkup(markup)) = message.reply_markup() {
        for row in markup.rows {
            let tl::enums::KeyboardButtonRow::Row(row) = row;
            for button in row.buttons {
                if let tl::enums::KeyboardButton::Callback(button) = button
                    && needles.iter().any(|s| button.text.contains(s))
                {
                    return Some(button.data);
                }
            }
        }
    }
    None
}
