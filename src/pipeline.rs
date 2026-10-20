use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, ensure};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn channel_id(value: &str) -> Result<i64> {
    let id: i64 = value.parse()?;
    let id = if id < -1_000_000_000_000 {
        -(id + 1_000_000_000_000)
    } else {
        id
    };
    ensure!(
        id > 0 && id < 1_000_000_000_000,
        "expected a positive channel ID or -100… Bot API ID"
    );
    Ok(id)
}

pub fn message_link(chat: i64, message: i32) -> Result<String> {
    ensure!(
        chat > 0 && chat < 1_000_000_000_000 && message > 0,
        "invalid message reference"
    );
    Ok(format!("https://t.me/c/{chat}/{message}"))
}

pub fn is_live(date: i64, boundary: i64, now: i64, ttl: u64) -> bool {
    date > boundary && date <= now + 5 && now.saturating_sub(date) <= ttl as i64
}

pub struct Seen {
    capacity: usize,
    ttl: Duration,
    keys: HashSet<(i64, i32)>,
    order: VecDeque<((i64, i32), Instant)>,
}
impl Seen {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            ttl,
            keys: HashSet::new(),
            order: VecDeque::new(),
        }
    }
    pub fn insert(&mut self, key: (i64, i32), now: Instant) -> bool {
        while self
            .order
            .front()
            .is_some_and(|(_, t)| now.saturating_duration_since(*t) >= self.ttl)
        {
            if let Some((key, _)) = self.order.pop_front() {
                self.keys.remove(&key);
            }
        }
        if self.keys.contains(&key) {
            return false;
        }
        if self.keys.len() == self.capacity
            && let Some((key, _)) = self.order.pop_front()
        {
            self.keys.remove(&key);
        }
        self.keys.insert(key);
        self.order.push_back((key, now));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_and_links() {
        assert_eq!(channel_id("-1001234567890").unwrap(), 1234567890);
        assert_eq!(message_link(123, 45).unwrap(), "https://t.me/c/123/45");
        assert!(channel_id("-123").is_err());
        assert!(message_link(-100123, 45).is_err());
    }
    #[test]
    fn no_history_at_boundary() {
        assert!(!is_live(100, 100, 101, 60));
        assert!(is_live(101, 100, 102, 60));
        assert!(!is_live(101, 100, 200, 60));
        assert!(!is_live(120, 100, 101, 60));
    }
    #[test]
    fn dedup_is_bounded_and_expires() {
        let mut seen = Seen::new(2, Duration::from_secs(10));
        let t = Instant::now();
        assert!(seen.insert((1, 1), t));
        assert!(!seen.insert((1, 1), t));
        assert!(seen.insert((1, 2), t));
        assert!(seen.insert((1, 3), t));
        assert_eq!(seen.keys.len(), 2);
        assert!(seen.insert((1, 1), t));
        assert!(seen.insert((1, 1), t + Duration::from_secs(11)));
    }
}
