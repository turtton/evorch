//! Test owners of blobs: two tables holding JSON with tokens, with the reference tables
//! the registry prescribes. Production owners arrive with A2b/B1; these stand in so the
//! whole write path, collector and checker are exercised by A2a.

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde_json::Value;

use super::{KeyColumn, OwnerSpec, put_owner};
use crate::{Database, StorageError};

const KEY_A: &[KeyColumn] = &[KeyColumn {
    ref_column: "owner_id",
    owner_column: "id",
    sql_type: "TEXT",
}];

const KEY_B: &[KeyColumn] = &[KeyColumn {
    ref_column: "owner_n",
    owner_column: "n",
    sql_type: "INTEGER",
}];

/// Text key, one container (`body`), attributed to a run.
pub(crate) const OWNER: OwnerSpec = OwnerSpec {
    name: "test_owner",
    ref_table: "content_blob_refs_test_owner",
    owner_table: "test_owner",
    keys: KEY_A,
    container_columns: &["body"],
    codec_column: None,
    filter: None,
    run_column: Some("run_id"),
};

/// Integer key; only rows with `tokenized = 1` hold tokens.
pub(crate) const OWNER_B: OwnerSpec = OwnerSpec {
    name: "test_owner_b",
    ref_table: "content_blob_refs_test_owner_b",
    owner_table: "test_owner_b",
    keys: KEY_B,
    container_columns: &["body"],
    codec_column: None,
    filter: Some("tokenized = 1"),
    run_column: None,
};

/// The test owners and the production ones, whose reference tables the latest schema
/// creates: the integrity check reports a reference table that no owner registers.
pub(crate) const OWNERS: &[OwnerSpec] = &[
    OWNER,
    OWNER_B,
    crate::body_store::MESSAGE_OWNER,
    crate::body_store::HEADER_OWNER,
];

/// Creates the owner tables and their reference tables on `conn`.
pub(crate) fn install(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE test_owner (id TEXT PRIMARY KEY, body TEXT NOT NULL, run_id TEXT);
         CREATE TABLE test_owner_b (n INTEGER PRIMARY KEY, body TEXT NOT NULL,
                                    tokenized INTEGER NOT NULL DEFAULT 1);",
    )
    .unwrap();
    conn.execute_batch(&OWNER.ref_table_ddl()).unwrap();
    conn.execute_batch(&OWNER_B.ref_table_ddl()).unwrap();
}

/// An in-memory database with the latest schema and the test owners.
pub(crate) fn database() -> Database {
    let database = Database::open_in_memory().unwrap();
    install(&database.conn);
    database
}

/// The IMMEDIATE transaction every write in these tests runs in.
pub(crate) fn immediate(conn: &Connection) -> Transaction<'_> {
    Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap()
}

/// Stores (inserts or updates) the row `id` of [`OWNER`] and commits.
pub(crate) fn store(conn: &Connection, id: &str, body: &Value) -> Result<(), StorageError> {
    store_in_run(conn, id, None, body)
}

pub(crate) fn store_in_run(
    conn: &Connection,
    id: &str,
    run: Option<&str>,
    body: &Value,
) -> Result<(), StorageError> {
    let tx = immediate(conn);
    put(&tx, id, run, body)?;
    tx.commit()?;
    Ok(())
}

/// [`store`] without committing: the caller owns the transaction.
pub(crate) fn put(
    conn: &Connection,
    id: &str,
    run: Option<&str>,
    body: &Value,
) -> Result<(), StorageError> {
    let mut containers = [body.clone()];
    put_owner(
        conn,
        &OWNER,
        &[SqlValue::Text(id.to_owned())],
        &mut containers,
        |conn, containers| {
            conn.execute(
                "INSERT INTO test_owner(id, body, run_id) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(id) DO UPDATE SET body = excluded.body, run_id = excluded.run_id",
                rusqlite::params![id, containers[0].to_string(), run],
            )?;
            Ok(())
        },
    )
}

/// Stores row `n` of [`OWNER_B`] and commits.
pub(crate) fn store_b(conn: &Connection, n: i64, body: &Value) -> Result<(), StorageError> {
    let tx = immediate(conn);
    let mut containers = [body.clone()];
    put_owner(
        &tx,
        &OWNER_B,
        &[SqlValue::Integer(n)],
        &mut containers,
        |conn, containers| {
            conn.execute(
                "INSERT INTO test_owner_b(n, body) VALUES (?1, ?2) \
                 ON CONFLICT(n) DO UPDATE SET body = excluded.body",
                rusqlite::params![n, containers[0].to_string()],
            )?;
            Ok(())
        },
    )?;
    tx.commit()?;
    Ok(())
}

/// A text of exactly `len` bytes that depends on `tag`, with no long repeats, so it
/// does not compress to much and two tags never collide.
pub(crate) fn text(tag: &str, len: usize) -> String {
    let mut state = tag.bytes().fold(0x2545_f491_4f6c_dd1d_u64, |acc, byte| {
        (acc ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    });
    state = (state ^ (state >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state = (state ^ (state >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    state = (state ^ (state >> 31)) | 1;
    let mut out = String::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(char::from(b'a' + (state % 26) as u8));
    }
    out
}

/// Resolves the stored body of row `id` of [`OWNER`] back to its original value.
pub(crate) fn load(conn: &Connection, id: &str) -> Result<Value, StorageError> {
    let stored: String =
        conn.query_row("SELECT body FROM test_owner WHERE id = ?1", [id], |row| {
            row.get(0)
        })?;
    let mut value: Value = serde_json::from_str(&stored)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    super::resolve_container(conn, &format!("test_owner id={id}"), &mut value)?;
    Ok(value)
}

pub(crate) fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}
