use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use conductor_remote::artifacts::store::*;
use conductor_remote::artifacts::{ArtifactClock, ArtifactError, ArtifactExposureReceipt};

const COOKIE: &str = "conductor_preview_0123456789abcdef0123456789abcdef";
const COOKIE_TWO: &str = "conductor_preview_fedcba9876543210fedcba9876543210";

struct Clock;
impl ArtifactClock for Clock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_792_152_000)
    }
    fn monotonic(&self) -> Duration {
        Duration::from_secs(42)
    }
}

fn identity(n: u8) -> String {
    format!("{n:064x}")
}

fn reserve(guard: &mut LifecycleGuard) -> OriginBinding {
    guard
        .reserve_origin(
            &identity(1),
            "https://relay.example.test:31001",
            COOKIE,
            Clock.now(),
        )
        .unwrap()
}

fn receipt(binding: &OriginBinding, generation: u64) -> ArtifactExposureReceipt {
    ArtifactExposureReceipt {
        binding_id: binding.binding_id.clone(),
        origin: binding.origin.clone(),
        https_port: 31001,
        loopback_port: 41001,
        generation,
    }
}

#[test]
fn persistent_binding_cookie_name_tombstone_and_deterministic_clock() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let store = ArtifactStore::open(&path).unwrap();
    let mut guard = store.lock().unwrap();
    let binding = reserve(&mut guard);
    assert_eq!(binding.identity, identity(1));
    assert_eq!(binding.reserved_at_millis, 1_792_152_000_000);
    guard.tombstone_binding(&binding.binding_id).unwrap();
    drop(guard);
    drop(store);
    let restarted = ArtifactStore::open(&path).unwrap();
    let snapshot = restarted.snapshot().unwrap();
    assert!(snapshot.bindings[0].tombstoned);
    assert_eq!(
        snapshot.reserved_origins().collect::<Vec<_>>(),
        [binding.origin.as_str()]
    );
    let mut guard = restarted.lock().unwrap();
    assert!(guard
        .reserve_origin(&identity(2), &binding.origin, COOKIE_TWO, Clock.now())
        .is_err());
    let reactivated = guard
        .reserve_origin(
            &identity(1),
            &binding.origin,
            COOKIE_TWO,
            Clock.now() + Duration::from_secs(100),
        )
        .unwrap();
    assert_eq!(reactivated, binding); // cookie name and reservation time remain stable
    assert!(!reactivated.tombstoned);
    let other = guard
        .reserve_origin(
            &identity(2),
            "http://localhost:41002",
            COOKIE_TWO,
            Clock.now(),
        )
        .unwrap();
    assert_ne!(other.binding_id, binding.binding_id);
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
    for name in ["artifact-state.json", "artifact-lifecycle.lock"] {
        assert_eq!(fs::metadata(path.join(name)).unwrap().mode() & 0o777, 0o600);
    }
    let wire = fs::read_to_string(path.join("artifact-state.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(
        json.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "version",
            "generation",
            "remoteRevoked",
            "bindings",
            "receipts"
        ]
    );
    for forbidden in ["html", "grant", "cookieValue", "authorization", "<html>"] {
        assert!(!wire.contains(forbidden));
    }
}

