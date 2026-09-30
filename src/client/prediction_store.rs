//! Best-effort profile persistence kept outside the input/render loop.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::shell::prediction_profiles::{ProfileUpdate, Profiles, MAX_PROFILE_BYTES};

const LOCK_TIMEOUT: Duration = Duration::from_secs(1);
const LOCK_RETRY: Duration = Duration::from_millis(20);
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const MAX_PENDING_UPDATES: usize = 128;

struct StoreRequest {
    sequence: u64,
    update: ProfileUpdate,
}

struct StoreSnapshot {
    accepted_seq: u64,
    profiles: Profiles,
}

pub(crate) struct ProfileStore {
    sender: Option<SyncSender<StoreRequest>>,
    worker: Option<JoinHandle<()>>,
    latest: Arc<Mutex<Option<StoreSnapshot>>>,
    last_enqueued: u64,
    stopping: Arc<AtomicBool>,
    requested_reset_epoch: Arc<AtomicU64>,
    minimum_snapshot_epoch: u64,
}

impl ProfileStore {
    pub(crate) fn start(path: PathBuf) -> io::Result<(Profiles, Self)> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid editor profile path")
            })?;
        ensure_directory(parent)?;
        let profiles = load_or_empty(&path)?;
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING_UPDATES);
        let latest = Arc::new(Mutex::new(None));
        let worker_latest = Arc::clone(&latest);
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = Arc::clone(&stopping);
        let requested_reset_epoch = Arc::new(AtomicU64::new(0));
        let worker_reset_epoch = Arc::clone(&requested_reset_epoch);
        let worker = thread::Builder::new()
            .name("herdr-editor-profiles".into())
            .spawn(move || {
                write_updates(
                    path,
                    receiver,
                    worker_latest,
                    worker_stopping,
                    worker_reset_epoch,
                )
            })?;
        Ok((
            profiles,
            Self {
                sender: Some(sender),
                worker: Some(worker),
                latest,
                last_enqueued: 0,
                stopping,
                requested_reset_epoch,
                minimum_snapshot_epoch: 0,
            },
        ))
    }

    pub(crate) fn enqueue(&mut self, update: ProfileUpdate) -> bool {
        if !update.is_valid() {
            tracing::debug!("ignoring invalid editor profile update");
            return false;
        }
        if let Some(sender) = &self.sender {
            let Some(sequence) = self.last_enqueued.checked_add(1) else {
                tracing::debug!("editor profile update counter exhausted");
                return false;
            };
            match sender.try_send(StoreRequest { sequence, update }) {
                Ok(()) => {
                    self.last_enqueued = sequence;
                    return true;
                }
                Err(mpsc::TrySendError::Full(request)) => {
                    let minimum = request.update.epoch().saturating_add(1);
                    self.minimum_snapshot_epoch = self.minimum_snapshot_epoch.max(minimum);
                    self.requested_reset_epoch
                        .fetch_max(minimum, Ordering::AcqRel);
                    tracing::debug!("editor profile queue is full; requesting cache reset");
                    return false;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    tracing::debug!("editor profile writer is unavailable");
                }
            }
        }
        false
    }

    pub(crate) fn take_latest(&self) -> Option<Profiles> {
        let mut latest = match self.latest.try_lock() {
            Ok(latest) => latest,
            Err(std::sync::TryLockError::WouldBlock) => return None,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                tracing::debug!("editor profile snapshot is unavailable");
                return None;
            }
        };
        let snapshot = latest.take()?;
        (snapshot.accepted_seq >= self.last_enqueued
            && snapshot.profiles.epoch() >= self.minimum_snapshot_epoch)
            .then_some(snapshot.profiles)
    }
}

impl Drop for ProfileStore {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::debug!("editor profile writer exited unexpectedly");
            }
        }
    }
}

