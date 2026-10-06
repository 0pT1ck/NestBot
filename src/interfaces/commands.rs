//! Python's command and caption rules, shared by the Bot and parity tests.
use regex::Regex;
use std::sync::LazyLock;
static KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w\-.:：]+$").unwrap());
static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"#[^\s#，。；、！？!?,.]+").unwrap());

pub fn looks_like_key(text: &str) -> bool {
    let t = text.trim();
    (6..=128).contains(&t.chars().count())
        && !t.starts_with(['/', ' '])
        && !t.starts_with("http")
        && KEY.is_match(t)
}
pub fn fetch_args(text: &str) -> (Vec<String>, bool, bool) {
    let (mut keys, mut redo, mut dry) = (vec![], false, false);
    for token in text.split_whitespace() {
        match token {
            "--redo" | "-r" => redo = true,
            "--dry" | "--dry-run" | "-d" => dry = true,
            _ => keys.push(token.into()),
        }
    }
    (keys, redo, dry)
}
pub fn search_args(text: &str) -> (String, Option<u32>, bool) {
    let mut words = text.split_whitespace().collect::<Vec<_>>();
    let resume = words
        .last()
        .is_some_and(|s| s.eq_ignore_ascii_case("continue"));
    if resume {
        words.pop();
    }
    let pages = words
        .last()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| (1..=50).contains(n));
    if pages.is_some() {
        words.pop();
    }
    (words.join(" "), pages, resume)
}
pub fn tags(text: &str) -> Vec<String> {
    TAG.find_iter(text).map(|m| m.as_str().into()).collect()
}
pub fn caption(original: &str, tags: &[String]) -> String {
    let base = original.trim();
    if base.is_empty() {
        tags.join(" ")
    } else {
        format!("{base}\n{}", tags.join(" "))
    }
}
