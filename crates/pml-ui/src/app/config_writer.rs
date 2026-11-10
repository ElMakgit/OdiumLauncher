//! Background writer for `config.json`.
//!
//! The UI thread used to serialise the config and fsync it inside `persist()`,
//! which ran from sliders, source switches and the close handler. A spinning
//! disk could stall a frame for tens of milliseconds. The writer moves the
//! `serde_json` serialisation, the temp-file write and the fsync to a worker
//! thread, and coalesces bursts: dragging a slider produces one disk write with
//! the final value instead of one per frame.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use pml_config::Config;

enum Command {
    Save(Box<Config>),
    Stop { save_last: bool },
}

/// Owns the worker thread that writes `config.json`.
pub(crate) struct ConfigWriter {
    path: PathBuf,
    tx: Option<Sender<Command>>,
    handle: Option<JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
}

impl ConfigWriter {
    pub(crate) fn spawn(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let error = Arc::new(Mutex::new(None));
        let worker_error = Arc::clone(&error);
        let worker_path = path.clone();
        let handle = std::thread::Builder::new()
            .name("config-writer".to_owned())
            .spawn(move || run(worker_path, rx, worker_error))
            .ok();
        if handle.is_none() {
            tracing::warn!(
                path = %path.display(),
                "config writer thread failed to start; saves fall back to the UI thread"
            );
        }
        Self {
            path,
            tx: handle.as_ref().map(|_| tx),
            handle,
            error,
        }
    }

    /// Queues a snapshot. The newest snapshot wins, so a burst of edits costs
    /// one write. When the worker is unavailable the snapshot is written here,
    /// which keeps the launcher usable on a host that refuses new threads.
    pub(crate) fn save(&self, config: Config) {
        match &self.tx {
            Some(tx) => {
                if let Err(error) = tx.send(Command::Save(Box::new(config))) {
                    // The worker is gone: write on this thread so the edit is
                    // not lost.
                    if let Command::Save(config) = error.0 {
                        write_config(&self.path, &config, &self.error);
                    }
                }
            }
            None => write_config(&self.path, &config, &self.error),
        }
    }

    /// Returns the last write error once, so the UI can show it exactly once
    /// instead of repeating the same dialog every frame.
    pub(crate) fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Stops the worker and writes the pending snapshot. Called while closing
    /// the window so the last edits are durable before the process exits.
    pub(crate) fn flush(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Command::Stop { save_last: true });
            drop(tx);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Stops the worker without writing. The data root was just deleted by the
    /// uninstaller, and a final save would recreate it.
    pub(crate) fn discard(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Command::Stop { save_last: false });
            drop(tx);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ConfigWriter {
    fn drop(&mut self) {
        // A normal drop (for example a crash-free window close that skipped
        // `flush`) still gets the latest snapshot written.
        self.flush();
    }
}

fn run(path: PathBuf, rx: Receiver<Command>, error: Arc<Mutex<Option<String>>>) {
    let mut pending: Option<Box<Config>> = None;
    let mut keep_last = true;
    loop {
        let mut stop = false;
        match rx.recv() {
            Ok(Command::Save(config)) => pending = Some(config),
            Ok(Command::Stop { save_last }) => {
                keep_last = save_last;
                stop = true;
            }
            Err(_) => stop = true,
        }
        // Drain everything already queued: the last snapshot is the one the
        // player sees on screen, and earlier ones are obsolete.
        while !stop {
            match rx.try_recv() {
                Ok(Command::Save(config)) => pending = Some(config),
                Ok(Command::Stop { save_last }) => {
                    keep_last = save_last;
                    stop = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => stop = true,
            }
        }
        if !stop || keep_last {
            if let Some(config) = pending.take() {
                write_config(&path, &config, &error);
            }
        } else {
            pending = None;
        }
        if stop {
            return;
        }
    }
}

fn write_config(path: &Path, config: &Config, error: &Arc<Mutex<Option<String>>>) {
    // Validation lives on the worker too: it walks the source list, which is
    // cheap, but it must not be repeated on the UI thread for every edit.
    let result = config
        .validate()
        .map_err(|error| error.to_string())
        .and_then(|()| {
            pml_config::save_json_atomic(path, config).map_err(|error| error.to_string())
        });
    match result {
        Ok(()) => {}
        Err(message) => {
            tracing::error!(path = %path.display(), %message, "config.json was not saved");
            *error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
        }
    }
}
