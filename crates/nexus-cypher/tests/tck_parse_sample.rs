//! Diagnostic helper: sample the first N parse errors per category and dump
//! the first line of the failing query + the parser error to stderr, so we
//! can categorise what the parser is missing.
//!
//! Run with:
//!   TCK_CATEGORY=clauses/match cargo test -p nexus-cypher --test tck_parse_sample -- --nocapture

use std::fs;
use std::path::PathBuf;

use nexus_cypher::parser::Parser;

const TCK_ROOT_REL: &str = "../../references/falkordb/tests/tck/features";

fn tck_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(TCK_ROOT_REL)
}

fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("feature") {
                out.push(p);
            }
        }
    }
}

/// Extract every triple-quoted docstring from a .feature file.
fn extract_queries(contents: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_doc = false;
    for line in contents.lines() {
        let t = line.trim_start();
        if t.starts_with("\"\"\"") {
            if in_doc {
                out.push(
                    std::mem::take(&mut current)
                        .trim_end_matches('\n')
                        .to_string(),
                );
            }
            in_doc = !in_doc;
            continue;
        }
        if in_doc {
            current.push_str(line);
            current.push('\n');
        }
    }
    out
}

#[test]
fn tck_parse_sample() {
    let filter = std::env::var("TCK_CATEGORY").ok();
    let root = tck_root();
    let mut features = Vec::new();
    walk(&root, &mut features);
    features.sort();

    let mut by_error: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    for path in &features {
        if let Some(f) = filter.as_deref() {
            if !path.to_string_lossy().contains(f) {
                continue;
            }
        }
        let Ok(contents) = fs::read_to_string(path) else {
            continue;
        };
        for q in extract_queries(&contents) {
            match Parser::parse(&q) {
                Ok(_) => {}
                Err(e) => {
                    let key = bucket_error(&e.to_string());
                    let sample = format!(
                        "{}: {}",
                        path.file_name().unwrap().to_string_lossy(),
                        truncate(&q, 100),
                    );
                    by_error.entry(key).or_default().push(sample);
                }
            }
        }
    }

    eprintln!();
    eprintln!("=== TCK parser error buckets ===");
    let mut buckets: Vec<_> = by_error.into_iter().collect();
    buckets.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    for (key, samples) in &buckets {
        eprintln!("\n[{}] count={}", key, samples.len());
        for s in samples.iter().take(5) {
            eprintln!("  {s}");
        }
    }
    eprintln!();
}

fn bucket_error(msg: &str) -> String {
    // Collapse similar errors into buckets. Keep the distinguishing token.
    if let Some(rest) = msg.strip_prefix("unexpected token: expected ") {
        // "expected X, got Y" — keep Y as the bucket key
        if let Some(got_idx) = rest.find(", got ") {
            let got = &rest[got_idx + 6..];
            return format!("got {}", &got[..got.len().min(40)]);
        }
    }
    msg.chars().take(60).collect()
}

fn truncate(s: &str, n: usize) -> String {
    let single_line: String = s.chars().filter(|c| *c != '\n').collect();
    if single_line.len() > n {
        format!("{}…", &single_line[..n])
    } else {
        single_line
    }
}
