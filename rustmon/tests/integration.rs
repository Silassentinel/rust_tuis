//! End-to-end tests against the **compiled binary**, not the library.
//!
//! Everything else in this crate tests functions directly; this file is the
//! one place that runs `target/.../rustmon` itself via
//! [`std::process::Command`] and checks what a real invocation actually
//! prints and exits with — the CLI parsing, the config validation, and the
//! renderer dispatch all wired together the way a user would actually hit
//! them, not just individually unit-tested.
//!
//! `CARGO_BIN_EXE_rustmon` is set by Cargo for integration tests in a crate
//! that has a `[[bin]]` target — no path-guessing needed.
//!
//! Chunk 11 asked for this against a *fixture* `--sysfs-root`, not the real
//! machine: fixtures are deterministic across every machine this test suite
//! ever runs on, where the real `/` is not.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Hand-rolled fixture-tree builder, the same shape as the library's own
/// `sysfs::tests::TempTree` — duplicated rather than shared because this file
/// compiles as a separate crate against `rustmon`'s public API and has no
/// access to that `#[cfg(test)]`-only, crate-internal helper.
struct FixtureRoot {
    root: PathBuf,
}

impl FixtureRoot {
    fn new(tag: &str) -> Self {
        let mut root = std::env::temp_dir();
        root.push(format!(
            "rustmon-integration-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create fixture root");
        let root = std::fs::canonicalize(&root).expect("canonicalise fixture root");
        FixtureRoot { root }
    }

    fn file(&self, rel: &str, contents: &str) -> &Self {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).expect("create fixture parent");
        let mut f = std::fs::File::create(&path).expect("create fixture file");
        f.write_all(contents.as_bytes()).expect("write fixture file");
        self
    }

