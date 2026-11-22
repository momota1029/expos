use super::*;

fn version(name: &str) -> VersionDir {
    VersionDir::parse(name).unwrap()
}

fn install(root: &Path, name: &str) -> VersionDir {
    let version = version(name);
    fs::create_dir(version.path(root)).unwrap();
    fs::write(version.executable(root), b"test payload").unwrap();
    version
}

#[test]
fn root_override_precedes_local_app_data() {
    let tmp = tempfile::tempdir().unwrap();
    let custom = tmp.path().join("custom root");
    let local = tmp.path().join("Local");
    assert_eq!(
        resolve_install_root(Some(custom.as_os_str()), Some(local.as_os_str())).unwrap(),
        custom
    );
    assert_eq!(
        resolve_install_root(Some(OsStr::new("")), Some(local.as_os_str())).unwrap(),
        local.join("expos")
    );
    assert!(resolve_install_root(None, None).is_err());
}

#[test]
fn version_dir_is_one_semver_path_component() {
    let name = "v1.2.3-beta.10+build.0001";
    assert_eq!(version(name).name(), name);
    assert_eq!(version(name).version().major, 1);
    for invalid in [
        "1.2.3",
        "V1.2.3",
        "v01.2.3",
        "v1.2.3-beta.01",
        "v1.2.3-alpha..1",
        "v1.2.3/../expos",
        "v1.2.3\\expos",
        "vC:1.2.3",
        "v1.2.3:stream",
        "v1.2.3 ",
    ] {
        assert!(VersionDir::parse(invalid).is_none(), "{invalid}");
    }
    assert!(VersionDir::parse(&format!("v1.0.0+{}", "x".repeat(250))).is_none());
}

#[test]
fn current_replacement_and_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    assert!(select_version(root).unwrap().is_none());
    let older = install(root, "v1.2.0");
    let newer = install(root, "v1.10.0");
    let release_candidate = install(root, "v1.10.0-rc.10");
    let prior_candidate = install(root, "v1.10.0-rc.2");
    fs::create_dir(root.join("v99.0.0")).unwrap();
    fs::write(root.join("v100.0.0"), b"not a directory").unwrap();
    assert_eq!(
        list_installed_versions(root).unwrap(),
        vec![
            newer.clone(),
            release_candidate,
            prior_candidate,
            older.clone()
        ]
    );
    assert_eq!(select_version(root).unwrap(), Some(newer.clone()));

    let lock = lock_install_root(root).unwrap();
    lock.activate(&newer).unwrap();
    lock.activate(&older).unwrap();
    assert_eq!(fs::read(root.join(CURRENT)).unwrap(), b"v1.2.0\n");
    assert_eq!(select_version(root).unwrap(), Some(older.clone()));
    assert!(lock.activate(&version("v99.0.0")).is_err());
    assert_eq!(read_current(root).unwrap(), Some(older.clone()));

    fs::write(root.join(CURRENT), b"v1.2.0\r\n").unwrap();
    assert_eq!(read_current(root).unwrap(), Some(older));
    for bad in [
        b"v1.2.0\n\n".as_slice(),
        b"v1.2.0\nv1.10.0",
        b"../v1.2.0",
        b"\xff",
    ] {
        fs::write(root.join(CURRENT), bad).unwrap();
        assert_eq!(read_current(root).unwrap(), None);
        assert_eq!(select_version(root).unwrap(), Some(newer.clone()));
    }
    fs::write(root.join(CURRENT), b"v99.0.0\n").unwrap();
    assert_eq!(select_version(root).unwrap(), Some(newer));
}

#[test]
fn prune_retains_newest_current_and_in_use() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let current = install(root, "v1.0.0");
    let running = install(root, "v1.1.0");
    let obsolete = install(root, "v1.2.0");
    let newest = install(root, "v1.3.0");
    let pin = File::open(running.executable(root)).unwrap();
    #[cfg(not(windows))]
    pin.lock_shared().unwrap();

    let lock = lock_install_root(root).unwrap();
    lock.activate(&current).unwrap();
    assert_eq!(lock.prune(1).unwrap(), vec![obsolete]);
    assert_eq!(
        list_installed_versions(root).unwrap(),
        vec![newest, running.clone(), current]
    );
    drop(pin);
    assert_eq!(lock.prune(1).unwrap(), vec![running]);
}
