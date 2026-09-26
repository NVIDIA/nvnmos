// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! In-memory IS-13 annotation store and its JSON checkpoint.
//!
//! Keyed by `(seed, resource type, name)`. Node and Device use an empty
//! name. The store outlives the [`nvnmos::NodeServer`]: removing a sender
//! or destroying a node does not drop entries. An entry is removed when
//! an IS-13 reset leaves it with no label, description, or writable tags.
//!
//! The checkpoint write runs on a background thread. [`AnnotationStore::apply`]
//! updates the map and returns; it does not serialize. [`AnnotationStore::flush`]
//! writes the current map before the process exits.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use nvnmos::{Annotation, AnnotationChange, ResourceType};
use serde::{Deserialize, Serialize};

const CHECKPOINT_VERSION: u32 = 1;
const DEFAULT_DEBOUNCE_MS: u64 = 1000;
const DEFAULT_ENTRY_LIMIT: u64 = 10_000;
const ENV_DEBOUNCE_MS: &str = "NVNMOSD_ANNOTATION_DEBOUNCE_MS";
const ENV_ENTRY_LIMIT: &str = "NVNMOSD_ANNOTATION_ENTRY_LIMIT";
const ENV_CHECKPOINT_FILE: &str = "NVNMOSD_ANNOTATION_CHECKPOINT_FILE";

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    seed: String,
    resource_type: ResourceType,
    name: String,
}

struct Inner {
    entries: HashMap<Key, Annotation>,
    limit: usize,
}

enum Msg {
    Dirty,
    Flush(Sender<()>),
    Shutdown,
}

/// Annotations remembered for every seed this process has seen.
pub struct AnnotationStore {
    inner: Arc<Mutex<Inner>>,
    tx: Option<Sender<Msg>>,
    writer: Option<JoinHandle<()>>,
}

#[derive(Debug)]
pub struct CheckpointError(String);

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid annotation checkpoint: {}", self.0)
    }
}

impl std::error::Error for CheckpointError {}

impl AnnotationStore {
    /// Load `path`, or start empty when the file is absent, then replace
    /// `path` with that store. Fails when an existing file cannot be read
    /// or parsed, or when the replacement write fails.
    ///
    /// Debounce and the entry limit come from the environment.
    pub fn open(path: &Path) -> Result<Self, CheckpointError> {
        Self::with_file(
            path,
            Duration::from_millis(read_u64_env(ENV_DEBOUNCE_MS, DEFAULT_DEBOUNCE_MS)),
            usize::try_from(read_u64_env(ENV_ENTRY_LIMIT, DEFAULT_ENTRY_LIMIT))
                .unwrap_or(usize::MAX),
        )
    }

    fn with_file(path: &Path, debounce: Duration, limit: usize) -> Result<Self, CheckpointError> {
        let entries = read_checkpoint(path)?;
        // Prove the file can be replaced before serving. A later write
        // failure is logged and the process keeps running.
        write_checkpoint(path, &entries)?;
        tracing::info!(
            path = %path.display(),
            entries = entries.len(),
            limit,
            debounce_ms = debounce.as_millis() as u64,
            "annotation checkpoint"
        );
        let inner = Arc::new(Mutex::new(Inner { entries, limit }));
        let (tx, rx) = mpsc::channel();
        let writer_inner = Arc::clone(&inner);
        let writer_path = path.to_path_buf();
        let writer = std::thread::Builder::new()
            .name("annotation-checkpoint".into())
            .spawn(move || writer_loop(rx, writer_inner, writer_path, debounce))
            .map_err(|error| {
                CheckpointError(format!("failed to start checkpoint thread: {error}"))
            })?;
        Ok(Self {
            inner,
            tx: Some(tx),
            writer: Some(writer),
        })
    }

