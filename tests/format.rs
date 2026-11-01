//! The report formats as the JavaScript port under js/ must reproduce
//! them: one script of zones, its table and rows with the time columns
//! masked, held in js/fixtures/. `UPDATE_FIXTURES=1 cargo test --test
//! format` rewrites the fixture; js/zonetrace.test.ts reads it.

use zonetrace::{Zone, copied, enter, read, take};

static OUTER: Zone = Zone::new(1, "outer");
static INNER: Zone = Zone::new(2, "inner");
static OTHER: Zone = Zone::new(3, "other");

fn script() {
    {
        let _o = enter(&OUTER);
        read(1_234_567);
        {
            let _i = enter(&INNER);
            copied(500_000);
        }
        {
            let _i = enter(&INNER);
            copied(123_456);
        }
    }
    let _t = enter(&OTHER);
    read(2_000_000);
}

/// The table with total ms, % and self ms replaced by X: the label and
/// calls columns are kept, then three numeric columns go.
fn mask_text(s: &str) -> String {
    s.lines()
        .enumerate()
        .map(|(k, line)| {
            if k == 0 || line.is_empty() {
                return line.to_owned();
            }
            // label (40) + calls (right-aligned in 11), then three numbers
            let (head, rest) = line.split_at(51);
            let mut fields = rest.split_whitespace();
            let _ = (fields.next(), fields.next(), fields.next());
            let tail: Vec<&str> = fields.collect();
            format!("{head} X X X {}", tail.join(" "))
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn mask_rows(s: &str) -> String {
    s.lines()
        .map(|line| {
            let a = line.find("\"nanos\":").expect("nanos");
            let b = line.find(",\"read\":").expect("read");
            format!(
                "{}\"nanos\":X,\"net\":X,\"self\":X{}",
                &line[..a],
                &line[b..]
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[test]
fn the_formats_match_the_fixture() {
    let _ = take();
    script();
    let report = take();
    let text = mask_text(&report.text(4.0));
    let rows = mask_rows(&report.ndjson(4.0));
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/js/fixtures");
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(dir).expect("fixtures dir");
        std::fs::write(format!("{dir}/tree.txt"), &text).expect("write");
        std::fs::write(format!("{dir}/tree.ndjson"), &rows).expect("write");
    }
    let want_text = std::fs::read_to_string(format!("{dir}/tree.txt")).expect("tree.txt");
    let want_rows = std::fs::read_to_string(format!("{dir}/tree.ndjson")).expect("tree.ndjson");
    assert_eq!(text, want_text);
    assert_eq!(rows, want_rows);
}
