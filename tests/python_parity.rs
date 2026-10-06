use nestbot::{
    application::App,
    config::Config,
    domain::{ClaimRecord, Entry, JobPayload, TransferMode},
    interfaces::commands,
    storage::{Store, vault::Vault},
    telegram::parser,
};
use serde_json::{Value, json};
use std::sync::Arc;

fn fixture() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let vault =
        Arc::new(Vault::open(&dir.path().join("master.key"), "synthetic-password").unwrap());
    let store = Store::open(&dir.path().join("test.sqlite"), vault, 1024, 20).unwrap();
    (dir, store)
}

#[test]
fn original_python_function_outputs_match_all_48_synthetic_cases() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/python-behavior.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let input = &case["input"];
        let actual = match case["kind"].as_str().unwrap() {
            "parser" => {
                let page = parser::parse_search(input.as_str().unwrap(), &Default::default());
                let mut value = serde_json::to_value(&page).unwrap();
                value["is_result"] = json!(page.is_result());
                value
            }
            "search_args" => {
                let (keyword, pages, _) = commands::search_args(input.as_str().unwrap());
                json!([keyword, pages])
            }
            "fetch_args" => {
                let (keys, redo, dry) = commands::fetch_args(input.as_str().unwrap());
                json!([keys, redo, dry])
            }
            "key" => json!(commands::looks_like_key(input.as_str().unwrap())),
            "tags" => json!(commands::tags(input.as_str().unwrap())),
            "caption" => {
                let tags: Vec<String> = serde_json::from_value(input["tags"].clone()).unwrap();
                let caption = commands::caption(input["original"].as_str().unwrap(), &tags);
                json!([caption, caption.chars().count() <= 1024])
            }
            "payload" => json!(
                serde_json::from_value::<Entry>(input.clone())
                    .unwrap()
                    .payload()
            ),
            "complete" => json!(
                serde_json::from_value::<ClaimRecord>(input["record"].clone())
                    .unwrap()
                    .complete(serde_json::from_value(input["expected"].clone()).unwrap())
            ),
            "claim_wait" => json!(parser::claim_rate_wait(input.as_str().unwrap())),
            other => panic!("unknown reference case {other}"),
        };
        assert_eq!(actual, case["expected"], "{} input {}", case["kind"], input);
    }
}

#[test]
fn cli_search_defaults_to_one_page_and_accepts_explicit_all() {
    use clap::Parser;
    use nestbot::interfaces::cli::{Cli, Command};
    let cli = Cli::try_parse_from(["nestbot", "search", "synthetic"]).unwrap();
    assert!(matches!(
        cli.command,
        Command::Search {
            pages: Some(1),
            all: false,
            ..
        }
    ));
    let cli = Cli::try_parse_from(["nestbot", "search", "synthetic", "--all"]).unwrap();
    assert!(matches!(cli.command, Command::Search { all: true, .. }));
}