fn write_updates(
    path: PathBuf,
    receiver: Receiver<StoreRequest>,
    latest: Arc<Mutex<Option<StoreSnapshot>>>,
    stopping: Arc<AtomicBool>,
    requested_reset_epoch: Arc<AtomicU64>,
) {
    let mut pending = Vec::with_capacity(MAX_PENDING_UPDATES);
    let mut accepted_seq = 0;
    let mut deferred = false;
    let mut next_refresh = Instant::now() + REFRESH_INTERVAL;
    loop {
        if stopping.load(Ordering::Acquire) {
            fill_pending(&receiver, &mut pending);
            // A final bounded batch may leave accepted updates in the channel.
            // Fence those discarded updates rather than losing an invalidation.
            for _ in 0..MAX_PENDING_UPDATES {
                let Ok(request) = receiver.try_recv() else {
                    break;
                };
                requested_reset_epoch
                    .fetch_max(request.update.epoch().saturating_add(1), Ordering::AcqRel);
            }
            if let Err(error) = flush_pending(
                &path,
                &mut pending,
                &mut accepted_seq,
                &latest,
                &requested_reset_epoch,
            ) {
                tracing::debug!(%error, "could not flush editor learning at shutdown");
            }
            return;
        }
        if !deferred && requested_reset_epoch.load(Ordering::Acquire) != 0 {
            if let Err(error) = flush_pending(
                &path,
                &mut pending,
                &mut accepted_seq,
                &latest,
                &requested_reset_epoch,
            ) {
                tracing::debug!(%error, "could not reset cached editor learning");
                deferred = true;
                next_refresh = Instant::now() + REFRESH_INTERVAL;
            }
            continue;
        }
        if Instant::now() >= next_refresh {
            match load(&path) {
                Ok(profiles) => {
                    deferred = false;
                    if pending.is_empty() && requested_reset_epoch.load(Ordering::Acquire) == 0 {
                        publish(&latest, accepted_seq, profiles);
                    } else if let Err(error) = flush_pending(
                        &path,
                        &mut pending,
                        &mut accepted_seq,
                        &latest,
                        &requested_reset_epoch,
                    ) {
                        tracing::debug!(%error, "could not persist editor learning");
                        deferred = true;
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "could not refresh editor learning");
                    deferred = true;
                }
            }
            next_refresh = Instant::now() + REFRESH_INTERVAL;
            continue;
        }
        if pending.len() == MAX_PENDING_UPDATES {
            while Instant::now() < next_refresh && !stopping.load(Ordering::Acquire) {
                thread::sleep(LOCK_RETRY);
            }
            continue;
        }
        match receiver.recv_timeout(next_refresh.saturating_duration_since(Instant::now())) {
            Ok(request) => {
                pending.push(request);
                fill_pending(&receiver, &mut pending);
                if !deferred && !stopping.load(Ordering::Acquire) {
                    if let Err(error) = flush_pending(
                        &path,
                        &mut pending,
                        &mut accepted_seq,
                        &latest,
                        &requested_reset_epoch,
                    ) {
                        tracing::debug!(%error, "could not persist editor learning");
                        deferred = true;
                        next_refresh = Instant::now() + REFRESH_INTERVAL;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                stopping.store(true, Ordering::Release);
            }
        }
    }
}

fn fill_pending(receiver: &Receiver<StoreRequest>, pending: &mut Vec<StoreRequest>) {
    while pending.len() < MAX_PENDING_UPDATES {
        let Ok(request) = receiver.try_recv() else {
            break;
        };
        pending.push(request);
    }
}

fn flush_pending(
    path: &Path,
    pending: &mut Vec<StoreRequest>,
    accepted_seq: &mut u64,
    latest: &Mutex<Option<StoreSnapshot>>,
    requested_reset_epoch: &AtomicU64,
) -> io::Result<()> {
    let minimum_epoch = requested_reset_epoch.swap(0, Ordering::AcqRel);
    if pending.is_empty() && minimum_epoch == 0 {
        return Ok(());
    }
    let sequence = pending.last().map_or(*accepted_seq, |last| last.sequence);
    let updates: Vec<_> = pending
        .iter()
        .map(|request| request.update.clone())
        .collect();
    let profiles = match persist_changes(path, &updates, minimum_epoch) {
        Ok(profiles) => profiles,
        Err(error) => {
            requested_reset_epoch.fetch_max(minimum_epoch, Ordering::AcqRel);
            return Err(error);
        }
    };
    *accepted_seq = sequence;
    pending.clear();
    publish(latest, *accepted_seq, profiles);
    Ok(())
}

fn publish(latest: &Mutex<Option<StoreSnapshot>>, accepted_seq: u64, profiles: Profiles) {
    match latest.lock() {
        Ok(mut latest) => {
            *latest = Some(StoreSnapshot {
                accepted_seq,
                profiles,
            })
        }
        Err(_) => tracing::debug!("editor profile snapshot is unavailable"),
    }
}

#[cfg(test)]
fn persist_updates(path: &Path, updates: &[ProfileUpdate]) -> io::Result<Profiles> {
    persist_changes(path, updates, 0)
}

fn persist_changes(
    path: &Path,
    updates: &[ProfileUpdate],
    minimum_epoch: u64,
) -> io::Result<Profiles> {
    let _lock = acquire_lock(path)?;
    // Never merge a stale whole-bank snapshot: an explicit invalidation must
    // remove the latest record written by another attached client.
    let mut profiles = load(path)?;
    let mut changed = false;
    if minimum_epoch != 0 {
        if !profiles.reset_to_epoch(minimum_epoch) {
            return Err(io::Error::other("editor profile epoch exhausted"));
        }
        changed = true;
    }
    for update in updates {
        changed |= profiles.apply(update);
    }
    if changed {
        let bytes = profiles.to_json().map_err(io::Error::other)?;
        super::endpoint::store_private_json_with_limit(
            path,
            &bytes,
            "editor profiles",
            MAX_PROFILE_BYTES,
        )
        .map_err(io::Error::other)?;
    }
    Ok(profiles)
}

fn load_or_empty(path: &Path) -> io::Result<Profiles> {
    match load(path) {
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            tracing::debug!(%error, "ignoring invalid stored editor profiles");
            Ok(Profiles::default())
        }
        other => other,
    }
}

fn load(path: &Path) -> io::Result<Profiles> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Profiles::default()),
        Err(error) => return Err(error),
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "editor profile path is not a regular file",
            ));
        }
        Ok(_) => {}
    }
    let file = File::open(path)?;
    if file.metadata()?.len() > MAX_PROFILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "editor profile file exceeds the storage limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PROFILE_BYTES + 1).read_to_end(&mut bytes)?;
    Profiles::from_json(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn ensure_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "editor profile parent is not a directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "invalid editor profile directory",
                    )
                })?;
            ensure_directory(parent)?;
            match crate::platform::create_remote_private_dir(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    ensure_directory(path)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

fn acquire_lock(path: &Path) -> io::Result<File> {
    let path = lock_path(path);
    let file = match crate::platform::create_private_state_file(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(&path)?.file_type().is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "editor profile lock is not a regular file",
                ));
            }
            OpenOptions::new().read(true).write(true).open(&path)?
        }
        Err(error) => return Err(error),
    };
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::Error(error)) => return Err(error),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => thread::sleep(LOCK_RETRY),
            Err(TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "editor profile lock remained busy",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::client::shell::prediction_profiles::{machine_key, EditorProfile};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "herdr-editor-profiles-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            Self(path)
        }

        fn path(&self) -> PathBuf {
            self.0.join("profiles.json")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn profile(agent: &str) -> EditorProfile {
        EditorProfile {
            machine_key: machine_key("generated-test-machine"),
            agent: agent.into(),
            word_rules: std::array::from_fn(|_| Default::default()),
            echo_trained: true,
            prompt_fingerprints: vec![[1; 32]],
        }
    }

    fn observe(agent: &str) -> ProfileUpdate {
        ProfileUpdate::Observe {
            epoch: 0,
            profile: profile(agent),
        }
    }

    #[test]
    fn writers_merge_latest_records_and_drop_flushes() {
        let directory = TestDirectory::new();
        let path = directory.path();
        let (_, mut first) = ProfileStore::start(path.clone()).unwrap();
        let (_, mut second) = ProfileStore::start(path.clone()).unwrap();
        first.enqueue(observe("claude"));
        second.enqueue(observe("codex"));
        drop(first);
        drop(second);
        let restored = load(&path).unwrap();
        assert!(restored
            .profile(&machine_key("generated-test-machine"), "claude")
            .is_some());
        assert!(restored
            .profile(&machine_key("generated-test-machine"), "codex")
            .is_some());
    }

    #[test]
    fn invalidation_reads_latest_file_and_does_not_resurrect_other_records() {
        let directory = TestDirectory::new();
        let path = directory.path();
        let (_, mut stale) = ProfileStore::start(path.clone()).unwrap();
        persist_updates(&path, &[observe("claude")]).unwrap();
        persist_updates(&path, &[observe("codex")]).unwrap();
        stale.enqueue(ProfileUpdate::Invalidate {
            epoch: 0,
            machine_key: machine_key("generated-test-machine"),
            agent: "claude".into(),
        });
        drop(stale);
        let restored = load(&path).unwrap();
        assert!(restored
            .profile(&machine_key("generated-test-machine"), "claude")
            .is_none());
        assert!(restored
            .profile(&machine_key("generated-test-machine"), "codex")
            .is_some());
    }

    #[test]
    fn stale_running_writer_cannot_replay_observation_after_invalidation() {
        let directory = TestDirectory::new();
        let path = directory.path();
        let (_, mut stale) = ProfileStore::start(path.clone()).unwrap();
        let old = observe("claude");
        persist_updates(&path, std::slice::from_ref(&old)).unwrap();
        persist_updates(
            &path,
            &[ProfileUpdate::Invalidate {
                epoch: 0,
                machine_key: machine_key("generated-test-machine"),
                agent: "claude".into(),
            }],
        )
        .unwrap();
        assert!(stale.enqueue(old));
        drop(stale);
        let fenced = load(&path).unwrap();
        assert_eq!(fenced.epoch(), 1);
        assert!(fenced
            .profile(&machine_key("generated-test-machine"), "claude")
            .is_none());
        let (current, mut fresh) = ProfileStore::start(path.clone()).unwrap();
        assert!(fresh.enqueue(ProfileUpdate::Observe {
            epoch: current.epoch(),
            profile: profile("claude")
        }));
        drop(fresh);
        assert!(load(&path)
            .unwrap()
            .profile(&machine_key("generated-test-machine"), "claude")
            .is_some());
    }

    #[test]
    fn corruption_backpressure_is_bounded_and_requests_priority_epoch_reset() {
        let directory = TestDirectory::new();
        ensure_directory(&directory.0).unwrap();
        let path = directory.path();
        fs::write(&path, b"{invalid").unwrap();
        let (_, mut store) = ProfileStore::start(path.clone()).unwrap();
        let mut accepted = 0;
        while store.enqueue(observe("claude")) {
            accepted += 1;
            assert!(accepted <= MAX_PENDING_UPDATES * 2);
        }
        assert_eq!(store.last_enqueued, accepted as u64);
        assert_eq!(store.requested_reset_epoch.load(Ordering::Acquire), 1);
        assert_eq!(store.minimum_snapshot_epoch, 1);
        publish(&store.latest, store.last_enqueued, Profiles::default());
        assert!(store.take_latest().is_none());
        let started = Instant::now();
        drop(store);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn priority_reset_runs_before_old_queued_observations() {
        let directory = TestDirectory::new();
        ensure_directory(&directory.0).unwrap();
        let path = directory.path();
        persist_updates(&path, &[observe("claude"), observe("codex")]).unwrap();
        let requested = AtomicU64::new(7);
        let latest = Mutex::new(None);
        let mut accepted_seq = 0;
        let mut pending = vec![StoreRequest {
            sequence: 1,
            update: observe("claude"),
        }];
        flush_pending(&path, &mut pending, &mut accepted_seq, &latest, &requested).unwrap();
        assert!(pending.is_empty());
        assert_eq!(requested.load(Ordering::Acquire), 0);
        let bank = load(&path).unwrap();
        assert_eq!(bank.epoch(), 7);
        assert!(bank
            .profile(&machine_key("generated-test-machine"), "claude")
            .is_none());
        assert!(bank
            .profile(&machine_key("generated-test-machine"), "codex")
            .is_none());
        assert_eq!(latest.lock().unwrap().as_ref().unwrap().profiles.epoch(), 7);
    }

    #[test]
    fn stale_refresh_cannot_erase_queued_learning() {
        let directory = TestDirectory::new();
        let path = directory.path();
        let (_, mut store) = ProfileStore::start(path.clone()).unwrap();
        let lock = acquire_lock(&path).unwrap();
        store.enqueue(observe("claude"));
        publish(&store.latest, 0, Profiles::default());
        assert!(store.take_latest().is_none());
        drop(lock);
        drop(store);
        assert!(load(&path)
            .unwrap()
            .profile(&machine_key("generated-test-machine"), "claude")
            .is_some());
    }

    #[test]
    fn running_client_refreshes_other_client_learning_and_keeps_valid_state_on_error() {
        let directory = TestDirectory::new();
        let path = directory.path();
        let (_, store) = ProfileStore::start(path.clone()).unwrap();
        persist_updates(&path, &[observe("codex")]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(6);
        let refreshed = loop {
            if let Some(profiles) = store.take_latest() {
                break profiles;
            }
            assert!(
                Instant::now() < deadline,
                "background profile refresh timed out"
            );
            thread::sleep(LOCK_RETRY);
        };
        assert!(refreshed
            .profile(&machine_key("generated-test-machine"), "codex")
            .is_some());
        fs::write(&path, b"{broken private draft").unwrap();
        thread::sleep(REFRESH_INTERVAL + LOCK_RETRY);
        assert!(store.take_latest().is_none());
        drop(store);
    }

    #[test]
    fn corruption_and_oversized_files_fall_back_without_importing_payload() {
        let directory = TestDirectory::new();
        ensure_directory(&directory.0).unwrap();
        let path = directory.path();
        for content in [
            b"{broken private draft".to_vec(),
            vec![b'x'; MAX_PROFILE_BYTES as usize + 1],
        ] {
            fs::write(&path, &content).unwrap();
            let (profiles, mut store) = ProfileStore::start(path.clone()).unwrap();
            assert_eq!(profiles, Profiles::default());
            store.enqueue(observe("claude"));
            drop(store);
            assert_eq!(fs::read(&path).unwrap(), content);
            fs::remove_file(&path).unwrap();
            let (_, mut recovered) = ProfileStore::start(path.clone()).unwrap();
            recovered.enqueue(observe("claude"));
            drop(recovered);
            assert!(load(&path)
                .unwrap()
                .profile(&machine_key("generated-test-machine"), "claude")
                .is_some());
            assert!(!String::from_utf8(fs::read(&path).unwrap())
                .unwrap()
                .contains("private draft"));
        }
    }

    #[test]
    fn busy_external_lock_is_bounded_and_leaves_existing_profile_unchanged() {
        let directory = TestDirectory::new();
        ensure_directory(&directory.0).unwrap();
        let path = directory.path();
        persist_updates(&path, &[observe("claude")]).unwrap();
        let lock = acquire_lock(&path).unwrap();
        let started = Instant::now();
        let error = persist_updates(&path, &[observe("codex")]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(load(&path)
            .unwrap()
            .profile(&machine_key("generated-test-machine"), "codex")
            .is_none());
        drop(lock);
    }

    #[cfg(unix)]
    #[test]
    fn private_files_and_directory_reject_symlink_targets() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let directory = TestDirectory::new();
        let path = directory.path();
        let (_, mut store) = ProfileStore::start(path.clone()).unwrap();
        store.enqueue(observe("claude"));
        drop(store);
        assert_eq!(
            fs::metadata(&directory.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for file in [&path, &lock_path(&path)] {
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let linked = directory.0.join("linked.json");
        symlink(&path, &linked).unwrap();
        assert_eq!(
            ProfileStore::start(linked).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        let linked_lock_target = directory.0.join("other.json");
        symlink(&path, lock_path(&linked_lock_target)).unwrap();
        assert_eq!(
            acquire_lock(&linked_lock_target).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
