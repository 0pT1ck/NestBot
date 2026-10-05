use clap::Parser;

fn main() -> std::process::ExitCode {
    let cli = nestbot::interfaces::cli::Cli::parse();
    if let Some(path) = &cli.env_file
        && nestbot::interfaces::cli::load_env_file(path).is_err()
    {
        eprintln!("启动失败：invalid_env_file");
        return std::process::ExitCode::FAILURE;
    }
    nestbot::telemetry::init();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .expect("runtime initialization");
    match runtime.block_on(nestbot::interfaces::cli::run(cli)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let code = nestbot::telemetry::safe_error(&error);
            eprintln!("操作失败：{code}");
            tracing::error!(event = "command_failed", error_code = code);
            std::process::ExitCode::FAILURE
        }
    }
}
