use std::error::Error;
use std::path::Path;

use turso::{Builder, Connection, Database, Rows};

const COUNT: &str = "SELECT count(*) FROM t";
const INSERT: &str = "INSERT INTO t VALUES ('kept')";

#[derive(Clone, Copy)]
enum Access {
    Prepare,
    PrepareCached,
    OldStatement,
    OldClone,
    Workaround,
    Connection,
}

impl Access {
    fn label(self) -> &'static str {
        match self {
            Access::Prepare => "prepare",
            Access::PrepareCached => "prepare_cached",
            Access::OldStatement => "old-statement",
            Access::OldClone => "old-clone",
            Access::Workaround => "workaround",
            Access::Connection => "connection",
        }
    }
}

async fn open(dir: &Path, scenario: &str, access: Access) -> turso::Result<Database> {
    let path = dir.join(format!("{scenario}-{}.db", access.label()));
    let db = Builder::new_local(path.to_str().unwrap()).build().await?;
    db.connect()?.execute("CREATE TABLE t (v TEXT)", ()).await?;
    Ok(db)
}

async fn first(mut rows: Rows) -> turso::Result<i64> {
    let n = rows.next().await?.unwrap().get::<i64>(0)?;
    while rows.next().await?.is_some() {}
    Ok(n)
}

async fn count(conn: &Connection, access: Access) -> turso::Result<i64> {
    let rows = match access {
        Access::Prepare => conn.prepare(COUNT).await?.query(()).await?,
        Access::PrepareCached => conn.prepare_cached(COUNT).await?.query(()).await?,
        _ => conn.query(COUNT, ()).await?,
    };
    first(rows).await
}

async fn insert(conn: &Connection, access: Access) -> turso::Result<u64> {
    match access {
        Access::Prepare => conn.prepare(INSERT).await?.execute(()).await,
        Access::PrepareCached => conn.prepare_cached(INSERT).await?.execute(()).await,
        _ => conn.execute(INSERT, ()).await,
    }
}

async fn dirty_read(dir: &Path, access: Access) -> turso::Result<i64> {
    let db = open(dir, "dirty-read", access).await?;
    let mut conn = db.connect()?;
    let old_clone = conn.clone();
    let mut old_statement = conn.prepare(COUNT).await?;
    {
        let tx = conn.transaction().await?;
        tx.execute("INSERT INTO t VALUES ('dropped')", ()).await?;
    }
    match access {
        Access::OldStatement => first(old_statement.query(()).await?).await,
        Access::OldClone => count(&old_clone, Access::Connection).await,
        Access::Workaround => {
            conn.execute_batch("").await?;
            count(&conn, Access::Prepare).await
        }
        _ => count(&conn, access).await,
    }
}

async fn lost_write(dir: &Path, access: Access) -> turso::Result<(u64, i64)> {
    let db = open(dir, "lost-write", access).await?;
    let mut conn = db.connect()?;
    {
        let tx = conn.transaction().await?;
        tx.execute("INSERT INTO t VALUES ('dropped')", ()).await?;
    }
    let reported = insert(&conn, access).await?;
    let kept = count(&conn, Access::Connection).await?;
    Ok((reported, kept))
}

async fn stale_snapshot(dir: &Path, access: Access) -> turso::Result<i64> {
    let db = open(dir, "stale-snapshot", access).await?;
    let mut reader = db.connect()?;
    let writer = db.connect()?;
    {
        let tx = reader.transaction().await?;
        count(&tx, access).await?;
    }
    writer
        .execute("INSERT INTO t VALUES ('committed')", ())
        .await?;
    count(&reader, access).await
}

fn verdict(works: bool) -> &'static str {
    if works { "WORKS" } else { "BROKEN" }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("repro-turso-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let mut verdicts = Vec::new();

    println!("dirty read: rows seen after a dropped write transaction, expected 0");
    for access in [
        Access::Prepare,
        Access::PrepareCached,
        Access::OldStatement,
        Access::OldClone,
        Access::Workaround,
        Access::Connection,
    ] {
        let seen = dirty_read(&dir, access).await?;
        println!("  {:<16}{seen}", access.label());
        verdicts.push(("dirty-read", access, seen == 0));
    }

    println!(
        "lost write: insert reported / rows kept after the next Connection::query, expected 1 / 1"
    );
    for access in [Access::Prepare, Access::PrepareCached, Access::Connection] {
        let (reported, kept) = lost_write(&dir, access).await?;
        println!("  {:<16}{reported} / {kept}", access.label());
        verdicts.push(("lost-write", access, reported == 1 && kept == 1));
    }

    println!("stale snapshot: rows seen after another connection committed one, expected 1");
    for access in [Access::Prepare, Access::PrepareCached, Access::Connection] {
        let seen = stale_snapshot(&dir, access).await?;
        println!("  {:<16}{seen}", access.label());
        verdicts.push(("stale-snapshot", access, seen == 1));
    }

    println!();
    for (scenario, access, works) in verdicts {
        println!("VERDICT {scenario} {}: {}", access.label(), verdict(works));
    }

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