    fn dir(&self, rel: &str) -> &Self {
        std::fs::create_dir_all(self.root.join(rel)).expect("create fixture dir");
        self
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A fixture tree with one of everything: CPU, memory, one hwmon chip, one
/// disk, one mount, one network interface. Real-shaped values (not `0`
/// everywhere) so a field actually being wrong would show up as a wrong
/// number, not just a missing one.
fn full_fixture(tag: &str) -> FixtureRoot {
    let f = FixtureRoot::new(tag);

    f.file(
        "proc/stat",
        "cpu  100 0 0 900 0 0 0 0 0 0\ncpu0 100 0 0 900 0 0 0 0 0 0\nctxt 12345\nbtime 1700000000\n",
    );
    f.file("proc/loadavg", "0.10 0.20 0.30 1/200 999\n");
    f.file("proc/cpuinfo", "model name\t: Fixture CPU\n");
    f.file(
        "proc/meminfo",
        "MemTotal: 1000 kB\nMemFree: 200 kB\nMemAvailable: 400 kB\nBuffers: 50 kB\nCached: 100 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
    );

    f.file("sys/class/hwmon/hwmon0/name", "fixturechip\n");
    f.file("sys/class/hwmon/hwmon0/temp1_input", "42000\n");
    f.file("sys/class/hwmon/hwmon0/temp1_label", "Core\n");

    f.file(
        "proc/diskstats",
        "   8       0 sda 10 0 20 30 5 0 40 50 0 60 70 0 0 0 0 0 0\n",
    );
    f.file("proc/self/mounts", "/dev/sda1 / ext4 rw,relatime 0 0\n");

    f.file(
        "proc/net/dev",
        "Inter-|   Receive                                                |  Transmit\n \
          face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n \
           eth0: 111 2 0 0 0 0 0 0 333 4 0 0 0 0 0 0\n",
    );
    f.file("sys/class/net/eth0/operstate", "up\n");
    f.file("sys/class/net/eth0/mtu", "1500\n");

    // No GPU in this fixture — `sys/class/drm` exists but is empty, which
    // must yield an absent `gpu` section, not an error.
    f.dir("sys/class/drm");

    f
}

fn rustmon() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rustmon"))
}

fn run(args: &[&str]) -> Output {
    rustmon().args(args).output().expect("binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("valid utf-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("valid utf-8 stderr")
}

// ---- --once --format json against a fixture ---------------------------------

#[test]
fn once_json_against_a_fixture_root_is_valid_and_field_correct() {
    let fixture = full_fixture("json");
    let root = fixture.path().to_str().expect("path is utf-8");

    let output = run(&["--once", "--format", "json", "--sysfs-root", root]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);

    let value = parse_json(&text);

    assert_eq!(value.get_i64("schema"), Some(1));
    assert_eq!(value.get_str("cpu.model"), Some("Fixture CPU"));
    assert_eq!(value.get_i64("cpu.cores"), Some(1));
    // 1000 kB - 400 kB kept as MemAvailable -> total 1000*1024, used
    // (total - available) = (1000-400)*1024 = 614400.
    assert_eq!(value.get_i64("memory.total_bytes"), Some(1_024_000));
    assert_eq!(value.get_i64("memory.used_bytes"), Some(614_400));
    assert_eq!(value.get_str("thermal.0.name"), Some("fixturechip"));
    assert_eq!(value.get_str("thermal.0.temps.0.label"), Some("Core"));
    assert_eq!(value.get_str("disk.0.name"), Some("sda"));
    assert_eq!(value.get_str("mounts.0.mount_point"), Some("/"));
    assert_eq!(value.get_str("net.0.name"), Some("eth0"));
    assert_eq!(value.get_str("net.0.operstate"), Some("up"));
    assert_eq!(value.get_i64("net.0.mtu"), Some(1500));

    // No GPU in the fixture: the section must be entirely absent, not an
    // empty array or a null.
    assert!(!text.contains("\"gpu\""), "unexpected gpu section: {text}");
}

/// `--once` (no `--format`) defaults to text, and it must actually be
/// readable text, not JSON, against the same fixture.
#[test]
fn once_defaults_to_text_and_contains_the_fixture_values() {
    let fixture = full_fixture("text");
    let root = fixture.path().to_str().unwrap();

    let output = run(&["--once", "--sysfs-root", root]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let text = stdout(&output);
    assert!(text.starts_with("rustmon —"), "{text}");
    assert!(text.contains("Fixture CPU"));
    assert!(text.contains("fixturechip"));
    assert!(text.contains("sda"));
    assert!(text.contains("eth0"));
    assert!(!text.trim_start().starts_with('{'), "expected text, got JSON-shaped output");
}

/// `--only` restricts which sections are collected — a fixture with every
/// source file present but `--only cpu` must show CPU and nothing else.
#[test]
fn only_flag_restricts_the_output_against_a_real_run() {
    let fixture = full_fixture("only");
    let root = fixture.path().to_str().unwrap();

    let output = run(&["--once", "--format", "json", "--sysfs-root", root, "--only", "cpu"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let text = stdout(&output);
    assert!(text.contains("\"cpu\""));
    for absent in ["\"memory\"", "\"thermal\"", "\"disk\"", "\"net\""] {
        assert!(!text.contains(absent), "unexpected {absent} with --only cpu: {text}");
    }
}

#[test]
fn verbose_includes_errors_and_quiet_does_not() {
    // A fixture with an unreadable /proc/stat forces the cpu collector to
    // fail (propagated, per chunk 3/6's fail-hard-on-primary-source rule),
    // which is exactly the kind of failure --verbose exists to surface.
    let fixture = FixtureRoot::new("verbose");
    fixture.file("proc/stat", "not the right shape at all\n");
    let root = fixture.path().to_str().unwrap();

    let quiet = run(&["--once", "--format", "json", "--sysfs-root", root]);
    assert!(!stdout(&quiet).contains("\"errors\""));

    let verbose = run(&["--once", "--format", "json", "--sysfs-root", root, "--verbose"]);
    assert!(stdout(&verbose).contains("\"errors\""));
    assert!(stdout(&verbose).contains("\"collector\":\"cpu\""));
}

// ---- CLI surface, against the real binary -----------------------------------

#[test]
fn help_and_version_exit_zero_and_do_not_touch_the_filesystem() {
    let help = run(&["--help"]);
    assert!(help.status.success());
    assert!(stdout(&help).contains("USAGE"));

    let version = run(&["--version"]);
    assert!(version.status.success());
    assert!(stdout(&version).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn an_unknown_flag_exits_non_zero_with_a_message_on_stderr() {
    let output = run(&["--this-flag-does-not-exist"]);
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("unknown flag"));
    assert!(stdout(&output).is_empty(), "an error must not also print output");
}

#[test]
fn a_nonexistent_sysfs_root_exits_non_zero_with_a_message_on_stderr() {
    let output = run(&["--once", "--sysfs-root", "/this/path/really/should/not/exist"]);
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(1));
    assert!(!stderr(&output).is_empty());
}

/// The read-only guarantee, proven operationally rather than just asserted
/// in docs: running the binary against a fixture root must never create,
/// modify, or delete anything under that root.
#[test]
fn running_once_never_writes_to_the_sysfs_root() {
    let fixture = full_fixture("readonly");
    let root = fixture.path().to_path_buf();

    let before = snapshot_tree(&root);
    let output = run(&["--once", "--format", "json", "--sysfs-root", root.to_str().unwrap()]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let after = snapshot_tree(&root);

    assert_eq!(before, after, "the sysfs root changed after a read-only run");
}

fn snapshot_tree(root: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                let contents = std::fs::read_to_string(&path).unwrap_or_default();
                out.push((path, contents));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

// ---- minimal JSON value access for assertions --------------------------------

/// Just enough of a JSON reader to assert on specific fields by dotted path
/// (`"cpu.model"`, `"thermal.0.name"`) — not a general-purpose parser. Same
/// reasoning as `render::json`'s own test-only structural validator: a crate
/// for this would need crate-checklist sign-off for something a few hundred
/// lines of test code already covers.
#[derive(Debug, Clone)]
enum Json {
    Null,
    // The current schema has no boolean field, so nothing reads this today —
    // kept so the parser stays correct for `true`/`false` literals if one is
    // ever added, rather than treating them as a parse error.
    #[allow(dead_code)]
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

fn parse_json(s: &str) -> Json {
    let mut chars = s.trim().chars().peekable();
    let value = parse_value(&mut chars);
    assert!(chars.next().is_none(), "trailing content after top-level JSON value");
    value
}

fn parse_value(chars: &mut std::iter::Peekable<std::str::Chars>) -> Json {
    skip_ws(chars);
    match *chars.peek().expect("unexpected end of input") {
        '{' => parse_object(chars),
        '[' => parse_array(chars),
        '"' => Json::String(parse_string(chars)),
        't' => {
            expect_literal(chars, "true");
            Json::Bool(true)
        }
        'f' => {
            expect_literal(chars, "false");
            Json::Bool(false)
        }
        'n' => {
            expect_literal(chars, "null");
            Json::Null
        }
        _ => Json::Number(parse_number(chars)),
    }
}

fn skip_ws(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while chars.next_if(|c| c.is_whitespace()).is_some() {}
}

fn expect_literal(chars: &mut std::iter::Peekable<std::str::Chars>, lit: &str) {
    for expected in lit.chars() {
        assert_eq!(chars.next(), Some(expected), "expected literal {lit:?}");
    }
}

fn parse_object(chars: &mut std::iter::Peekable<std::str::Chars>) -> Json {
    assert_eq!(chars.next(), Some('{'));
    let mut members = Vec::new();
    skip_ws(chars);
    if chars.peek() == Some(&'}') {
        chars.next();
        return Json::Object(members);
    }
    loop {
        skip_ws(chars);
        let key = parse_string(chars);
        skip_ws(chars);
        assert_eq!(chars.next(), Some(':'));
        let value = parse_value(chars);
        members.push((key, value));
        skip_ws(chars);
        match chars.next() {
            Some(',') => continue,
            Some('}') => break,
            other => panic!("expected ',' or '}}', got {other:?}"),
        }
    }
    Json::Object(members)
}

fn parse_array(chars: &mut std::iter::Peekable<std::str::Chars>) -> Json {
    assert_eq!(chars.next(), Some('['));
    let mut items = Vec::new();
    skip_ws(chars);
    if chars.peek() == Some(&']') {
        chars.next();
        return Json::Array(items);
    }
    loop {
        items.push(parse_value(chars));
        skip_ws(chars);
        match chars.next() {
            Some(',') => continue,
            Some(']') => break,
            other => panic!("expected ',' or ']', got {other:?}"),
        }
    }
    Json::Array(items)
}

fn parse_string(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    assert_eq!(chars.next(), Some('"'));
    let mut out = String::new();
    loop {
        match chars.next().expect("unterminated string") {
            '"' => break,
            '\\' => match chars.next().expect("dangling escape") {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let hex: String = (0..4).map(|_| chars.next().expect("short \\u escape")).collect();
                    let code = u32::from_str_radix(&hex, 16).expect("valid hex in \\u escape");
                    out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                }
                other => panic!("unknown escape \\{other}"),
            },
            c => out.push(c),
        }
    }
    out
}

fn parse_number(chars: &mut std::iter::Peekable<std::str::Chars>) -> f64 {
    let mut raw = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E') {
            raw.push(c);
            chars.next();
        } else {
            break;
        }
    }
    raw.parse().expect("valid JSON number")
}

impl Json {
    /// Navigate a dotted path like `"thermal.0.temps.0.label"` — a numeric
    /// segment indexes an array, anything else indexes an object member.
    fn get(&self, path: &str) -> Option<&Json> {
        let mut current = self;
        for segment in path.split('.') {
            current = match (current, segment.parse::<usize>()) {
                (Json::Array(items), Ok(idx)) => items.get(idx)?,
                (Json::Object(members), _) => &members.iter().find(|(k, _)| k == segment)?.1,
                _ => return None,
            };
        }
        Some(current)
    }

    fn get_str(&self, path: &str) -> Option<&str> {
        match self.get(path)? {
            Json::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn get_i64(&self, path: &str) -> Option<i64> {
        match self.get(path)? {
            Json::Number(n) => Some(*n as i64),
            _ => None,
        }
    }
}
