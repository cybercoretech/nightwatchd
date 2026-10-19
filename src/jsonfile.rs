//! A small list of records kept in a private JSON file next to the config
//! (device tokens, share links). The file is re-read whenever it changes, so
//! the CLI can edit it while the server runs, and written atomically with
//! mode 0600.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

pub struct Records<T> {
    pub list: Vec<T>,
    seen: Option<SystemTime>,
}

pub struct JsonFile<T> {
    path: Option<PathBuf>,
    inner: Mutex<Records<T>>,
}

impl<T: Serialize + DeserializeOwned> JsonFile<T> {
    pub fn open(path: PathBuf) -> JsonFile<T> {
        let f = JsonFile {
            path: Some(path),
            inner: Mutex::new(Records {
                list: vec![],
                seen: None,
            }),
        };
        drop(f.lock());
        f
    }

    /// No file (tests).
    #[cfg(test)]
    pub fn memory() -> JsonFile<T> {
        JsonFile {
            path: None,
            inner: Mutex::new(Records {
                list: vec![],
                seen: None,
            }),
        }
    }

    /// The records, re-read first if the file changed on disk.
    pub fn lock(&self) -> MutexGuard<'_, Records<T>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(path) = &self.path {
            let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
            if mtime != inner.seen {
                inner.list = std::fs::read(path)
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default();
                inner.seen = mtime;
            }
        }
        inner
    }

    pub fn save(&self, records: &mut Records<T>) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(&records.list)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        records.seen = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        Ok(())
    }
}
