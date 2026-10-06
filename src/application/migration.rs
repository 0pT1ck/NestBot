use crate::{
    domain::Entry,
    storage::{Store, vault::decode_legacy},
};
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize)]
struct LegacyBatch {
    keyword: String,
    entries: Vec<Entry>,
}

pub async fn import_legacy(
    store: &Store,
    root: &Path,
    password: &str,
) -> anyhow::Result<(u32, u32)> {
    let mut batches = 0;
    let mut entries = 0;
    let directory = root.join("keys");
    if directory.exists() {
        let mut paths = std::fs::read_dir(directory)?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        for path in paths {
            if path.extension().and_then(|v| v.to_str()) != Some("bin") {
                continue;
            }
            anyhow::ensure!(
                std::fs::metadata(&path)?.len() <= 16 * 1024 * 1024,
                "legacy_batch_too_large"
            );
            let blob = std::fs::read(&path)?;
            let clear = decode_legacy(&blob, password)?;
            let batch: LegacyBatch = serde_json::from_slice(&clear)?;
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                store
                    .set_preference(
                        &format!("batch_alias:{name}"),
                        &store.vault.index("keyword", &batch.keyword),
                    )
                    .await?;
            }
            store.save_page(&batch.keyword, 0, None, vec![]).await?;
            // Insert in small transactions; stable sequence numbers survive reruns.
            for chunk in batch.entries.chunks(64) {
                store
                    .save_page(
                        &batch.keyword,
                        chunk.iter().filter_map(|e| e.page).max().unwrap_or(0),
                        None,
                        chunk.to_vec(),
                    )
                    .await?;
            }
            batches += 1;
            entries += batch.entries.len() as u32;
        }
    }
    let bot = root.join("bot.json");
    if bot.exists() {
        anyhow::ensure!(
            std::fs::metadata(&bot)?.len() < 1024 * 1024,
            "legacy_state_too_large"
        );
        let prefs: serde_json::Value = serde_json::from_slice(&std::fs::read(bot)?)?;
        if let Some(last) = prefs.get("last_batch")
            && let Some(name) = last.get("file").and_then(|v| v.as_str())
        {
            if let Some(id) = store.preference(&format!("batch_alias:{name}")).await? {
                store.set_preference("last_batch", &id).await?;
            }
            store
                .set_preference(
                    "last_batch_start",
                    &last
                        .get("start")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(1)
                        .max(1)
                        .to_string(),
                )
                .await?;
        }
        if let Some(last) = prefs.get("last_search")
            && let Some(hash) = last
                .get("keyword_hash")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        {
            use sha2::{Digest, Sha256};
            let mut offset = 0;
            'find: loop {
                let batches = store.batches(64, offset).await?;
                if batches.is_empty() {
                    break;
                }
                for batch in batches {
                    let keyword = store.batch_keyword(&batch.id).await?;
                    if format!("{:x}", Sha256::digest(keyword.as_bytes())).starts_with(hash) {
                        store
                            .save_page(
                                &keyword,
                                last.get("page").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                last.get("result_msg_id")
                                    .and_then(|v| v.as_i64())
                                    .and_then(|n| i32::try_from(n).ok()),
                                vec![],
                            )
                            .await?;
                        break 'find;
                    }
                }
                offset += 64;
            }
        }
        if let Some(target) = prefs.get("target_chat").and_then(|v| v.as_str()) {
            store.set_preference("target", target).await?;
        }
        if let Some(mode) = prefs
            .get("mode")
            .and_then(|v| v.as_str())
            .filter(|v| ["copy", "deep"].contains(v))
        {
            store.set_preference("mode", mode).await?;
        }
    }
    let state = root.join("state.json");
    if state.exists() {
        anyhow::ensure!(
            std::fs::metadata(&state)?.len() <= 16 * 1024 * 1024,
            "legacy_state_too_large"
        );
        let data: serde_json::Value = serde_json::from_slice(&std::fs::read(state)?)?;
        if let Some(claims) = data.get("claims").and_then(|v| v.as_object()) {
            for (id, claim) in claims {
                if id.len() != 32 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                let body = store
                    .vault
                    .encrypt(&format!("legacy:{id}"), &serde_json::to_vec(claim)?)?;
                let id = id.clone();
                store
                    .call(move |c| {
                        c.execute(
                            "INSERT OR IGNORE INTO legacy_claims VALUES(?1,?2)",
                            rusqlite::params![id, body],
                        )?;
                        Ok(())
                    })
                    .await?;
            }
        }
    }
    Ok((batches, entries))
}