    #[cfg(test)]
    fn memory(limit: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: HashMap::new(),
                limit,
            })),
            tx: None,
            writer: None,
        }
    }

    pub fn get(&self, seed: &str, resource_type: ResourceType, name: &str) -> Option<Annotation> {
        let key = Key {
            seed: seed.to_string(),
            resource_type,
            name: name.to_string(),
        };
        self.lock().entries.get(&key).cloned()
    }

    /// Apply one callback. A new key at the limit is logged and ignored.
    /// An entry with nothing left annotated is deleted.
    pub fn apply(&self, seed: &str, change: &AnnotationChange<'_>) {
        let name = change.name.unwrap_or("");
        let key = Key {
            seed: seed.to_string(),
            resource_type: change.resource_type,
            name: name.to_string(),
        };
        let mut inner = self.lock();
        if !inner.entries.contains_key(&key) && inner.entries.len() >= inner.limit {
            tracing::error!(
                seed,
                resource_type = resource_type_name(change.resource_type),
                name,
                limit = inner.limit,
                "annotation store is full; not remembering a new annotation"
            );
            return;
        }
        let entry = inner.entries.entry(key.clone()).or_default();
        if change.label_changed {
            entry.label.clone_from(&change.annotation.label);
        }
        if change.description_changed {
            entry.description.clone_from(&change.annotation.description);
        }
        if change.tags_changed {
            entry.tags.clone_from(&change.annotation.tags);
        }
        if entry.label.is_none() && entry.description.is_none() && entry.tags.is_empty() {
            inner.entries.remove(&key);
        }
        drop(inner);
        self.notify(Msg::Dirty);
    }

    /// Write the current map and wait for that write to finish.
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.notify(Msg::Flush(ack_tx));
        let _ = ack_rx.recv();
    }

    fn notify(&self, msg: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("annotation store mutex poisoned")
    }
}

impl Drop for AnnotationStore {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Shutdown);
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

/// Checkpoint file: `NVNMOSD_ANNOTATION_CHECKPOINT_FILE`, or
/// `<socket-filename>-annotations.json` in the socket's directory.
/// `/tmp/nvnmosd.sock` uses `/tmp/nvnmosd.sock-annotations.json`.
pub fn checkpoint_path(uds: &Path) -> PathBuf {
    match std::env::var(ENV_CHECKPOINT_FILE) {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
        _ => default_checkpoint_file(uds),
    }
}

fn default_checkpoint_file(uds: &Path) -> PathBuf {
    let mut name = uds.file_name().unwrap_or_default().to_os_string();
    name.push("-annotations.json");
    uds.with_file_name(name)
}

fn read_only_tag(key: &str) -> bool {
    key.starts_with("urn:x-nmos:tag:asset:")
        || key.starts_with("urn:x-nmos:tag:grouphint/")
        || key.starts_with("urn:x-nvnmos:tag:")
}

fn read_u64_env(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Err(_) => default,
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(parsed) => parsed,
            Err(_) => {
                tracing::warn!(
                    env = name,
                    value = value.trim(),
                    default,
                    "invalid integer; using default"
                );
                default
            }
        },
    }
}

fn writer_loop(rx: Receiver<Msg>, inner: Arc<Mutex<Inner>>, path: PathBuf, debounce: Duration) {
    loop {
        match rx.recv() {
            Ok(Msg::Dirty) => {
                if !debounce_then_write(&rx, &inner, &path, debounce) {
                    return;
                }
            }
            Ok(Msg::Flush(ack)) => {
                write_snapshot(&inner, &path);
                let _ = ack.send(());
            }
            Ok(Msg::Shutdown) | Err(_) => {
                write_snapshot(&inner, &path);
                return;
            }
        }
    }
}

/// Returns false when the writer should exit.
fn debounce_then_write(
    rx: &Receiver<Msg>,
    inner: &Mutex<Inner>,
    path: &Path,
    debounce: Duration,
) -> bool {
    loop {
        match rx.recv_timeout(debounce) {
            Ok(Msg::Dirty) => {}
            Ok(Msg::Flush(ack)) => {
                write_snapshot(inner, path);
                let _ = ack.send(());
            }
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                write_snapshot(inner, path);
                return false;
            }
            Err(RecvTimeoutError::Timeout) => {
                write_snapshot(inner, path);
                return true;
            }
        }
    }
}

fn write_snapshot(inner: &Mutex<Inner>, path: &Path) {
    let entries = inner
        .lock()
        .expect("annotation store mutex poisoned")
        .entries
        .clone();
    if let Err(error) = write_checkpoint(path, &entries) {
        tracing::error!(
            path = %path.display(),
            %error,
            "failed to write annotation checkpoint"
        );
    }
}

fn resource_type_name(resource_type: ResourceType) -> &'static str {
    match resource_type {
        ResourceType::Node => "node",
        ResourceType::Device => "device",
        ResourceType::Source => "source",
        ResourceType::Flow => "flow",
        ResourceType::Sender => "sender",
        ResourceType::Receiver => "receiver",
    }
}

fn resource_type_from_name(name: &str) -> Result<ResourceType, CheckpointError> {
    match name {
        "node" => Ok(ResourceType::Node),
        "device" => Ok(ResourceType::Device),
        "source" => Ok(ResourceType::Source),
        "flow" => Ok(ResourceType::Flow),
        "sender" => Ok(ResourceType::Sender),
        "receiver" => Ok(ResourceType::Receiver),
        _ => Err(CheckpointError(format!("unknown resource_type {name:?}"))),
    }
}

