use std::{
    fs,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};

use crate::paths::{Paths, atomic_write};

pub fn status(paths: &Paths) -> Result<bool> {
    if !paths.root.join("run").exists() {
        return Ok(false);
    }
    paths.is_locked()
}

pub async fn stop(paths: &Paths) -> Result<()> {
    if !status(paths)? {
        println!("stopped");
        return Ok(());
    }
    atomic_write(&paths.file("run/stop.request")?, b"stop\n")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if !status(paths)? {
            println!("stopped");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("instance has not stopped within 30 seconds; no process was killed")
}

pub fn clear_stop(paths: &Paths) -> Result<()> {
    let file = paths.file("run/stop.request")?;
    if file.exists() {
        fs::remove_file(file)?;
    }
    Ok(())
}

pub async fn wait_for_stop(paths: &Paths) -> Result<()> {
    let path = paths.file("run/stop.request")?;
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        loop {
            tokio::select! {
                result = tokio::signal::ctrl_c() => { result?; break; }
                _ = term.recv() => break,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if path.try_exists()? { break; }
                }
            }
        }
    }
    #[cfg(not(unix))]
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => { result?; break; }
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if path.try_exists()? { break; }
            }
        }
    }
    Ok(())
}
