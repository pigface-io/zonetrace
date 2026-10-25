//! zonetrace-join, run as a user runs it: timelines named, samples on
//! stdin.

use std::io::Write;
use std::process::{Command, Stdio};

fn run(args: &[&str], stdin: &str) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zonetrace-join"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the tool runs");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("samples written");
    let out = child.wait_with_output().expect("it ends");
    (
        out.status.code().expect("an exit code"),
        String::from_utf8(out.stdout).expect("text"),
        String::from_utf8(out.stderr).expect("text"),
    )
}

#[test]
fn samples_are_joined_to_a_written_timeline() {
    static WALK: zonetrace::Zone = zonetrace::Zone::new(1, "walk");
    zonetrace::timeline::start();
    drop(zonetrace::enter(&WALK));
    // A deep zone records as any other.
    drop(zonetrace::enter_deep(&WALK));
    let t = zonetrace::timeline::stop().expect("recording");
    let path = std::env::temp_dir().join(format!("zonetrace-join-{}.timeline", std::process::id()));
    let mut file = std::fs::File::create(&path).expect("created");
    t.write(&mut file).expect("written");
    let text = std::fs::read_to_string(&path).expect("read");
    let first = text
        .lines()
        .find_map(|l| l.strip_prefix("e "))
        .and_then(|l| l.split(' ').next())
        .expect("an enter");
    let tid = text
        .lines()
        .nth(1)
        .expect("a thread line")
        .trim_start_matches("thread ");
    let at: u64 = first.parse().expect("a time");
    let samples = format!(
        "{tid} {}.{:09}: 42 cycles:\nnot a sample\n",
        at / 1_000_000_000,
        at % 1_000_000_000
    );
    let (code, out, err) = run(&[path.to_str().expect("a path")], &samples);
    std::fs::remove_file(&path).expect("removed");
    assert_eq!(code, 0, "{err}");
    let walk = out.lines().find(|l| l.starts_with("walk ")).expect("walk");
    assert!(walk.contains(" 42 "), "{out}");
    assert!(
        err.contains("1 samples, 0 outside every timeline, 1 lines not samples"),
        "{err}"
    );
}

#[test]
fn a_timeline_that_does_not_read_is_an_error() {
    let (code, _, err) = run(&["/nonexistent/zonetrace.timeline"], "");
    assert_eq!(code, 2);
    assert!(
        err.starts_with("zonetrace-join: /nonexistent/zonetrace.timeline:"),
        "{err}"
    );
}
