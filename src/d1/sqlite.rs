//! One D1 database file: connection settings, guest restrictions and the
//! execution of a `D1Op`. Everything here blocks; callers run it off the
//! async executor.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Instant;

use openworkers_core::D1Error;
use openworkers_core::D1ErrorKind;
use openworkers_core::D1Meta;
use openworkers_core::D1Op;
use openworkers_core::D1Result;
use openworkers_core::D1Rows;
use openworkers_core::D1Statement;
use openworkers_core::D1StatementResult;
use openworkers_core::ObjectStorageOp;
use openworkers_core::ObjectStorageResult;
use openworkers_core::SqlPrimitive;
use rusqlite::Connection;
use rusqlite::InterruptHandle;
use rusqlite::Statement;
use rusqlite::config::DbConfig;
use rusqlite::fallible_iterator::FallibleIterator;
use rusqlite::hooks::AuthAction;
use rusqlite::hooks::AuthContext;
use rusqlite::hooks::Authorization;
use rusqlite::limits::Limit;
use rusqlite::types::Value;
use rusqlite::types::ValueRef;

mod object;

/// Pragmas a guest can run: schema reads, and the deferral migration tools use
const GUEST_PRAGMAS: &[&str] = &[
    "defer_foreign_keys",
    "foreign_key_check",
    "foreign_key_list",
    "index_info",
    "index_list",
    "index_xinfo",
    "quick_check",
    "table_info",
    "table_list",
    "table_xinfo",
];

/// The ledger of `ow d1 migrations`, inside the database it describes
const MIGRATIONS_TABLE: &str = "CREATE TABLE IF NOT EXISTS d1_migrations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    checksum TEXT NOT NULL,
    applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
)";

/// Largest integer a JavaScript number holds exactly (2^53 - 1)
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[derive(Debug, Clone)]
pub struct Limits {
    pub sql_length: i32,
    pub parameters: i32,
    pub columns: i32,
    pub value_length: i32,
    pub database_bytes: u64,
    pub result_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            sql_length: 100_000,
            parameters: 100,
            columns: 100,
            value_length: 2_000_000,
            database_bytes: 500 * 1024 * 1024,
            result_bytes: 16 * 1024 * 1024,
        }
    }
}

/// One migration file: its SQL runs in one transaction with its ledger row
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Migration {
    pub name: String,
    pub sql: String,
    pub checksum: String,
}

/// What the executor of a database runs
pub enum Job {
    /// A guest operation
    Op(D1Op),
    /// A management migration
    Migrate(Migration),
    /// An operation on the storage of a Durable Object
    Object(ObjectStorageOp),
    /// Deletes the alarm of a Durable Object if it is still this time
    ClearAlarm(i64),
}

/// What a job gives back
pub enum Done {
    D1(D1Result),
    Object(ObjectStorageResult),
}

pub struct Database {
    conn: Connection,
    /// Off while the runner runs its own statements (BEGIN, page_count), which
    /// the guest restrictions would deny.
    guest: Arc<AtomicBool>,
    page_size: u64,
    result_bytes: usize,
    /// Why writes are refused during the current job, if they are
    paused: Option<String>,
    /// Why writes that add data are refused during the current job, if they
    /// are; deletes still run
    full: Option<String>,
}

