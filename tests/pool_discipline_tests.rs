//! One request, one pooled connection at a time.
//!
//! A handler that holds a transaction (or an acquired connection) and then
//! reaches for the pool again needs TWO connections. With a per-org pool of 5,
//! five such requests each hold one and wait forever for a second: the org
//! stalls until the acquire timeout. `/sync/pull` did exactly that (and so did
//! ticket fires, booking writes, table swaps, payroll generation, …).
//!
//! This is a source lint, run as a test: inside the scope of
//! `let <tx> = <pool>.begin()` / `.acquire()`, the pool variable must not be
//! used again until `<tx>` is committed, rolled back or dropped (a release
//! inside a nested block covers the rest of that block, e.g. `tx.rollback()`
//! then `return`). A function may not take both a transaction/connection and a
//! pool either. It is a heuristic over the text, not a borrow checker, but it
//! found every instance fixed with it; the runtime guard for the hot path is
//! `sync::pull::tests::pull_concurrent_pulls_on_small_pool_all_complete`.
use regex::Regex;
use std::path::Path;

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs")
            && !p.file_name().unwrap().to_string_lossy().contains("test")
        {
            out.push(p);
        }
    }
}

fn indent(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

fn violations(src: &str) -> Vec<(usize, String)> {
    let begin = Regex::new(r"let\s+(?:mut\s+)?(\w+)\s*(?::[^=]+)?=\s*(\w+)(?:\.get_ref\(\))?\s*\.(?:begin|acquire)\(\)").unwrap();
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(m) = begin.captures(l) else { continue };
        let (tx, pool) = (&m[1], &m[2]);
        let release = Regex::new(&format!(r"\b{tx}\.(commit|rollback)\(\)|drop\({tx}\)")).unwrap();
        let uses_pool = Regex::new(&format!(r"(^|[^\w.]){pool}\b")).unwrap();
        let base = indent(l);
        let mut skip_from: Option<usize> = None;
        for (j, s) in lines.iter().enumerate().skip(i + 1) {
            let t = s.trim();
            if t.is_empty() {
                continue;
            }
            let ind = indent(s);
            if t.starts_with('}') && ind < base {
                break;
            }
            if let Some(k) = skip_from {
                if ind < k {
                    skip_from = None;
                } else {
                    continue;
                }
            }
            if release.is_match(s) {
                if ind == base {
                    break;
                }
                skip_from = Some(ind);
                continue;
            }
            if t.starts_with("//") || t.contains("fn ") {
                continue;
            }
            // `pool: ...` is a field/param name, not a use.
            if uses_pool.is_match(s)
                && !Regex::new(&format!(r"\b{pool}\s*:[^:]"))
                    .unwrap()
                    .is_match(s)
            {
                out.push((j + 1, t.to_string()));
            }
        }
    }
    let both = Regex::new(r"fn\s+\w+\s*(?:<[^>]*>)?\s*\(([^)]*)\)").unwrap();
    for m in both.captures_iter(src) {
        let args = &m[1];
        if (args.contains("Transaction") || args.contains("PgConnection"))
            && (args.contains("PgPool") || args.contains("db::Db"))
        {
            let line = src[..m.get(0).unwrap().start()].lines().count() + 1;
            out.push((
                line,
                format!("takes a transaction/connection AND a pool: {}", m[0].trim()),
            ));
        }
    }
    out
}

#[test]
fn no_handler_takes_a_second_pooled_connection_while_holding_one() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut all = Vec::new();
    for f in files {
        for (line, text) in violations(&std::fs::read_to_string(&f).unwrap()) {
            all.push(format!(
                "{}:{line}: {text}",
                f.strip_prefix(&root).unwrap().display()
            ));
        }
    }
    assert!(
        all.is_empty(),
        "pool used while a transaction/connection is held:\n{}",
        all.join("\n")
    );
}

#[test]
fn pool_lint_catches_the_patterns_it_exists_for() {
    let bad = "async fn h(pool: &PgPool) {
    let mut tx = pool.begin().await?;
    project(pool, &mut tx).await?;
    tx.commit().await?;
}";
    assert_eq!(violations(bad).len(), 1);
    let ok_after_commit = "async fn h(pool: &PgPool) {
    let mut tx = pool.begin().await?;
    x(&mut tx).await?;
    if gone {
        tx.rollback().await?;
        return fetch(pool).await;
    }
    tx.commit().await?;
    publish(pool).await;
}";
    assert!(
        violations(ok_after_commit).is_empty(),
        "{:?}",
        violations(ok_after_commit)
    );
    let both = "pub async fn fire(tx: &mut Transaction<'_, Postgres>, pool: &PgPool) {}";
    assert_eq!(violations(both).len(), 1);
}
