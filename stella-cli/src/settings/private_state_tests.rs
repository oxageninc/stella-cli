use std::path::Path;

use super::{AgentEngineConfig, project_settings_path, user_settings_path};

#[cfg(unix)]
#[test]
fn user_settings_are_private_but_project_settings_modes_are_untouched() {
    use std::os::unix::fs::PermissionsExt;

    let _env = crate::test_env::lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let user_dir = home.join(".stella");
    std::fs::create_dir_all(&user_dir).unwrap();
    std::fs::set_permissions(&user_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let previous_home = std::env::var_os("HOME");
    // SAFETY: serialized behind the binary-wide environment lock.
    unsafe { std::env::set_var("HOME", &home) };

    let engine = AgentEngineConfig::default();
    let user = user_settings_path().unwrap();
    engine.save_to(&user).unwrap();
    let mode = |path: &Path| {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&user_dir), 0o700);
    assert_eq!(mode(&user), 0o600);

    let workspace = tmp.path().join("workspace");
    let project_dir = workspace.join(".stella");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::set_permissions(&project_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let project = project_settings_path(&workspace);
    std::fs::write(&project, "{}\n").unwrap();
    std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o664)).unwrap();
    engine.save_to(&project).unwrap();
    assert_eq!(mode(&project_dir), 0o777);
    assert_eq!(mode(&project), 0o664);

    match previous_home {
        Some(previous) => unsafe { std::env::set_var("HOME", previous) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}

#[cfg(unix)]
#[test]
fn user_settings_save_rejects_a_symlink_without_touching_target() {
    use std::os::unix::fs::symlink;

    let _env = crate::test_env::lock();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let user_dir = home.join(".stella");
    std::fs::create_dir_all(&user_dir).unwrap();
    let previous_home = std::env::var_os("HOME");
    // SAFETY: serialized behind the binary-wide environment lock.
    unsafe { std::env::set_var("HOME", &home) };
    let user = user_settings_path().unwrap();
    let target = tmp.path().join("outside.json");
    std::fs::write(&target, "{}\n").unwrap();
    symlink(&target, &user).unwrap();

    assert!(AgentEngineConfig::default().save_to(&user).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "{}\n");

    match previous_home {
        Some(previous) => unsafe { std::env::set_var("HOME", previous) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}