impl Database {
    /// Opens or creates the database. `commits` counts the transactions that
    /// commit, so the archive knows what a snapshot still lacks.
    pub fn open(path: &Path, limits: &Limits, commits: Arc<AtomicU64>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;

        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;

        let page_size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
        let page_size = page_size as u64;

        conn.pragma_update(
            None,
            "max_page_count",
            (limits.database_bytes / page_size) as i64,
        )?;

        conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, limits.sql_length)?;
        conn.set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, limits.parameters)?;
        conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, limits.columns)?;
        conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, limits.value_length)?;
        conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;

        let guest = Arc::new(AtomicBool::new(true));
        let hook_guest = guest.clone();

        conn.authorizer(Some(move |context: AuthContext<'_>| {
            if hook_guest.load(Ordering::Relaxed) {
                authorize_guest(&context)
            } else {
                Authorization::Allow
            }
        }))?;

        conn.commit_hook(Some(move || {
            commits.fetch_add(1, Ordering::SeqCst);
            false
        }))?;

        Ok(Self {
            conn,
            guest,
            page_size,
            result_bytes: limits.result_bytes,
            paused: None,
            full: None,
        })
    }

    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.conn.get_interrupt_handle()
    }

    /// Runs a job. With `paused`, a statement that writes fails with that
    /// reason and reads still run. With `full`, a put of an object fails.
    pub fn run(&mut self, job: Job, paused: Option<String>, full: Option<String>) -> Done {
        self.paused = paused;
        self.full = full;

        let op = match job {
            Job::Op(op) => op,
            Job::Migrate(migration) => return Done::D1(self.migrate(&migration)),
            Job::Object(op) => return Done::Object(self.object(op)),
            Job::ClearAlarm(at) => return Done::Object(self.clear_alarm(at)),
        };

        Done::D1(match op {
            D1Op::Query { statement, rows } => match self.query(&statement, rows) {
                Ok(result) => D1Result::Query(result),
                Err(err) => D1Result::Error(err),
            },
            D1Op::Batch { statements } => self.batch(&statements),
            D1Op::Exec { sql } => self.exec(&sql),
        })
    }

    fn query(&self, statement: &D1Statement, rows: D1Rows) -> Result<D1StatementResult, D1Error> {
        let started = Instant::now();
        let changes_before = self.conn.total_changes();

        let mut stmt = self.conn.prepare(&statement.sql).map_err(sql_error)?;

        bind(&mut stmt, &statement.params)?;

        let readonly = stmt.readonly();

        if !readonly {
            self.check_writes()?;
        }
        let columns: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
        let mut values = Vec::new();
        let mut bytes = 0;
        let mut cursor = stmt.raw_query();

        while let Some(row) = cursor.next().map_err(sql_error)? {
            if rows == D1Rows::First && !values.is_empty() {
                if readonly {
                    break;
                }

                continue;
            }

            let mut row_values = Vec::with_capacity(columns.len());

            for index in 0..columns.len() {
                let value = row.get_ref(index).map_err(sql_error)?;

                bytes += value_size(&value);
                row_values.push(to_primitive(value));
            }

            if bytes > self.result_bytes {
                return Err(error(
                    D1ErrorKind::Overloaded,
                    format!("result larger than {} bytes", self.result_bytes),
                ));
            }

            values.push(row_values);
        }

        drop(cursor);

        let changed = self.conn.total_changes() != changes_before;

        Ok(D1StatementResult {
            columns,
            rows: values,
            meta: D1Meta {
                duration: started.elapsed().as_secs_f64() * 1000.0,
                changes: if changed { self.conn.changes() } else { 0 },
                last_row_id: self.conn.last_insert_rowid(),
                changed_db: !readonly,
                size_after: self.size(),
            },
        })
    }

    fn batch(&self, statements: &[D1Statement]) -> D1Result {
        if let Err(err) = self.trusted("BEGIN IMMEDIATE") {
            return D1Result::Error(err);
        }

        let mut results = Vec::with_capacity(statements.len());

        for (index, statement) in statements.iter().enumerate() {
            match self.query(statement, D1Rows::All) {
                Ok(result) => results.push(result),
                Err(err) => {
                    self.rollback();

                    return D1Result::Error(D1Error {
                        statement: Some(index),
                        ..err
                    });
                }
            }
        }

        if let Err(err) = self.trusted("COMMIT") {
            self.rollback();

            return D1Result::Error(err);
        }

        D1Result::Batch { results }
    }

    fn exec(&self, sql: &str) -> D1Result {
        let started = Instant::now();

        match self.script(sql) {
            Ok(count) => D1Result::Exec {
                count,
                duration: started.elapsed().as_secs_f64() * 1000.0,
            },
            Err(err) => D1Result::Error(err),
        }
    }

    /// Runs the statements of `sql` in order and counts them. The error names
    /// the failing statement; the ones before it are not undone here.
    fn script(&self, sql: &str) -> Result<u32, D1Error> {
        let mut batch = rusqlite::Batch::new(&self.conn, sql);
        let mut count = 0;

        loop {
            let at = |err: D1Error| D1Error {
                statement: Some(count as usize),
                ..err
            };

            let mut stmt = match batch.next() {
                Ok(Some(stmt)) => stmt,
                Ok(None) => return Ok(count),
                Err(err) => return Err(at(sql_error(err))),
            };

            if stmt.parameter_count() > 0 {
                return Err(at(error(
                    D1ErrorKind::Sql,
                    "exec() does not take parameters".to_string(),
                )));
            }

            if !stmt.readonly() {
                self.check_writes().map_err(at)?;
            }

            let mut cursor = stmt.raw_query();

            loop {
                match cursor.next() {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(err) => return Err(at(sql_error(err))),
                }
            }

            count += 1;
        }
    }

    /// Applies a migration and records it, in one transaction. A migration
    /// already recorded with the same checksum is a no-op that counts 0.
    fn migrate(&self, migration: &Migration) -> D1Result {
        let started = Instant::now();

        if let Err(err) = self.check_writes() {
            return D1Result::Error(err);
        }

        if let Err(err) = self.trusted("BEGIN IMMEDIATE") {
            return D1Result::Error(err);
        }

        let count = match self.migrate_in_transaction(migration) {
            Ok(count) => count,
            Err(err) => {
                self.rollback();
                return D1Result::Error(err);
            }
        };

        if let Err(err) = self.trusted("COMMIT") {
            self.rollback();
            return D1Result::Error(err);
        }

        D1Result::Exec {
            count,
            duration: started.elapsed().as_secs_f64() * 1000.0,
        }
    }

    fn migrate_in_transaction(&self, migration: &Migration) -> Result<u32, D1Error> {
        self.trusted(MIGRATIONS_TABLE)?;

        let applied: Option<String> = self
            .conn
            .query_row(
                "SELECT checksum FROM d1_migrations WHERE name = ?1",
                [&migration.name],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                err => Err(sql_error(err)),
            })?;

        match applied {
            Some(checksum) if checksum == migration.checksum => return Ok(0),
            Some(_) => {
                return Err(error(
                    D1ErrorKind::Sql,
                    format!(
                        "migration {} was applied with another checksum",
                        migration.name
                    ),
                ));
            }
            None => {}
        }

        let count = self.script(&migration.sql)?;

        self.conn
            .execute(
                "INSERT INTO d1_migrations (name, checksum) VALUES (?1, ?2)",
                [&migration.name, &migration.checksum],
            )
            .map_err(sql_error)?;

        Ok(count)
    }

    fn check_writes(&self) -> Result<(), D1Error> {
        match &self.paused {
            Some(reason) => Err(error(
                D1ErrorKind::Overloaded,
                format!("writes are paused: {reason}"),
            )),
            None => Ok(()),
        }
    }

    /// Runs a runner statement that the guest restrictions would deny
    fn trusted(&self, sql: &str) -> Result<(), D1Error> {
        self.guest.store(false, Ordering::Relaxed);

        let result = self.conn.execute_batch(sql).map_err(sql_error);

        self.guest.store(true, Ordering::Relaxed);

        result
    }

    /// SQLite already rolls back after some errors (interrupt, full disk), so
    /// a transaction can be gone by now.
    fn rollback(&self) {
        if !self.conn.is_autocommit() {
            let _ = self.trusted("ROLLBACK");
        }
    }

    /// The bytes of the pages in use, which a delete frees at once
    pub fn used_bytes(&self) -> u64 {
        self.guest.store(false, Ordering::Relaxed);

        let pages = |pragma: &str| -> u64 {
            self.conn
                .pragma_query_value(None, pragma, |row| row.get::<_, i64>(0))
                .unwrap_or(0) as u64
        };
        let used = pages("page_count").saturating_sub(pages("freelist_count"));

        self.guest.store(true, Ordering::Relaxed);

        used * self.page_size
    }

    fn size(&self) -> u64 {
        self.guest.store(false, Ordering::Relaxed);

        let pages: i64 = self
            .conn
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .unwrap_or(0);

        self.guest.store(true, Ordering::Relaxed);

        pages as u64 * self.page_size
    }
}

