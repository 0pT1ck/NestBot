pub mod application;
pub mod config;
pub mod domain;
pub mod interfaces;
pub mod scheduler;
pub mod storage;
pub mod telegram;
pub mod telemetry;

pub async fn serve(config: config::Config, store: storage::Store) -> anyhow::Result<()> {
    let app = application::App::new(config, store)?;
    app.store.recover().await?;
    // Validate authentication before starting network consumers or accepting jobs.
    let web = if app.config.web.enabled {
        Some(interfaces::web::WebState::new(app.clone()).await?)
    } else {
        None
    };
    let mut services = tokio::task::JoinSet::new();
    services.spawn(scheduler::worker(app.clone(), false));
    services.spawn(scheduler::worker(app.clone(), true));
    services.spawn(interfaces::control::serve(app.clone()));
    if app.bot.is_some() {
        services.spawn(interfaces::bot::run(app.clone()));
    }
    if let Some(web) = web {
        services.spawn(interfaces::web::serve(web));
    }
    tracing::info!(
        event = "service_started",
        version = env!("CARGO_PKG_VERSION")
    );
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let signal = async {
        #[cfg(unix)]
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{}};
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    let result = tokio::select! {
        _=signal=>Ok(()),
        service=services.join_next()=>match service{Some(Ok(Err(error)))=>Err(error),_=>Err(anyhow::anyhow!("service_stopped"))},
    };
    app.shutdown.cancel();
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(15));
    tokio::pin!(deadline);
    loop {
        tokio::select! {_= &mut deadline=>{services.abort_all();break;},service=services.join_next()=>{if service.is_none(){break;}}}
    }
    tracing::info!(event = "service_stopped");
    result
}
