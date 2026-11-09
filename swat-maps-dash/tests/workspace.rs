//! W6: no embedded database engine anywhere in the crate's tree, with every
//! feature on. Connectors are sans-IO over HTTP; a host that wants a native
//! Postgres client implements `hook::Source` in its own crate.

use std::process::Command;

const FORBIDDEN: [&str; 7] = ["duckdb", "rusqlite", "libsqlite3-sys", "postgres", "tokio-postgres", "sqlx", "clickhouse"];

#[test]
fn no_embedded_database_in_the_tree() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let out = match Command::new(&cargo)
        .args(["tree", "--offline", "--manifest-path", manifest, "-p", "swat-maps-dash", "--all-features", "-e", "normal,build", "--prefix", "none"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipped: cargo is not runnable here ({e})");
            return;
        }
    };
    assert!(out.status.success(), "cargo tree failed: {}", String::from_utf8_lossy(&out.stderr));
    let tree = String::from_utf8_lossy(&out.stdout);
    assert!(tree.lines().count() > 5, "the tree is not empty: {tree}");
    for line in tree.lines() {
        let name = line.split_whitespace().next().unwrap_or("");
        assert!(!FORBIDDEN.contains(&name), "W6: {name} is in swat-maps-dash's tree:\n{tree}");
    }
}