#[tokio::test]
async fn original_database_upgrades_in_place_and_old_job_json_remains_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.sqlite");
    let vault =
        Arc::new(Vault::open(&dir.path().join("master.key"), "synthetic-password").unwrap());
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(include_str!("../migrations/001_initial.sql"))
        .unwrap();
    drop(connection);
    let payload: JobPayload = serde_json::from_value(json!({"kind":"incoming","source_chat":42,"message_ids":[101],"mode":"copy","target":"-100123456789","caption":null})).unwrap();
    assert!(matches!(
        payload,
        JobPayload::Incoming {
            caption_message: None,
            ..
        }
    ));
    let store = Store::open(&path, vault.clone(), 1024, 20).unwrap();
    store
        .set_preference("synthetic-sentinel", "retained")
        .await
        .unwrap();
    drop(store);
    let store = Store::open(&path, vault, 1024, 20).unwrap();
    assert_eq!(
        store
            .preference("synthetic-sentinel")
            .await
            .unwrap()
            .as_deref(),
        Some("retained")
    );
    assert_eq!(
        store
            .call(|c| Ok(c.query_row(
                "SELECT count(*) FROM pragma_table_info('claim_inbox') WHERE name='group_id'",
                [],
                |r| r.get::<_, u32>(0)
            )?))
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn captions_obey_flags_and_album_tags_follow_the_caption_bearing_message() {
    let (_dir, store) = fixture();
    let app = App::new(Config::default(), store.clone()).unwrap();
    let payload = JobPayload::Transfer {
        options: nestbot::domain::TransferOptions {
            keep_caption: false,
            tag_key: true,
            ..Default::default()
        },
        keys: vec!["synthetic-key".into()],
        batch: None,
        start: 1,
        end: None,
        mode: TransferMode::Deep,
        target: "-100123456789".into(),
        redo: false,
        dry_run: false,
    };
    store.enqueue(payload, None, None).await.unwrap();
    let job = store.next_job().await.unwrap().unwrap();
    assert_eq!(
        nestbot::telegram::transfer::caption(&job, "old caption", Some("synthetic-key")),
        "🔑 synthetic-key"
    );
    nestbot::interfaces::bot::remember_media_carrier(
        &app,
        42,
        "-100123456789",
        &[(101, 201), (102, 202)],
        "second caption",
        Some(102),
    )
    .await
    .unwrap();
    let recent: Value =
        serde_json::from_str(&store.preference("media:42").await.unwrap().unwrap()).unwrap();
    assert_eq!(recent["messages"][0]["carrier"], false);
    assert_eq!(recent["messages"][1]["carrier"], true);
    assert_eq!(recent["messages"][1]["caption"], "second caption");
}

#[tokio::test]
async fn legacy_completion_is_read_and_reopened_when_expected_count_grows() {
    use sha2::{Digest, Sha256};
    let (_dir, store) = fixture();
    let payload = "synthetic-old-progress";
    let id = format!("{:x}", Sha256::digest(payload.as_bytes()))[..32].to_string();
    let body = store
        .vault
        .encrypt(
            &format!("legacy:{id}"),
            br#"{"status":"done","files":2,"file_ids":[["d",7],["d",8]]}"#,
        )
        .unwrap();
    store
        .call(move |c| {
            c.execute(
                "INSERT INTO legacy_claims VALUES(?1,?2)",
                rusqlite::params![id, body],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut record = store.claim_record(payload).await.unwrap();
    assert!(record.complete(Some(2)));
    assert!(!record.complete(Some(3)));
    assert!(record.contains("d:7"));
    record.add("d:9");
    record.status = "done".into();
    store.save_claim(payload, &record).await.unwrap();
    assert!(store.claim_record(payload).await.unwrap().complete(Some(3)));
    assert_eq!(store.claim_record(payload).await.unwrap().files, 3);
}

fn transfer(keys: Vec<String>) -> JobPayload {
    JobPayload::Transfer {
        options: Default::default(),
        keys,
        batch: None,
        start: 1,
        end: None,
        mode: TransferMode::Copy,
        target: "-100123456789".into(),
        redo: false,
        dry_run: false,
    }
}

#[tokio::test]
async fn completed_keys_skip_without_connecting_and_failed_keys_do_not_stop_the_batch() {
    let (_dir, store) = fixture();
    let app = App::new(Config::default(), store.clone()).unwrap();
    let mut record = ClaimRecord {
        status: "done".into(),
        ..Default::default()
    };
    record.add("d:1");
    store.save_claim("synthetic-done", &record).await.unwrap();
    let id = store
        .enqueue(
            transfer(vec![
                "synthetic-done".into(),
                "synthetic-missing-one".into(),
                "synthetic-missing-one".into(),
                "synthetic-missing-two".into(),
            ]),
            None,
            None,
        )
        .await
        .unwrap();
    let job = store.next_job().await.unwrap().unwrap();
    app.execute(&job, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let report = store.report(&id).await.unwrap();
    assert_eq!(report.skipped_keys, 1);
    assert_eq!(report.failed_keys, 2);
    assert!(
        store
            .claim_record("synthetic-done")
            .await
            .unwrap()
            .complete(None)
    );
}

#[tokio::test]
async fn selection_keeps_the_python_new_search_scope_while_database_sequences_stay_stable() {
    let (_dir, store) = fixture();
    let a = Entry {
        key: "synthetic-a".into(),
        ..Default::default()
    };
    let b = Entry {
        key: "synthetic-b".into(),
        ..Default::default()
    };
    let c = Entry {
        key: "synthetic-c".into(),
        ..Default::default()
    };
    let batch = store
        .save_page("synthetic", 1, Some(10), vec![a.clone(), b.clone()])
        .await
        .unwrap();
    store
        .save_page("synthetic", 2, Some(10), vec![b.clone(), c.clone()])
        .await
        .unwrap();
    store
        .select_page("synthetic-job", &batch, &[b, c])
        .await
        .unwrap();
    let selected = store
        .selected_entries("synthetic-job", &batch, 1, 100)
        .await
        .unwrap();
    assert_eq!(
        selected.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(store.entries(&batch, 1, 100).await.unwrap()[0].1.key, a.key);
}

#[tokio::test]
async fn callbacks_fit_telegram_limit_are_private_and_expire() {
    let (_dir, store) = fixture();
    let token = store
        .callback(
            42,
            &json!({"payload":transfer(vec!["synthetic-secret".into()])}),
        )
        .await
        .unwrap();
    assert!(token.len() <= 64);
    let id = token.strip_prefix("C:").unwrap();
    assert!(store.callback_value(43, id).await.unwrap().is_none());
    assert!(store.callback_value(42, id).await.unwrap().is_some());
    store
        .call(|c| {
            c.execute("UPDATE bot_callbacks SET expires=0", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(store.callback_value(42, id).await.unwrap().is_none());
}

#[tokio::test]
async fn albums_are_read_as_a_group_and_send_intents_commit_atomically() {
    let (_dir, store) = fixture();
    store
        .inbox_push_group("job", "claim", 1, "p:1", None)
        .await
        .unwrap();
    for id in 2..=11 {
        store
            .inbox_push_group("job", "claim", id, &format!("p:{id}"), Some(100))
            .await
            .unwrap();
    }
    assert_eq!(store.inbox_next("job", "claim").await.unwrap(), vec![1]);
    store.inbox_done("job", "claim", 1).await.unwrap();
    assert_eq!(
        store.inbox_next("job", "claim").await.unwrap(),
        (2..=11).collect::<Vec<_>>()
    );
    store
        .transfer_intent("scope", "p:2", "job", false)
        .await
        .unwrap();
    assert!(
        store
            .album_intent("scope", &["p:3".into(), "p:2".into()], "job", false)
            .await
            .is_err()
    );
    assert!(
        store
            .transfer_status("scope", "p:3")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn bot_flags_search_limits_and_batch_continue_use_python_rules() {
    let (_dir, store) = fixture();
    let config = Config {
        default_target: "-100123456789".into(),
        ..Default::default()
    };
    let app = App::new(config, store.clone()).unwrap();
    nestbot::interfaces::bot::command(&app, 42, "/fetch -r synthetic-key -d", None)
        .await
        .unwrap();
    let job = store.next_job().await.unwrap().unwrap();
    assert!(matches!(
        job.payload,
        JobPayload::Transfer {
            redo: true,
            dry_run: true,
            ..
        }
    ));
    store
        .finish(&job.summary.id, "completed", None, None)
        .await
        .unwrap();
    nestbot::interfaces::bot::command(&app, 42, "/search synthetic 3 CONTINUE", None)
        .await
        .unwrap();
    let job = store.next_job().await.unwrap().unwrap();
    assert!(matches!(
        job.payload,
        JobPayload::Search {
            pages: Some(3),
            resume: true,
            ..
        }
    ));
    store
        .finish(&job.summary.id, "completed", None, None)
        .await
        .unwrap();
    let batch = store
        .save_page(
            "synthetic-batch",
            1,
            None,
            (1..=30)
                .map(|n| Entry {
                    key: format!("synthetic-batch-{n}"),
                    ..Default::default()
                })
                .collect(),
        )
        .await
        .unwrap();
    store.set_preference("last_batch", &batch).await.unwrap();
    store
        .set_preference("last_batch_start", "25")
        .await
        .unwrap();
    nestbot::interfaces::bot::command(&app, 42, "/batch continue", None)
        .await
        .unwrap();
    let job = store.next_job().await.unwrap().unwrap();
    assert!(matches!(
        job.payload,
        JobPayload::Transfer { start: 25, .. }
    ));
    if let JobPayload::Transfer { options, .. } = job.payload {
        assert!(options.confirmed);
    } else {
        panic!("expected transfer");
    }
}

#[tokio::test]
async fn idle_stop_is_harmless_and_status_reports_the_queue_mode_and_target() {
    let (_dir, store) = fixture();
    let app = App::new(
        Config {
            default_target: "-100123456789".into(),
            ..Default::default()
        },
        store,
    )
    .unwrap();
    assert!(
        nestbot::interfaces::bot::command(&app, 42, "/stop", None)
            .await
            .is_ok()
    );
    let text = nestbot::interfaces::bot::command(&app, 42, "/status", None)
        .await
        .unwrap();
    assert!(text.contains("空闲"));
    assert!(text.contains("0 个等待"));
    assert!(text.contains("copy"));
    assert!(text.contains("-100123456789"));
}
