use crate::{
    application::App,
    config::Config,
    interfaces::control,
    storage::{Store, vault::Vault},
};
use std::{sync::Arc, time::Duration};

pub(crate) fn fixture() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().unwrap();
    let vault = Arc::new(
        Vault::open(&dir.path().join("master.key"), "synthetic-restart-password").unwrap(),
    );
    let store = Store::open(&dir.path().join("test.sqlite"), vault, 1024, 20).unwrap();
    let mut config = Config::default();
    config.paths.data = dir.path().join("data");
    config.paths.run = dir.path().join("run");
    config.paths.cache = dir.path().join("cache");
    config.web.enabled = false;
    config.telegram.allowed_users = vec![42];
    config.telegram.bot_token_env = "NESTBOT_RESTART_TEST_DISABLED_BOT".into();
    config.create_dirs().unwrap();
    (dir, App::new(config, store).unwrap())
}

#[tokio::test]
async fn control_restart_replies_before_shutdown_and_services_can_start_again() {
    let (_dir, app) = fixture();
    app.store
        .set_preference("target", "synthetic-target")
        .await
        .unwrap();
    {
        let service = tokio::spawn(super::serve_once(app.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if control::request(&app.config.paths.run, control::Action::Status)
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let response = control::request(&app.config.paths.run, control::Action::Restart)
            .await
            .unwrap();
        assert_eq!(response["restarting"], true);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), service)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
        );
        assert!(!app.config.paths.run.join("control.json").exists());
        assert_eq!(app.target().await.unwrap(), "synthetic-target");
    }
    let replacement = App::new((*app.config).clone(), app.store.clone()).unwrap();
    assert!(!replacement.restart.is_cancelled());
    assert!(!replacement.shutdown.is_cancelled());
    let service = tokio::spawn(super::serve_once(replacement.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if control::request(&replacement.config.paths.run, control::Action::Status)
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    replacement.restart.cancel();
    assert!(service.await.unwrap().unwrap());
}
