//! `svcd.conf` document and block-upsert tests for `settings`.

use super::*;

fn temp_file(name: &str, text: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = crate::platform::test_support::temp_dir().join(format!(
        "svcd-settings-{}-{}-{}",
        std::process::id(),
        nanos,
        name
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("svcd.conf");
    std::fs::write(&path, text).expect("write conf");
    path
}

#[test]
fn a_trailing_comment_is_part_of_the_value_syntax() {
    let settings = parse(
        "max_connections = 32   # half of nothing\nworkload = tiny # derived\n",
        Path::new("/etc/svcd"),
    )
    .expect("parse");

    assert_eq!(settings.max_connections, Some(32));
    assert_eq!(settings.workload, Some(Workload::Tiny));
}

#[test]
fn a_hash_inside_a_value_without_a_leading_space_survives() {
    let settings = parse("bind = 127.0.0.1:8787\n", Path::new("/etc/svcd")).expect("parse");
    assert!(settings.bind.is_some());

    let settings = parse("config_dir = /srv/svcd#1\n", Path::new("/")).expect("parse");
    assert_eq!(settings.config_dir, Some(PathBuf::from("/srv/svcd#1")));
}

#[test]
fn a_percentage_budget_parses_and_is_kept_apart_from_a_number() {
    let settings = parse("mem_budget_percent = 90\n", Path::new("/etc/svcd")).expect("percent");
    assert_eq!(settings.mem_budget_percent, Some(90));
    assert_eq!(settings.mem_budget_bytes, None);

    for value in ["0", "101", "-5"] {
        let error = parse(
            &format!("mem_budget_percent = {value}\n"),
            Path::new("/etc/svcd"),
        )
        .expect_err("out of range");
        assert!(error.contains("mem_budget_percent"), "{error}");
    }

    // One size, stated one way.
    let error = parse(
        "mem_budget_bytes = 52428800\nmem_budget_percent = 50\n",
        Path::new("/etc/svcd"),
    )
    .expect_err("both keys");
    assert!(error.contains("mutually exclusive"), "{error}");
}

#[test]
fn the_budget_and_the_workload_parse() {
    let settings = parse(
        "mem_budget_bytes = auto\nworkload = streaming\n",
        Path::new("/etc/svcd"),
    )
    .expect("auto");
    assert_eq!(settings.mem_budget_bytes, None);
    assert_eq!(settings.workload, Some(Workload::Streaming));

    let settings = parse("mem_budget_bytes = 536870912\n", Path::new("/etc/svcd")).expect("number");
    assert_eq!(settings.mem_budget_bytes, Some(512 * 1024 * 1024));

    let error = parse("workload = huge\n", Path::new("/etc/svcd")).expect_err("invalid");
    assert!(error.contains("tiny|interactive|streaming"), "{error}");
    let error = parse("mem_budget_bytes = lots\n", Path::new("/etc/svcd")).expect_err("invalid");
    assert!(error.contains("mem_budget_bytes"), "{error}");
}

#[test]
fn the_probe_inputs_parse_and_become_overrides() {
    let settings = parse(
        "probe_available_memory = 34359738368\n\
             probe_cgroup_remaining = 1073741824\n\
             probe_cpus = 2\n\
             probe_install_free_disk = 10737418240\n\
             probe_temp_free_disk = 5368709120\n",
        Path::new("/etc/svcd"),
    )
    .expect("parse");
    let overrides = settings.probe_overrides();

    assert_eq!(overrides.available_memory, Some(32 * 1024 * 1024 * 1024));
    assert_eq!(overrides.cgroup_remaining, Some(1024 * 1024 * 1024));
    assert_eq!(overrides.cpus, Some(2));
    assert_eq!(overrides.install_free_disk, Some(10 * 1024 * 1024 * 1024));
    assert_eq!(overrides.temp_free_disk, Some(5 * 1024 * 1024 * 1024));
    assert_eq!(overrides.stated_names().len(), 5);

    // Nothing stated: the probe reads every item.
    assert!(Settings::default()
        .probe_overrides()
        .stated_names()
        .is_empty());

    let error = parse("probe_cpus = many\n", Path::new("/etc/svcd")).expect_err("invalid");
    assert!(error.contains("probe_cpus"), "{error}");
}

#[test]
fn upsert_block_appends_once_and_replaces_in_place() {
    let path = temp_file("block", "bind = 127.0.0.1:8787\n");
    upsert_block(&path, "# BEGIN", "# END", "max_connections = 32\n").expect("append");
    upsert_block(&path, "# BEGIN", "# END", "max_connections = 64\n").expect("replace");
    let text = std::fs::read_to_string(&path).expect("read");

    assert_eq!(
        text,
        "bind = 127.0.0.1:8787\n\n# BEGIN\nmax_connections = 64\n# END\n"
    );
    let settings = parse(&text, Path::new("/")).expect("parse");
    assert_eq!(settings.max_connections, Some(64));
}

#[test]
fn upsert_block_keeps_everything_outside_the_block() {
    let path = temp_file(
        "keep",
        "bind = 127.0.0.1:8787\n# BEGIN\nmax_connections = 32\n# END\nmax_body_bytes = 65536\n",
    );
    upsert_block(&path, "# BEGIN", "# END", "max_streams = 8\n").expect("replace");

    let text = std::fs::read_to_string(&path).expect("read");
    assert_eq!(
        text,
        "bind = 127.0.0.1:8787\n# BEGIN\nmax_streams = 8\n# END\nmax_body_bytes = 65536\n"
    );
}

#[test]
fn an_unterminated_block_is_an_error_and_the_file_is_untouched() {
    let original = "# BEGIN\nmax_connections = 32\n";
    let path = temp_file("unterminated", original);

    let error =
        upsert_block(&path, "# BEGIN", "# END", "max_streams = 8\n").expect_err("must refuse");
    assert!(error.contains("no \"# END\" line"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), original);
}
