use super::RootFd;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before UNIX epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("omp-anchored-race-{}-{unique}", std::process::id()));
        fs::create_dir(&path).expect("failed to create isolated test directory");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct SwappedParent {
    pinned_parent: RootFd,
    parent_path: PathBuf,
    pinned_parent_path: PathBuf,
    outside_victim: PathBuf,
    outside_path: PathBuf,
}

fn swap_parent_for_outside_symlink(temp: &TestDir) -> SwappedParent {
    let root = temp.0.join("root");
    let parent_path = root.join("parent");
    let pinned_parent_path = root.join("pinned-parent");
    let outside_path = temp.0.join("outside");
    let victim = parent_path.join("victim");
    let outside_victim = outside_path.join("victim");
    fs::create_dir_all(&parent_path).expect("failed to create root/parent");
    fs::create_dir_all(&outside_path).expect("failed to create outside directory");
    fs::write(&victim, b"original pinned victim\n").expect("failed to seed pinned victim");
    fs::write(&outside_victim, b"outside sentinel\n").expect("failed to seed outside victim");

    let root_fd = RootFd::open(&root).expect("failed to open anchored test root");
    let pinned_parent = root_fd
        .open_subdir(Path::new("parent"))
        .expect("failed to pin parent directory");
    fs::rename(&parent_path, &pinned_parent_path).expect("failed to rename pinned parent");
    symlink(&outside_path, &parent_path).expect("failed to replace parent with symlink");

    SwappedParent {
        pinned_parent,
        parent_path,
        pinned_parent_path,
        outside_victim,
        outside_path,
    }
}

#[test]
fn atomic_write_stays_with_pinned_parent_after_path_replacement() {
    let temp = TestDir::new();
    let swapped = swap_parent_for_outside_symlink(&temp);

    swapped
        .pinned_parent
        .write_atomic("victim", b"written through pinned descriptor\n", None)
        .expect("atomic write through pinned parent failed");

    assert_eq!(
        fs::read(swapped.pinned_parent_path.join("victim")).expect("pinned victim disappeared"),
        b"written through pinned descriptor\n",
        "write did not affect the directory pinned before replacement"
    );
    assert_eq!(
        fs::read(&swapped.outside_victim).expect("outside victim disappeared"),
        b"outside sentinel\n",
        "write escaped through the replacement symlink"
    );
    assert!(
        fs::symlink_metadata(&swapped.parent_path)
            .expect("replacement symlink disappeared")
            .file_type()
            .is_symlink(),
        "write replaced the parent symlink"
    );
    assert_eq!(
        fs::read_link(&swapped.parent_path).expect("replacement symlink disappeared"),
        swapped.outside_path
    );
}

#[test]
fn unlink_stays_with_pinned_parent_after_path_replacement() {
    let temp = TestDir::new();
    let swapped = swap_parent_for_outside_symlink(&temp);

    swapped
        .pinned_parent
        .remove_file("victim")
        .expect("unlink through pinned parent failed");

    assert!(
        !swapped.pinned_parent_path.join("victim").exists(),
        "unlink did not remove the victim from the pinned directory"
    );
    assert_eq!(
        fs::read(&swapped.outside_victim).expect("outside victim disappeared"),
        b"outside sentinel\n",
        "unlink escaped through the replacement symlink"
    );
    assert!(
        fs::symlink_metadata(&swapped.parent_path)
            .expect("replacement symlink disappeared")
            .file_type()
            .is_symlink(),
        "unlink removed or replaced the parent symlink"
    );
    assert_eq!(
        fs::read_link(&swapped.parent_path).expect("replacement symlink disappeared"),
        swapped.outside_path
    );
}

#[test]
fn strict_root_rejects_symlinked_ancestor() {
    let temp = TestDir::new();
    let root = temp.0.join("root");
    let outside = temp.0.join("outside");
    let outside_root = outside.join("configured-root");
    let symlinked_ancestor = root.join("ancestor");
    let sentinel = outside.join("sentinel");
    fs::create_dir_all(&root).expect("failed to create root directory");
    fs::create_dir_all(&outside_root).expect("failed to create outside root");
    fs::write(&sentinel, b"outside sentinel\n").expect("failed to seed outside sentinel");
    symlink(&outside, &symlinked_ancestor).expect("failed to create symlinked ancestor");

    let outcome = RootFd::open_strict_root(&symlinked_ancestor.join("configured-root"));
    assert!(
        matches!(outcome, Err(_) | Ok(super::StrictRoot::Symlink)),
        "strict root accepted a path with a symlinked ancestor"
    );
    assert_eq!(
        fs::read(&sentinel).expect("outside sentinel disappeared"),
        b"outside sentinel\n",
        "strict root access modified the outside sentinel"
    );
}

#[test]
fn open_or_create_private_rejects_symlinked_ancestor_without_creating_outside() {
    let temp = TestDir::new();
    let root = temp.0.join("root");
    let outside = temp.0.join("outside");
    let symlinked_ancestor = root.join("ancestor");
    let outside_created_root = outside.join("created-root");
    let sentinel = outside.join("sentinel");
    fs::create_dir_all(&root).expect("failed to create root directory");
    fs::create_dir(&outside).expect("failed to create outside directory");
    fs::write(&sentinel, b"outside sentinel\n").expect("failed to seed outside sentinel");
    symlink(&outside, &symlinked_ancestor).expect("failed to create symlinked ancestor");

    let result = RootFd::open_or_create_private(&symlinked_ancestor.join("created-root"));
    assert!(
        result.is_err(),
        "private root creation succeeded through a symlinked ancestor"
    );
    assert!(
        !outside_created_root.exists(),
        "private root creation created a directory outside the configured root"
    );
    assert_eq!(
        fs::read(&sentinel).expect("outside sentinel disappeared"),
        b"outside sentinel\n",
        "private root creation modified the outside sentinel"
    );
}
