//! Tests for OMP profile discovery and CLI target resolution.
//!
//! The regression for the profile-cleanup TOCTOU lives here: after
//! `omp_config_path_no_create` validates the conventional profile tree
//! and consults `omp`, a parent directory swapped for a symlink before
//! the empty-dir cleanup must fail the removal descriptor-relative
//! instead of `rmdir`-ing outside the `.omp` tree.

use super::{missing_ancestors, remove_created_dirs};
use crate::anchored::RootFd;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestTempDir(PathBuf);

impl TestTempDir {
    fn new() -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

        loop {
            let path = std::env::temp_dir().join(format!(
                "omp-profile-cleanup-test-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!(
                    "failed to create test temp directory {}: {error}",
                    path.display()
                ),
            }
        }
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn profile_cleanup_refuses_symlinked_parent_swap() {
    let temp_dir = TestTempDir::new();
    let temp_root = fs::canonicalize(&temp_dir.0).expect("failed to canonicalize test temp root");
    let temp_home = temp_root.join("home");
    fs::create_dir_all(&temp_home).expect("failed to create test home");
    let home = fs::canonicalize(&temp_home).expect("failed to canonicalize test home");
    let omp_root = home.join(".omp");
    fs::create_dir_all(&omp_root).expect("failed to create .omp root");

    let profile = "cleanup-regression";
    let agent = omp_root.join("profiles").join(profile).join("agent");
    let home_pin = RootFd::open(&home).expect("failed to pin test home");
    let omp_pin = home_pin
        .open_subdir_follow(".omp")
        .expect("failed to pin .omp root");
    let missing = missing_ancestors(&agent);
    assert_eq!(
        missing,
        vec![
            agent.clone(),
            omp_root.join("profiles").join(profile),
            omp_root.join("profiles"),
        ],
        "the test must record the absent conventional profile ancestors"
    );

    fs::create_dir_all(&agent).expect("failed to create profile tree");
    let external = temp_root.join("external");
    let external_empty_child = external.join(profile).join("agent");
    fs::create_dir_all(&external_empty_child).expect("failed to create external child");

    let profiles = omp_root.join("profiles");
    let renamed_profiles = omp_root.join("profiles-original");
    fs::rename(&profiles, &renamed_profiles).expect("failed to rename original profiles tree");
    symlink(&external, &profiles).expect("failed to replace profiles with symlink");

    remove_created_dirs(&missing, &home, &home_pin, Some(&omp_pin));

    assert!(
        external_empty_child.is_dir(),
        "cleanup must not remove the external empty child through the substituted parent"
    );
    assert!(
        renamed_profiles.join(profile).join("agent").is_dir(),
        "the original in-root subtree must remain after its parent is renamed"
    );
}
