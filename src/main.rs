use std::{fs::OpenOptions, io::Write, path::PathBuf};

use anyhow::{Context, Result, bail};
use nestbot::{config::Config, control, logging, paths::Paths};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let code = match execute().await {
        Ok(code) => code,
        Err(e) => {
            tracing::error!(error = %e, "operation failed");
            eprintln!("Error: {e:#}");
            1
        }
    };
    if code != 0 {
        std::process::exit(code);
    }
}

async fn execute() -> Result<i32> {
    let mut args = std::env::args_os().skip(1);
    let mut root = None;
    let mut command = None;
    while let Some(arg) = args.next() {
        if arg == "--root" {
            if root.is_some() {
                bail!("--root can only appear once");
            }
            root = Some(PathBuf::from(
                args.next().context("--root requires a path")?,
            ));
        } else if arg == "--version" {
            println!(
                "nestbot {} {}-{}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::ARCH,
                std::env::consts::OS
            );
            return Ok(0);
        } else if arg == "--help" || arg == "-h" {
            println!(
                "nestbot [--root PATH] init|check|login|run|status|stop\nDevelopment builds require --root. All local files stay in that directory."
            );
            return Ok(0);
        } else if command.replace(arg).is_some() {
            bail!("unexpected argument; use --help");
        }
    }
    let command = command.context("expected init|check|login|run|status|stop")?;
    let paths = Paths::new(root)?;
    match command.to_str().context("invalid command")? {
        "status" => {
            let running = control::status(&paths)?;
            println!("{}", if running { "running" } else { "stopped" });
            Ok(if running { 0 } else { 3 })
        }
        "stop" => {
            control::stop(&paths).await?;
            Ok(0)
        }
        "init" => {
            paths.prepare()?;
            let _lock = paths.lock()?;
            let file = paths.file("config/config.toml")?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(file) {
                Ok(mut file) => {
                    file.write_all(include_bytes!("../config/config.example.toml"))?;
                    file.sync_all()?;
                    println!(
                        "Created config/config.toml. Fill credentials locally, then run login."
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    println!("Existing configuration preserved.")
                }
                Err(e) => return Err(e.into()),
            }
            Ok(0)
        }
        "check" => {
            paths.prepare()?;
            let config = Config::load(&paths)?;
            nestbot::rules::Rules::load(&paths, config.max_rules)?;
            println!(
                "Local configuration and rules are valid. Telegram authorization is not checked."
            );
            Ok(0)
        }
        "login" => {
            paths.prepare()?;
            let _lock = paths.lock()?;
            let config = Config::load(&paths)?;
            nestbot::telegram::Telegram::login(&config, &paths).await?;
            Ok(0)
        }
        "run" => {
            paths.prepare()?;
            let _lock = paths.lock()?;
            logging::init(&paths)?;
            control::clear_stop(&paths)?;
            let config = Config::load(&paths)?;
            tracing::info!("starting realtime monitor");
            let result = nestbot::app::run(&config, &paths).await;
            control::clear_stop(&paths)?;
            tracing::info!("monitor stopped");
            result?;
            Ok(0)
        }
        _ => bail!("unknown command; use --help"),
    }
}
