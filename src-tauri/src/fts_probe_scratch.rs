#[cfg(test)]
mod fts_probe {
    #[test]
    fn pure_fts_check() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE VIRTUAL TABLE t USING fts5(a, b)")
            .unwrap();
        c.execute("INSERT INTO t(rowid, a, b) VALUES (1, 'x', 'y')", [])
            .unwrap();
        let m: i64 = c
            .query_row("SELECT count(*) FROM t WHERE t MATCH 'x'", [], |r| r.get(0))
            .unwrap();
        println!("match: {m}");
        let d1 = c.execute(
            "INSERT INTO t(t, rowid, a, b) VALUES ('delete', 1, 'x', 'y')",
            [],
        );
        println!("delete via execute: {d1:?}");
    }
}