fn authorize_guest(context: &AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Attach { .. }
        | AuthAction::Detach { .. }
        | AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. } => Authorization::Deny,
        AuthAction::Pragma { pragma_name, .. } => {
            if GUEST_PRAGMAS.contains(&pragma_name.to_ascii_lowercase().as_str()) {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Function { function_name }
            if function_name.eq_ignore_ascii_case("load_extension") =>
        {
            Authorization::Deny
        }
        _ => Authorization::Allow,
    }
}

/// Binds `params[i]` to `?{i + 1}`, after the checks D1 makes
fn bind(stmt: &mut Statement<'_>, params: &[SqlPrimitive]) -> Result<(), D1Error> {
    let count = stmt.parameter_count();

    for index in 1..=count {
        if let Some(name) = stmt.parameter_name(index)
            && name.starts_with([':', '@', '$'])
        {
            return Err(error(
                D1ErrorKind::Sql,
                format!("named parameter {name} is not supported, use ?NNN"),
            ));
        }
    }

    if params.len() != count {
        return Err(error(
            D1ErrorKind::Range,
            format!(
                "Wrong number of parameter bindings for SQL query: {} expected, {} given",
                count,
                params.len()
            ),
        ));
    }

    for (index, param) in params.iter().enumerate() {
        stmt.raw_bind_parameter(index + 1, to_value(param))
            .map_err(sql_error)?;
    }

    Ok(())
}

