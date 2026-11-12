# repro-turso-prepare-dangling-tx

Repro for a transaction bug in the `turso` Rust crate. When a `Transaction` is dropped without
`commit` or `rollback`, the crate rolls it back on the next call on the connection. Statements from
`Connection::prepare` and `Connection::prepare_cached` skip that step, so they run inside the
transaction that should be gone.

## Status

Not reported upstream yet. No issue or PR covered it on 2026-11-12. The closest ones are about
other things: https://github.com/tursodatabase/turso/issues/4597 (an unfinished `Rows`) and
https://github.com/tursodatabase/turso/issues/8477 (a failed `COMMIT` in core).

Checked with turso 0.8.3, the latest release, and 0.9.0-pre.5, the latest pre-release. main at
`4b3befbd` has the same bindings code.

## What happens

This is the crate's own `test_drop_rollback_on_query`, with the read done through `prepare`:

```rust
let mut conn = db.connect()?;
{
    let tx = conn.transaction().await?;
    tx.execute("INSERT INTO t VALUES ('dropped')", ()).await?;
}
let mut stmt = conn.prepare("SELECT count(*) FROM t").await?;
let mut rows = stmt.query(()).await?;
```

The count is 1. With `conn.query` it's 0.

A transaction gets dropped like this when `?` returns early, or when a timeout cancels the future
that holds it.

`src/main.rs` checks three things, each on a new database file:

```
dirty read       a write transaction adds a row and is dropped, the next count should be 0
lost write       after the same drop, add a row, the count after the next query should be 1
stale snapshot   a read transaction is dropped, another connection adds a row, the count should be 1
```

It runs them through these paths:

```
prepare          Connection::prepare, then Statement::query or execute
prepare_cached   the same with Connection::prepare_cached
old-statement    a Statement prepared before the transaction
old-clone        Connection::query on a clone made before the transaction
workaround       Connection::execute_batch("") first, then prepare
connection       Connection::query or execute
```

Output, the same on 0.8.3 and 0.9.0-pre.5:

```
dirty read: rows seen after a dropped write transaction, expected 0
  prepare         1
  prepare_cached  1
  old-statement   1
  old-clone       1
  workaround      0
  connection      0
lost write: insert reported / rows kept after the next Connection::query, expected 1 / 1
  prepare         1 / 0
  prepare_cached  1 / 0
  connection      1 / 1
stale snapshot: rows seen after another connection committed one, expected 1
  prepare         0
  prepare_cached  0
  connection      1

VERDICT dirty-read prepare: BROKEN
VERDICT dirty-read prepare_cached: BROKEN
VERDICT dirty-read old-statement: BROKEN
VERDICT dirty-read old-clone: BROKEN
VERDICT dirty-read workaround: WORKS
VERDICT dirty-read connection: WORKS
VERDICT lost-write prepare: BROKEN
VERDICT lost-write prepare_cached: BROKEN
VERDICT lost-write connection: WORKS
VERDICT stale-snapshot prepare: BROKEN
VERDICT stale-snapshot prepare_cached: BROKEN
VERDICT stale-snapshot connection: WORKS
```

The lost write is the worst one. `Statement::execute` returns 1, but the row went into the dropped
transaction, and the next `conn.query` rolls it back.

## Run it

```
cargo run
```

For the pre-release:

```
sed -i 's/"=0.8.3"/"=0.9.0-pre.5"/' Cargo.toml
cargo run
```

## Why it's a bug

The docs say the rollback happens on the next access to the connection:

```
bindings/rust/src/transaction.rs:246
    /// The transaction defaults to rolling back on the next access to the connection
    /// if it is not finished when the transaction is dropped.
```

`Drop` can't run SQL, so it stores the `DropBehavior` on the connection and the next call runs it.
Only some calls do. Lines are from main, under `bindings/rust/src/`:

```
connection.rs:125, 132    Connection::query, execute run it
connection.rs:148, 266    execute_batch, batch run it
connection.rs:354, 367    prepare, prepare_cached don't
lib.rs:444, 485           Statement::query, execute don't
connection.rs:78          Clone copies the flag instead of sharing it
```

The step came in with https://github.com/tursodatabase/turso/pull/3750, which added it to `query`,
`execute` and `execute_batch` but not to `prepare`.

## Suggested fix

Two commits with tests, on top of main:
https://github.com/tursodatabase/turso/compare/main...tehfreak:turso:dropped-tx-prepared-statements

1. Share `dangling_tx` between clones with an `Arc`.
2. Run the stored step in `Statement::query` and `Statement::execute`, after they take the shared
   operation guard.
3. Make `maybe_handle_dangling_tx` take the guard and call the unguarded version. It used to run
   its ROLLBACK through `Statement::execute`, which would now fail with
   `cannot rollback - no transaction is active`.

With the fix every line here prints WORKS.

Until then, `conn.execute_batch("")` before a prepared statement works around it.

## CI

`.github/workflows/reproduce.yml` runs the repro on 0.8.3 and on 0.9.0-pre.5 and compares the
VERDICT lines with the list above. The job stays green while the bug is there and goes red when
any line changes, a fix included.