#[test]
fn explicit_off_survives_restart_and_configured_tailnet_without_live_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let store = ArtifactStore::open(&path).unwrap();
    assert_eq!(store.current_permission(true).unwrap().generation, 0);
    let mut guard = store.lock().unwrap();
    let binding = reserve(&mut guard);
    let pending = receipt(&binding, 0);
    guard.begin_publication(pending.clone(), true).unwrap();
    let revoked = guard.revoke_remote().unwrap();
    assert!(!revoked.enabled);
    assert_eq!(revoked.generation, 1);
    assert!(matches!(
        guard.mark_active(&pending),
        Err(ArtifactError::Forbidden(_))
    ));
    assert!(matches!(
        guard.begin_publication(pending.clone(), true),
        Err(ArtifactError::Forbidden(_))
    ));
    drop(guard);
    drop(store);
    let restarted = ArtifactStore::open(&path).unwrap();
    assert!(!restarted.current_permission(true).unwrap().enabled);
    assert!(!restarted.current_permission(false).unwrap().enabled); // saved-off Ensure is read-only
    let mut guard = restarted.lock().unwrap();
    let enabled = guard.allow_remote().unwrap();
    assert!(enabled.enabled);
    assert_eq!(enabled.generation, 1);
    assert!(guard.begin_publication(pending.clone(), true).is_err()); // stale generation
    assert!(guard.finish_cleanup(&pending).unwrap());
    let current = receipt(&binding, 1);
    assert!(guard.begin_publication(current.clone(), false).is_err());
    guard.begin_publication(current.clone(), true).unwrap();
    guard.mark_active(&current).unwrap();
    assert_eq!(guard.revoke_remote().unwrap().generation, 2);
    assert_eq!(
        guard.snapshot().unwrap().recovery_receipts()[0].receipt,
        current
    );
}

#[test]
fn pending_active_restart_recovery_and_failed_cleanup_retention() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let store = ArtifactStore::open(&path).unwrap();
    let mut guard = store.lock().unwrap();
    let binding = reserve(&mut guard);
    let owned = receipt(&binding, 0);
    assert_eq!(
        guard.begin_publication(owned.clone(), true).unwrap().phase,
        ReceiptPhase::Pending
    );
    drop(guard);
    drop(store); // crash between pending persist and external publish
    let restarted = ArtifactStore::open(&path).unwrap();
    let recovery = restarted.snapshot().unwrap();
    assert_eq!(
        recovery.recovery_receipts(),
        &[OwnedMappingReceipt {
            receipt: owned.clone(),
            phase: ReceiptPhase::Pending
        }]
    );
    assert_eq!(restarted.snapshot().unwrap(), recovery); // failed cleanup makes no mutation
    let mut guard = restarted.lock().unwrap();
    let mut different_target = owned.clone();
    different_target.loopback_port += 1;
    assert!(!guard.finish_cleanup(&different_target).unwrap());
    assert!(guard.begin_publication(different_target, true).is_err());
    guard.mark_active(&owned).unwrap();
    assert_eq!(
        guard.begin_publication(owned.clone(), true).unwrap().phase,
        ReceiptPhase::Active
    );
    drop(guard);
    let again = ArtifactStore::open(&path).unwrap();
    assert_eq!(
        again.snapshot().unwrap().recovery_receipts()[0].phase,
        ReceiptPhase::Active
    );
    let mut guard = again.lock().unwrap();
    assert!(guard.finish_cleanup(&owned).unwrap());
    assert!(!guard.finish_cleanup(&owned).unwrap());
    assert!(guard.snapshot().unwrap().receipts.is_empty());
    assert_eq!(guard.snapshot().unwrap().bindings, [binding]);
}

#[test]
fn separate_handles_serialize_recheck_and_avoid_lost_updates() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let first = ArtifactStore::open(&path).unwrap();
    let second = ArtifactStore::open(&path).unwrap();
    let mut guard = first.lock().unwrap();
    let binding = reserve(&mut guard);
    let stale = receipt(&binding, 0);
    let (attempted_tx, attempted_rx) = mpsc::channel();
    let (acquired_tx, acquired_rx) = mpsc::channel();
    let child = std::thread::spawn(move || {
        attempted_tx.send(()).unwrap();
        let mut other = second.lock().unwrap();
        acquired_tx.send(()).unwrap();
        assert!(matches!(
            other.begin_publication(stale, true),
            Err(ArtifactError::Forbidden(_))
        ));
        other
            .reserve_origin(
                &identity(2),
                "http://localhost:41002",
                COOKIE_TWO,
                Clock.now(),
            )
            .unwrap();
    });
    attempted_rx.recv().unwrap();
    assert!(acquired_rx
        .recv_timeout(Duration::from_millis(100))
        .is_err());
    guard.revoke_remote().unwrap();
    drop(guard);
    acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    child.join().unwrap();
    assert_eq!(first.snapshot().unwrap().bindings.len(), 2);
    assert_eq!(first.snapshot().unwrap().generation, 1);
    assert!(first.snapshot().unwrap().remote_revoked);
}