/// A JavaScript number arrives as a float when it is not a 32-bit integer;
/// D1 stores a whole number in the safe range as INTEGER.
fn to_value(param: &SqlPrimitive) -> Value {
    match param {
        SqlPrimitive::Null => Value::Null,
        SqlPrimitive::Bool(value) => Value::Integer(i64::from(*value)),
        SqlPrimitive::Int(value) => Value::Integer(*value),
        SqlPrimitive::Float(value) => {
            if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER {
                Value::Integer(*value as i64)
            } else {
                Value::Real(*value)
            }
        }
        SqlPrimitive::String(value) => Value::Text(value.clone()),
        SqlPrimitive::Bytes(value) => Value::Blob(value.clone()),
    }
}

fn to_primitive(value: ValueRef<'_>) -> SqlPrimitive {
    match value {
        ValueRef::Null => SqlPrimitive::Null,
        ValueRef::Integer(value) => SqlPrimitive::Int(value),
        ValueRef::Real(value) => SqlPrimitive::Float(value),
        ValueRef::Text(value) => SqlPrimitive::String(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(value) => SqlPrimitive::Bytes(value.to_vec()),
    }
}

fn value_size(value: &ValueRef<'_>) -> usize {
    match value {
        ValueRef::Null => 1,
        ValueRef::Integer(_) | ValueRef::Real(_) => 8,
        ValueRef::Text(value) | ValueRef::Blob(value) => value.len(),
    }
}

fn error(kind: D1ErrorKind, message: String) -> D1Error {
    D1Error {
        kind,
        message,
        statement: None,
    }
}

/// The D1 form of a SQLite error: `<message>: SQLITE_<CODE>`
fn sql_error(err: rusqlite::Error) -> D1Error {
    let (failure, message) = match &err {
        rusqlite::Error::SqliteFailure(failure, message) => (failure, message.clone()),
        // Errors at prepare time, such as a syntax error or a denied function
        rusqlite::Error::SqlInputError { error, msg, .. } => (error, Some(msg.clone())),
        _ => return error(D1ErrorKind::Sql, err.to_string()),
    };

    let code = failure.extended_code & 0xff;

    let kind = match code {
        rusqlite::ffi::SQLITE_INTERRUPT => D1ErrorKind::Interrupted,
        rusqlite::ffi::SQLITE_FULL => D1ErrorKind::Overloaded,
        _ => D1ErrorKind::Sql,
    };

    let text = message.unwrap_or_else(|| failure.to_string());

    error(kind, format!("{}: {}", text, code_name(code)))
}

fn code_name(code: i32) -> &'static str {
    match code {
        1 => "SQLITE_ERROR",
        2 => "SQLITE_INTERNAL",
        3 => "SQLITE_PERM",
        4 => "SQLITE_ABORT",
        5 => "SQLITE_BUSY",
        6 => "SQLITE_LOCKED",
        7 => "SQLITE_NOMEM",
        8 => "SQLITE_READONLY",
        9 => "SQLITE_INTERRUPT",
        10 => "SQLITE_IOERR",
        11 => "SQLITE_CORRUPT",
        13 => "SQLITE_FULL",
        14 => "SQLITE_CANTOPEN",
        17 => "SQLITE_SCHEMA",
        18 => "SQLITE_TOOBIG",
        19 => "SQLITE_CONSTRAINT",
        20 => "SQLITE_MISMATCH",
        21 => "SQLITE_MISUSE",
        23 => "SQLITE_AUTH",
        25 => "SQLITE_RANGE",
        26 => "SQLITE_NOTADB",
        _ => "SQLITE_ERROR",
    }
}
