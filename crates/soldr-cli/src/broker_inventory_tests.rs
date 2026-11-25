//! soldr#3193: classification of soldr processes by the `HOME` they serve.

use super::*;

fn record(pid: u32, exe: &str, home: Option<&str>) -> ProcessRecord {
    ProcessRecord {
        pid,
        exe: PathBuf::from(exe),
        cmd: Vec::new(),
        home: home.map(PathBuf::from),
        start_time: u64::from(pid),
    }
}

#[test]
fn only_soldr_images_are_inventoried() {
    let own = Path::new("/home/me");
    let records = [
        record(1, "/usr/bin/bash", Some("/home/me")),
        record(2, "/tmp/x/.soldr/broker/soldr-broker", Some("/tmp/x")),
        record(3, "/tmp/x/.soldr/bin/soldr-daemon", Some("/tmp/x")),
        record(4, "/tmp/x/.soldr/bin/soldr", Some("/tmp/x")),
    ];
    let inventory = classify(&records, own);
    assert_eq!(inventory.own_processes, 0);
    let roles: Vec<_> = inventory.leaked.iter().map(|p| (p.pid, p.role)).collect();
    assert_eq!(roles, vec![(2, Role::Broker), (3, Role::Daemon)]);
}

#[test]
fn processes_serving_the_own_home_are_never_leaked() {
    let own = Path::new("/home/me");
    let records = [
        record(2, "/home/me/.soldr/broker/soldr-broker", Some("/home/me")),
        record(3, "/home/me/.soldr/bin/soldr-daemon", Some("/home/me")),
        record(
            4,
            "/tmp/other/.soldr/broker/soldr-broker",
            Some("/tmp/other"),
        ),
    ];
    let inventory = classify(&records, own);
    assert_eq!(inventory.own_processes, 2);
    assert_eq!(inventory.leaked.len(), 1);
    assert_eq!(inventory.leaked[0].pid, 4);
    assert_eq!(inventory.leaked_brokers(), 1);
    assert_eq!(inventory.leaked_daemons(), 0);
}

#[test]
fn an_unreadable_environment_falls_back_to_the_install_layout() {
    // Another user's broker: environ is unreadable, but the staged image
    // path names its HOME.
    let own = Path::new("/home/me");
    let records = [
        record(2, "/home/me/.soldr/broker/soldr-broker", None),
        record(3, "/tmp/other/.soldr/broker/soldr-broker", None),
    ];
    let inventory = classify(&records, own);
    assert_eq!(inventory.own_processes, 1);
    assert_eq!(inventory.leaked.len(), 1);
    assert_eq!(inventory.leaked[0].home.as_deref(), Some("/tmp/other"));
}

#[test]
fn a_daemon_with_no_home_at_all_is_still_reported() {
    let own = Path::new("/home/me");
    let records = [record(3, "/opt/soldr-daemon", None)];
    let inventory = classify(&records, own);
    assert_eq!(inventory.leaked.len(), 1);
    assert_eq!(inventory.leaked[0].home, None);
    assert!(!inventory.leaked[0].home_present);
}

#[test]
fn a_missing_home_is_flagged_and_an_existing_one_is_not() {
    let temp = tempfile::tempdir().expect("tempdir");
    let present = temp.path().join("present");
    std::fs::create_dir_all(&present).expect("mkdir");
    let gone = temp.path().join("gone");
    let records = [
        record(2, "/x/.soldr/broker/soldr-broker", present.to_str()),
        record(3, "/y/.soldr/broker/soldr-broker", gone.to_str()),
    ];
    let inventory = classify(&records, Path::new("/home/me"));
    assert_eq!(inventory.leaked_with_missing_home(), 1);
    assert!(inventory.leaked[0].home_present);
    assert!(!inventory.leaked[1].home_present);
}

#[test]
fn brokers_sort_before_daemons_oldest_first() {
    let records = [
        record(30, "/c/.soldr/bin/soldr-daemon", Some("/c")),
        record(20, "/b/.soldr/broker/soldr-broker", Some("/b")),
        record(10, "/a/.soldr/broker/soldr-broker", Some("/a")),
    ];
    let inventory = classify(&records, Path::new("/home/me"));
    let pids: Vec<_> = inventory.leaked.iter().map(|p| p.pid).collect();
    assert_eq!(pids, vec![10, 20, 30]);
}

#[test]
fn image_name_variants_are_recognised() {
    assert_eq!(
        Role::of_executable(Path::new("/x/.soldr/broker/soldr-broker.exe")),
        Some(Role::Broker)
    );
    assert_eq!(
        Role::of_executable(Path::new("/x/soldr-daemon.exe")),
        Some(Role::Daemon)
    );
    // Linux: the image was unlinked under the running process.
    assert_eq!(
        Role::of_executable(Path::new("/x/.soldr/broker/soldr-broker (deleted)")),
        Some(Role::Broker)
    );
    assert_eq!(Role::of_executable(Path::new("/x/soldr")), None);
    assert_eq!(Role::of_executable(Path::new("/x/soldr-brokerage")), None);
}