#[derive(Serialize, Deserialize)]
struct CheckpointFile {
    version: u32,
    annotations: Vec<CheckpointRecord>,
}

#[derive(Serialize, Deserialize)]
struct CheckpointRecord {
    seed: String,
    resource_type: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tags: BTreeMap<String, Vec<String>>,
}

fn read_checkpoint(path: &Path) -> Result<HashMap<Key, Annotation>, CheckpointError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => {
            return Err(CheckpointError(format!(
                "failed to read {}: {error}",
                path.display()
            )));
        }
    };
    let parsed: CheckpointFile = serde_json::from_reader(file)
        .map_err(|error| CheckpointError(format!("failed to parse {}: {error}", path.display())))?;
    if parsed.version != CHECKPOINT_VERSION {
        return Err(CheckpointError(format!(
            "unsupported version {} in {}",
            parsed.version,
            path.display()
        )));
    }
    let mut entries = HashMap::with_capacity(parsed.annotations.len());
    for record in parsed.annotations {
        let resource_type = resource_type_from_name(&record.resource_type)?;
        let key = Key {
            seed: record.seed,
            resource_type,
            name: record.name,
        };
        if let Some(tag) = record.tags.keys().find(|tag| read_only_tag(tag)) {
            return Err(CheckpointError(format!(
                "read-only annotation tag {tag:?} in {}",
                path.display()
            )));
        }
        let annotation = Annotation {
            label: record.label,
            description: record.description,
            tags: record.tags,
        };
        if entries.insert(key, annotation).is_some() {
            return Err(CheckpointError(format!(
                "duplicate annotation in {}",
                path.display()
            )));
        }
    }
    Ok(entries)
}

