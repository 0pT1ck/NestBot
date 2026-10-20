use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use tracing_subscriber::fmt::MakeWriter;

use crate::paths::Paths;

const LIMIT: u64 = 2 * 1024 * 1024;

struct State {
    file: Option<File>,
    size: u64,
    current: PathBuf,
    previous: PathBuf,
}
#[derive(Clone)]
struct Writer(Arc<Mutex<State>>);

fn open(path: &PathBuf) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| io::Error::other("log lock poisoned"))?;
        if state.size.saturating_add(bytes.len() as u64) > LIMIT {
            state.file.take();
            if state.previous.exists() {
                std::fs::remove_file(&state.previous)?;
            }
            std::fs::rename(&state.current, &state.previous)?;
            state.file = Some(open(&state.current)?);
            state.size = 0;
        }
        let count = bytes.len().min(LIMIT as usize);
        state
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log unavailable"))?
            .write_all(&bytes[..count])?;
        state.size += count as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| io::Error::other("log lock poisoned"))?;
        if let Some(file) = state.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}
impl<'a> MakeWriter<'a> for Writer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

pub fn init(paths: &Paths) -> Result<()> {
    let current = paths.file("logs/nestbot.log")?;
    let previous = paths.file("logs/nestbot.previous.log")?;
    let file = open(&current)?;
    let size = file.metadata()?.len();
    let writer = Writer(Arc::new(Mutex::new(State {
        file: Some(file),
        size,
        current,
        previous,
    })));
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(writer)
        .try_init()
        .map_err(|_| anyhow::anyhow!("failed to initialize logging"))?;
    Ok(())
}