#[test]
fn a_deleted_image_still_yields_its_home_from_the_install_layout() {
    let records = [record(
        2,
        "/tmp/gone/.soldr/broker/soldr-broker (deleted)",
        None,
    )];
    let inventory = classify(&records, Path::new("/home/me"));
    assert_eq!(inventory.leaked.len(), 1);
    assert_eq!(inventory.leaked[0].home.as_deref(), Some("/tmp/gone"));
    assert!(!inventory.leaked[0].home_present);
}

#[test]
fn the_toast_names_the_counts_the_remedy_and_the_silencer() {
    let records = [
        record(2, "/x/.soldr/broker/soldr-broker", Some("/nonexistent/x")),
        record(3, "/x/.soldr/bin/soldr-daemon", Some("/nonexistent/x")),
    ];
    let text = toast_text(&classify(&records, Path::new("/home/me")));
    assert!(
        text.contains("1 leaked soldr-broker and 1 leaked soldr-daemon"),
        "{text}"
    );
    assert!(text.contains("(2 whose HOME no longer exists)"), "{text}");
    assert!(text.contains(BROKER_PURGE_COMMAND), "{text}");
    assert!(text.contains(&format!("{TOAST_ENV}=0")), "{text}");
}

#[test]
fn the_scripted_process_table_round_trips_through_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    let file = temp.path().join("procs.json");
    let records = vec![record(7, "/x/.soldr/broker/soldr-broker", Some("/x"))];
    std::fs::write(&file, serde_json::to_vec(&records).expect("json")).expect("write");
    assert_eq!(scripted_process_table(&file), records);
    assert!(scripted_process_table(&temp.path().join("missing.json")).is_empty());
}

#[test]
fn a_fresh_stamp_is_fresh_and_an_old_one_is_not() {
    let temp = tempfile::tempdir().expect("tempdir");
    let stamp = temp.path().join("a").join("b").join(TOAST_STAMP_FILE);
    assert!(!stamp_is_fresh(&stamp, Duration::from_secs(60)));
    touch_stamp(&stamp);
    assert!(stamp_is_fresh(&stamp, Duration::from_secs(60)));
    assert!(!stamp_is_fresh(&stamp, Duration::ZERO));
}

#[test]
fn a_foreground_broker_serve_is_a_broker_too() {
    let mut foreground = record(9, "/x/target/debug/soldr", Some("/tmp/fixture"));
    foreground.cmd = vec!["soldr".into(), "broker".into(), "serve".into()];
    let mut other = record(10, "/x/target/debug/soldr", Some("/tmp/fixture"));
    other.cmd = vec!["soldr".into(), "broker".into(), "status".into()];
    let inventory = classify(&[foreground, other], Path::new("/home/me"));
    let pids: Vec<_> = inventory.leaked.iter().map(|p| p.pid).collect();
    assert_eq!(pids, vec![9]);
}

#[test]
fn the_doctor_view_keeps_exact_counts_and_a_bounded_sample() {
    let records: Vec<ProcessRecord> = (1..=(DOCTOR_LISTED_ROWS as u32 + 5))
        .map(|n| record(n, "/x/.soldr/broker/soldr-broker", Some("/nonexistent/x")))
        .collect();
    let view = DoctorInventory::from_inventory(&classify(&records, Path::new("/home/me")));
    assert_eq!(view.leaked_brokers, DOCTOR_LISTED_ROWS + 5);
    assert_eq!(view.leaked_with_missing_home, DOCTOR_LISTED_ROWS + 5);
    assert_eq!(view.leaked.len(), DOCTOR_LISTED_ROWS);
    assert_eq!(view.leaked_omitted, 5);

    let small = DoctorInventory::from_inventory(&classify(&records[..3], Path::new("/home/me")));
    assert_eq!(small.leaked.len(), 3);
    assert_eq!(small.leaked_omitted, 0);
}

#[test]
fn the_process_home_prefers_userprofile_then_home() {
    let both = ["HOME=/h".to_string(), "USERPROFILE=/u".to_string()];
    assert_eq!(env_home_of(&both), Some(PathBuf::from("/u")));
    let home_only = ["PATH=/bin".to_string(), "HOME=/h".to_string()];
    assert_eq!(env_home_of(&home_only), Some(PathBuf::from("/h")));
    let empty = ["HOME=".to_string()];
    assert_eq!(env_home_of(&empty), None);
}

#[test]
fn foreground_broker_is_recognized_only_at_the_command_position() {
    let role_for = |args: &[&str]| {
        let cmd: Vec<String> = args.iter().map(|arg| (*arg).into()).collect();
        Role::of_process(Path::new("soldr"), &cmd)
    };
    assert_eq!(
        role_for(&[
            "soldr",
            "--jobs",
            "2",
            "broker",
            "--allow-unpinned",
            "serve"
        ]),
        Some(Role::Broker)
    );
    assert_eq!(
        role_for(&["soldr", "cargo", "test", "--", "broker", "serve"]),
        None
    );
}
