//! `--profile-dir` end to end: the folder the CLI lists profiles from and
//! looks a `--profile` name up in, instead of the default one.

use std::path::PathBuf;
use std::process::{Command, Output};

/// A new, empty folder of its own for one test.
fn folder(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("talker-profile-dir-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn talker(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_talker"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn list_profiles_lists_the_chosen_folder() {
    let dir = folder("list");
    for file in ["gps.toml", "ais.toml", "notes.txt"] {
        std::fs::write(dir.join(file), "").unwrap();
    }

    let out = talker(&[
        "--profile-dir".as_ref(),
        dir.as_os_str(),
        "--list-profiles".as_ref(),
    ]);

    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let names: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(names, ["ais", "gps"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The name is looked for in the chosen folder: a name with no file there
/// is refused before anything opens, naming the file it looked for.
#[test]
fn a_profile_name_is_looked_for_in_the_chosen_folder() {
    let dir = folder("load");

    let out = talker(&[
        "--profile-dir".as_ref(),
        dir.as_os_str(),
        "--profile".as_ref(),
        "missing".as_ref(),
    ]);

    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let folder_name = dir.file_name().unwrap().to_string_lossy();
    assert!(
        stderr.contains(&*folder_name) && stderr.contains("missing.toml"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