// Spawn this same fixture-only test executable to establish a genuinely cross-process flock.
#[test]
fn lock_child_process() {
    let Some(path) = std::env::var_os("ARTIFACT_TEST_STATE_DIR") else {
        return;
    };
    let store = ArtifactStore::open(std::path::Path::new(&path)).unwrap();
    store.lock().unwrap().revoke_remote().unwrap();
}

#[test]
fn cross_process_lock_serializes_publication_and_disable() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let store = ArtifactStore::open(&path).unwrap();
    let mut guard = store.lock().unwrap();
    let binding = reserve(&mut guard);
    let owned = receipt(&binding, 0);
    guard.begin_publication(owned.clone(), true).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_child_process", "--nocapture"])
        .env("ARTIFACT_TEST_STATE_DIR", &path)
        .spawn()
        .unwrap();
    // Parent can mark publication active while child must wait to revoke under the same lock.
    guard.mark_active(&owned).unwrap();
    assert!(child.try_wait().unwrap().is_none());
    drop(guard);
    assert!(child.wait().unwrap().success());
    assert_eq!(store.current_permission(true).unwrap().generation, 1);
    assert!(!store.current_permission(true).unwrap().enabled);
    assert_eq!(
        store.snapshot().unwrap().recovery_receipts()[0].receipt,
        owned
    );
}

#[test]
fn atomic_snapshots_are_complete_during_updates() {
    let temp = tempfile::tempdir().unwrap();
    let store = ArtifactStore::open(&temp.path().join("state")).unwrap();
    let reader = store.clone();
    let child = std::thread::spawn(move || {
        for _ in 0..200 {
            let snapshot = reader.snapshot().unwrap();
            assert!(snapshot.generation <= 30);
            assert!(snapshot.bindings.len() <= 1);
        }
    });
    let mut guard = store.lock().unwrap();
    reserve(&mut guard);
    for _ in 0..30 {
        guard.revoke_remote().unwrap();
    }
    child.join().unwrap();
    assert_eq!(store.snapshot().unwrap().generation, 30);
    assert!(!fs::read_dir(temp.path().join("state"))
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_str()
            .unwrap()
            .ends_with(".tmp")));
}

#[test]
fn descriptor_pinning_and_symlink_state_or_lock_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let store = ArtifactStore::open(&path).unwrap();
    let moved = temp.path().join("original");
    fs::rename(&path, &moved).unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), &path).unwrap();
    let mut guard = store.lock().unwrap();
    reserve(&mut guard);
    assert!(moved.join("artifact-state.json").exists());
    assert!(!outside.path().join("artifact-state.json").exists());
    assert!(ArtifactStore::open(&path).is_err());
    drop(guard);
    fs::remove_file(moved.join("artifact-state.json")).unwrap();
    let target = outside.path().join("synthetic-target");
    fs::write(&target, "untouched fixture").unwrap();
    symlink(&target, moved.join("artifact-state.json")).unwrap();
    assert!(store.snapshot().is_err());
    assert!(ArtifactStore::open(&moved).is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "untouched fixture");
    fs::remove_file(moved.join("artifact-lifecycle.lock")).unwrap();
    symlink(&target, moved.join("artifact-lifecycle.lock")).unwrap();
    assert!(store.lock().is_err());
    assert_eq!(fs::read_to_string(target).unwrap(), "untouched fixture");
}