fn write_checkpoint(
    path: &Path,
    entries: &HashMap<Key, Annotation>,
) -> Result<(), CheckpointError> {
    let mut annotations: Vec<CheckpointRecord> = entries
        .iter()
        .map(|(key, annotation)| CheckpointRecord {
            seed: key.seed.clone(),
            resource_type: resource_type_name(key.resource_type).to_string(),
            name: key.name.clone(),
            label: annotation.label.clone(),
            description: annotation.description.clone(),
            tags: annotation.tags.clone(),
        })
        .collect();
    annotations.sort_by(|left, right| {
        (&left.seed, &left.resource_type, &left.name).cmp(&(
            &right.seed,
            &right.resource_type,
            &right.name,
        ))
    });
    let body = serde_json::to_vec(&CheckpointFile {
        version: CHECKPOINT_VERSION,
        annotations,
    })
    .map_err(|error| CheckpointError(error.to_string()))?;

    let tmp = temp_path(path);
    let write_result = (|| {
        let mut file = File::create(&tmp).map_err(|error| {
            CheckpointError(format!("failed to create {}: {error}", tmp.display()))
        })?;
        file.write_all(&body).map_err(|error| {
            CheckpointError(format!("failed to write {}: {error}", tmp.display()))
        })?;
        file.sync_all().map_err(|error| {
            CheckpointError(format!("failed to sync {}: {error}", tmp.display()))
        })?;
        std::fs::rename(&tmp, path).map_err(|error| {
            CheckpointError(format!(
                "failed to replace {} with {}: {error}",
                path.display(),
                tmp.display()
            ))
        })?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result
}

fn temp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change<'a>(
        resource_type: ResourceType,
        name: Option<&'a str>,
        annotation: &'a Annotation,
        label_changed: bool,
        description_changed: bool,
        tags_changed: bool,
    ) -> AnnotationChange<'a> {
        AnnotationChange {
            resource_type,
            name,
            annotation,
            label_changed,
            description_changed,
            tags_changed,
        }
    }

    #[test]
    fn apply_keeps_unflagged_members_and_deletes_an_empty_entry() {
        let store = AnnotationStore::memory(8);
        let labeled = Annotation {
            label: Some("overlay".into()),
            ..Annotation::default()
        };
        store.apply(
            "seed",
            &change(
                ResourceType::Sender,
                Some("video"),
                &labeled,
                true,
                false,
                false,
            ),
        );
        let described = Annotation {
            description: Some("info".into()),
            ..Annotation::default()
        };
        store.apply(
            "seed",
            &change(
                ResourceType::Sender,
                Some("video"),
                &described,
                false,
                true,
                false,
            ),
        );
        let stored = store.get("seed", ResourceType::Sender, "video").unwrap();
        assert_eq!(stored.label.as_deref(), Some("overlay"));
        assert_eq!(stored.description.as_deref(), Some("info"));

        let reset = Annotation::default();
        store.apply(
            "seed",
            &change(
                ResourceType::Sender,
                Some("video"),
                &reset,
                true,
                true,
                false,
            ),
        );
        assert!(store.get("seed", ResourceType::Sender, "video").is_none());
    }

    #[test]
    fn limit_refuses_a_new_key_and_still_updates_an_existing_one() {
        let store = AnnotationStore::memory(1);
        let first = Annotation {
            label: Some("one".into()),
            ..Annotation::default()
        };
        store.apply(
            "seed",
            &change(ResourceType::Sender, Some("a"), &first, true, false, false),
        );
        let second = Annotation {
            label: Some("two".into()),
            ..Annotation::default()
        };
        store.apply(
            "seed",
            &change(ResourceType::Sender, Some("b"), &second, true, false, false),
        );
        assert!(store.get("seed", ResourceType::Sender, "b").is_none());
        let updated = Annotation {
            label: Some("one-b".into()),
            ..Annotation::default()
        };
        store.apply(
            "seed",
            &change(
                ResourceType::Sender,
                Some("a"),
                &updated,
                true,
                false,
                false,
            ),
        );
        assert_eq!(
            store
                .get("seed", ResourceType::Sender, "a")
                .unwrap()
                .label
                .as_deref(),
            Some("one-b")
        );
    }

    #[test]
    fn checkpoint_round_trip_and_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("annotations.json");
        assert!(
            AnnotationStore::with_file(&path, Duration::ZERO, 8)
                .unwrap()
                .get("seed", ResourceType::Node, "")
                .is_none()
        );
        assert!(path.is_file(), "missing checkpoint is created empty");

        {
            let store = AnnotationStore::with_file(&path, Duration::from_secs(60), 8).unwrap();
            let labeled = Annotation {
                label: Some("node-overlay".into()),
                tags: BTreeMap::from([("foo".into(), vec!["a".into()])]),
                ..Annotation::default()
            };
            store.apply(
                "seed",
                &change(ResourceType::Node, None, &labeled, true, false, true),
            );
            store.flush();
        }
        let restored = AnnotationStore::with_file(&path, Duration::ZERO, 8).unwrap();
        let node = restored.get("seed", ResourceType::Node, "").unwrap();
        assert_eq!(node.label.as_deref(), Some("node-overlay"));
        assert_eq!(
            node.tags.get("foo").map(Vec::as_slice),
            Some(["a".to_string()].as_slice())
        );

        std::fs::write(&path, b"not-json").unwrap();
        assert!(AnnotationStore::with_file(&path, Duration::ZERO, 8).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not-json");
    }

    #[test]
    fn default_checkpoint_file_follows_the_socket_name() {
        let nvnmosd = default_checkpoint_file(Path::new("/tmp/nvnmosd.sock"));
        let other = default_checkpoint_file(Path::new("/tmp/other.sock"));
        assert_eq!(nvnmosd, Path::new("/tmp/nvnmosd.sock-annotations.json"));
        assert_eq!(other, Path::new("/tmp/other.sock-annotations.json"));
        assert_ne!(nvnmosd, other);
    }

    #[test]
    fn read_only_tag_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("annotations.json");
        for key in [
            "urn:x-nvnmos:tag:name",
            "urn:x-nmos:tag:grouphint/v1.0",
            "urn:x-nmos:tag:asset:manufacturer/v1.0",
        ] {
            let body = format!(
                r#"{{"version":1,"annotations":[{{"seed":"s","resource_type":"sender","name":"v","tags":{{"{key}":["x"]}}}}]}}"#
            );
            std::fs::write(&path, &body).unwrap();
            let error = match AnnotationStore::with_file(&path, Duration::ZERO, 8) {
                Err(error) => error,
                Ok(_) => panic!("read-only tag {key} should fail open"),
            };
            let message = error.to_string();
            assert!(
                message.contains(key),
                "error should name the tag {key}: {message}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        }
    }

    #[test]
    fn unwritable_checkpoint_directory_fails_open() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("annotations.json");
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir.path(), perms).unwrap();
        let error = match AnnotationStore::with_file(&path, Duration::ZERO, 8) {
            Err(error) => error,
            Ok(_) => panic!("unwritable directory should fail open"),
        };
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dir.path(), perms).unwrap();
        let message = error.to_string();
        assert!(
            message.contains("failed to create"),
            "unwritable directory should fail the replacement write: {message}"
        );
    }
}
