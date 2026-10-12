//! The storage of a Durable Object: one table of keys and serialized values
//! in the database file of the object, and one row for its alarm.

use openworkers_core::ObjectStorageOp;
use openworkers_core::ObjectStorageResult;
use rusqlite::OptionalExtension;
use rusqlite::types::Value;

use super::Database;

const TABLE: &str =
    "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID";

/// Milliseconds since the Unix epoch
const ALARM_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS alarm (id INTEGER PRIMARY KEY CHECK (id = 0), at INTEGER NOT NULL)";

/// Bytes of a key, of a value, and keys per call, as on Cloudflare
const MAX_KEY_BYTES: usize = 2048;
const MAX_VALUE_BYTES: usize = 128 * 1024;
const MAX_KEYS: usize = 128;

impl Database {
    /// Runs an operation of the object storage. The statements are the
    /// runner's, so the guest restrictions are off while they run.
    pub(super) fn object(&mut self, op: ObjectStorageOp) -> ObjectStorageResult {
        self.guest
            .store(false, std::sync::atomic::Ordering::Relaxed);

        let result = self.object_op(op);

        self.guest.store(true, std::sync::atomic::Ordering::Relaxed);

        result.unwrap_or_else(ObjectStorageResult::Error)
    }

    /// Deletes the alarm if it is still `at`, after its handler ran, and
    /// answers the alarm after that: a handler can set the next one.
    pub(super) fn clear_alarm(&mut self, at: i64) -> ObjectStorageResult {
        self.guest
            .store(false, std::sync::atomic::Ordering::Relaxed);

        let result = self.check_writes().map_err(|e| e.message).and_then(|()| {
            self.conn
                .execute(ALARM_TABLE, [])
                .and_then(|_| self.conn.execute("DELETE FROM alarm WHERE at = ?1", [at]))
                .map_err(|e| e.to_string())?;

            self.alarm().map(ObjectStorageResult::Alarm)
        });

        self.guest.store(true, std::sync::atomic::Ordering::Relaxed);

        result.unwrap_or_else(ObjectStorageResult::Error)
    }

    fn alarm(&self) -> Result<Option<i64>, String> {
        self.conn
            .query_row("SELECT at FROM alarm", [], |row| row.get(0))
            .optional()
            .map_err(|e| e.to_string())
    }

    fn object_op(&mut self, op: ObjectStorageOp) -> Result<ObjectStorageResult, String> {
        self.conn.execute(TABLE, []).map_err(|e| e.to_string())?;
        self.conn
            .execute(ALARM_TABLE, [])
            .map_err(|e| e.to_string())?;

        match op {
            ObjectStorageOp::GetAlarm => Ok(ObjectStorageResult::Alarm(self.alarm()?)),
            ObjectStorageOp::SetAlarm(at) => {
                self.check_writes().map_err(|e| e.message)?;

                match at {
                    Some(at) => self.conn.execute(
                        "INSERT INTO alarm (id, at) VALUES (0, ?1) \
                         ON CONFLICT (id) DO UPDATE SET at = excluded.at",
                        [at],
                    ),
                    None => self.conn.execute("DELETE FROM alarm", []),
                }
                .map_err(|e| e.to_string())?;

                Ok(ObjectStorageResult::Alarm(at))
            }
            ObjectStorageOp::Get { keys } => {
                check_keys(&keys)?;

                let mut stmt = self
                    .conn
                    .prepare_cached("SELECT value FROM kv WHERE key = ?1")
                    .map_err(|e| e.to_string())?;

                let values = keys
                    .iter()
                    .map(|key| stmt.query_row([key], |row| row.get(0)).optional())
                    .collect::<rusqlite::Result<Vec<Option<Vec<u8>>>>>()
                    .map_err(|e| e.to_string())?;

                Ok(ObjectStorageResult::Values(values))
            }
            ObjectStorageOp::List {
                start,
                start_after,
                end,
                prefix,
                reverse,
                limit,
            } => {
                let mut sql = "SELECT key, value FROM kv WHERE 1".to_string();
                let mut params: Vec<Value> = Vec::new();

                let mut bound = |sql: &mut String, clause: &str, value: Value| {
                    params.push(value);
                    sql.push_str(&clause.replace('?', &format!("?{}", params.len())));
                };

                if let Some(start) = start {
                    bound(&mut sql, " AND key >= ?", Value::Text(start));
                }

                if let Some(after) = start_after {
                    bound(&mut sql, " AND key > ?", Value::Text(after));
                }

                if let Some(end) = end {
                    bound(&mut sql, " AND key < ?", Value::Text(end));
                }

                if let Some(prefix) = prefix {
                    bound(
                        &mut sql,
                        " AND substr(key, 1, length(?)) = ?",
                        Value::Text(prefix),
                    );
                }

                sql.push_str(if reverse {
                    " ORDER BY key DESC"
                } else {
                    " ORDER BY key ASC"
                });

                if let Some(limit) = limit {
                    bound(
                        &mut sql,
                        " LIMIT ?",
                        Value::Integer(limit.min(i64::MAX as u64) as i64),
                    );
                }

                let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
                let mut rows = stmt
                    .query(rusqlite::params_from_iter(params))
                    .map_err(|e| e.to_string())?;
                let mut entries = Vec::new();
                let mut bytes = 0;

                while let Some(row) = rows.next().map_err(|e| e.to_string())? {
                    let key: String = row.get(0).map_err(|e| e.to_string())?;
                    let value: Vec<u8> = row.get(1).map_err(|e| e.to_string())?;

                    bytes += key.len() + value.len();

                    if bytes > self.result_bytes {
                        return Err("the list is too large; list with a limit".to_string());
                    }

                    entries.push((key, value));
                }

                Ok(ObjectStorageResult::Entries(entries))
            }
            ObjectStorageOp::Write {
                puts,
                deletes,
                delete_all,
            } => {
                check_keys(&deletes)?;

                if puts.len() > MAX_KEYS {
                    return Err(format!("a put takes at most {MAX_KEYS} keys"));
                }

                for (key, value) in &puts {
                    check_key(key)?;

                    if value.len() > MAX_VALUE_BYTES {
                        return Err(format!(
                            "the value of '{key}' is over {MAX_VALUE_BYTES} bytes"
                        ));
                    }
                }

                self.check_writes().map_err(|e| e.message)?;

                let txn = self
                    .conn
                    .unchecked_transaction()
                    .map_err(|e| e.to_string())?;

                if delete_all {
                    txn.execute("DELETE FROM kv", [])
                        .map_err(|e| e.to_string())?;
                }

                let mut deleted = 0;

                for key in &deletes {
                    deleted += txn
                        .execute("DELETE FROM kv WHERE key = ?1", [key])
                        .map_err(|e| e.to_string())? as u64;
                }

                for (key, value) in &puts {
                    txn.execute(
                        "INSERT INTO kv (key, value) VALUES (?1, ?2) \
                         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                        rusqlite::params![key, value],
                    )
                    .map_err(|e| e.to_string())?;
                }

                txn.commit().map_err(|e| e.to_string())?;

                Ok(ObjectStorageResult::Written { deleted })
            }
        }
    }
}

fn check_key(key: &str) -> Result<(), String> {
    if key.len() > MAX_KEY_BYTES {
        return Err(format!("a key is at most {MAX_KEY_BYTES} bytes"));
    }

    Ok(())
}

fn check_keys(keys: &[String]) -> Result<(), String> {
    if keys.len() > MAX_KEYS {
        return Err(format!("a call takes at most {MAX_KEYS} keys"));
    }

    keys.iter().try_for_each(|key| check_key(key))
}