#[test]
fn held_guard_fails_closed_when_lock_inode_is_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let store = ArtifactStore::open(&path).unwrap();
    let mut guard = store.lock().unwrap();
    let before = store.snapshot().unwrap();
    let lock_path = path.join("artifact-lifecycle.lock");
    fs::rename(&lock_path, path.join("old-lock")).unwrap();
    let replacement = fs::File::create(&lock_path).unwrap();
    replacement
        .set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    assert!(guard.revoke_remote().is_err());
    assert_eq!(store.snapshot().unwrap(), before);
    drop(guard);
    let mut current = store.lock().unwrap();
    current.revoke_remote().unwrap();
    assert_eq!(store.snapshot().unwrap().generation, 1);
}

#[test]
fn corrupt_insecure_unknown_or_secret_fields_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let store = ArtifactStore::open(&path).unwrap();
    let state_file = path.join("artifact-state.json");
    let original = fs::read_to_string(&state_file).unwrap();
    for bad in [
        "not json".to_owned(),
        original.replace("\"version\":1", "\"version\":2"),
        original.replace("\"version\":1", "\"version\":1,\"html\":\"synthetic\""),
    ] {
        fs::write(&state_file, &bad).unwrap();
        assert!(store.snapshot().is_err());
        assert!(ArtifactStore::open(&path).is_err());
        assert_eq!(fs::read_to_string(&state_file).unwrap(), bad);
    }
    fs::write(&state_file, &original).unwrap();
    fs::set_permissions(&state_file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.snapshot().is_err());
    fs::set_permissions(&state_file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&state_file, temp.path().join("hard-link")).unwrap();
    assert!(store.snapshot().is_err());
}

#[test]
fn invalid_identity_cookie_origin_receipt_and_generation_overflow() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let store = ArtifactStore::open(&path).unwrap();
    let mut guard = store.lock().unwrap();
    assert!(guard
        .reserve_origin(
            "<html>not an identity</html>",
            "https://relay.example.test:31001",
            COOKIE,
            Clock.now()
        )
        .is_err());
    for origin in [
        "https://user@relay.example.test:31001",
        "https://relay.example.test:31001/path",
        "https://relay.example.test:31001?x=1",
        "ftp://relay.example.test",
        "http://localhost:0",
    ] {
        assert!(
            guard
                .reserve_origin(&identity(1), origin, COOKIE, Clock.now())
                .is_err(),
            "{origin}"
        );
    }
    assert!(guard
        .reserve_origin(
            &identity(1),
            "https://relay.example.test:31001",
            "cookie=value",
            Clock.now()
        )
        .is_err());
    assert!(guard
        .reserve_origin(
            &identity(1),
            "https://relay.example.test:31001",
            COOKIE,
            UNIX_EPOCH - Duration::from_secs(1)
        )
        .is_err());
    let binding = reserve(&mut guard);
    assert!(guard
        .reserve_origin(&identity(2), "http://localhost:41002", COOKIE, Clock.now())
        .is_err());
    let good = receipt(&binding, 0);
    for bad in [
        ArtifactExposureReceipt {
            https_port: 20001,
            ..good.clone()
        },
        ArtifactExposureReceipt {
            loopback_port: 0,
            ..good.clone()
        },
        ArtifactExposureReceipt {
            binding_id: identity(9),
            ..good.clone()
        },
        ArtifactExposureReceipt {
            generation: 1,
            ..good.clone()
        },
    ] {
        assert!(guard.begin_publication(bad, true).is_err());
    }
    guard.tombstone_binding(&binding.binding_id).unwrap();
    assert!(guard.begin_publication(good, true).is_err());
    drop(guard);
    let state_file = path.join("artifact-state.json");
    let wire = fs::read_to_string(&state_file)
        .unwrap()
        .replace("\"generation\":0", &format!("\"generation\":{}", u64::MAX));
    fs::write(state_file, wire).unwrap();
    let mut guard = store.lock().unwrap();
    assert!(guard.revoke_remote().is_err());
    assert_eq!(guard.snapshot().unwrap().generation, u64::MAX);
    assert!(!guard.snapshot().unwrap().permission(true).enabled);
    assert!(guard.allow_remote().is_err());
}
