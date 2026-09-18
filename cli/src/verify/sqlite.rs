//! Read-only, bounded SQL over an explicitly quiescent private-app snapshot.
use super::Status;
use anyhow::{Context, Result};
use rusqlite::{
    Connection, OpenFlags,
    fallible_iterator::FallibleIterator,
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    types::{Value as SqlValue, ValueRef},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub sql: String,
    #[serde(default)]
    pub parameters: Vec<Value>,
    pub expected_rows: Vec<Vec<Value>>,
    #[serde(default = "row_limit")]
    pub max_rows: usize,
    #[serde(default = "time_limit")]
    pub timeout_ms: u64,
}
fn row_limit() -> usize {
    100
}
fn time_limit() -> u64 {
    2000
}
impl Query {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.sql.trim().is_empty() && self.sql.len() <= 16_384,
            "SQL must contain 1..16384 bytes"
        );
        anyhow::ensure!(
            (1..=1000).contains(&self.max_rows) && (1..=10_000).contains(&self.timeout_ms),
            "SQL limits: rows 1..1000, time 1..10000 ms"
        );
        anyhow::ensure!(
            self.parameters.len() <= 100 && self.expected_rows.len() <= self.max_rows,
            "SQL parameter/expected row limit exceeded"
        );
        for value in &self.parameters {
            parameter(value)?;
        }
        Ok(())
    }
}
fn parameter(value: &Value) -> Result<SqlValue> {
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::String(s) => {
            anyhow::ensure!(s.len() <= 65_536, "SQL parameter too long");
            SqlValue::Text(s.clone())
        }
        Value::Bool(v) => SqlValue::Integer(i64::from(*v)),
        Value::Number(n) => {
            if let Some(v) = n.as_i64() {
                SqlValue::Integer(v)
            } else {
                SqlValue::Real(n.as_f64().context("invalid SQL number")?)
            }
        }
        _ => anyhow::bail!("SQL parameters must be scalar JSON values"),
    })
}

pub fn query(path: &Path, request: &Query) -> Result<Value> {
    request.validate()?;
    // No URI parameters, extension loading or custom VFS; open an existing copy only.
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_millis(100))?;
    connection.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")?;
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, 1_048_576)?;
    connection.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 16_384)?;
    connection.set_limit(Limit::SQLITE_LIMIT_COLUMN, 256)?;
    connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    connection.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, 100)?;
    let deadline = Instant::now() + Duration::from_millis(request.timeout_ms);
    connection.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
    // Validate the copied database before interpreting an empty query as evidence.
    let integrity: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    anyhow::ensure!(
        integrity == "ok",
        "snapshot integrity check failed: {integrity}"
    );
    connection.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Select | AuthAction::Read { .. } | AuthAction::Recursive => {
            Authorization::Allow
        }
        AuthAction::Function { function_name }
            if !matches!(
                function_name.to_ascii_lowercase().as_str(),
                "load_extension" | "writefile" | "readfile" | "fts3_tokenizer"
            ) =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }))?;
    let schema=connection.prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")?.query_map([],|row|Ok(json!({"type":row.get::<_,String>(0)?,"name":row.get::<_,String>(1)?,"table":row.get::<_,String>(2)?,"sql":row.get::<_,Option<String>>(3)?})))?.take(1001).collect::<rusqlite::Result<Vec<_>>>()?;
    anyhow::ensure!(schema.len() <= 1000, "schema exceeds 1000 objects");
    let mut batch = rusqlite::Batch::new(&connection, &request.sql);
    let mut statement = batch.next()?.context("SQL contains no statement")?;
    anyhow::ensure!(
        batch.next()?.is_none(),
        "exactly one SQL statement is allowed"
    );
    anyhow::ensure!(
        statement.readonly() && statement.column_count() > 0,
        "only read-only result queries are allowed"
    );
    let columns = statement
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let parameters = request
        .parameters
        .iter()
        .map(parameter)
        .collect::<Result<Vec<_>>>()?;
    let mut cursor = statement.query(rusqlite::params_from_iter(parameters))?;
    let mut rows = Vec::new();
    let mut bytes = 0;
    while let Some(row) = cursor.next()? {
        anyhow::ensure!(
            rows.len() < request.max_rows,
            "SQL row limit exceeded; result is incomplete"
        );
        let mut values = Vec::new();
        for index in 0..columns.len() {
            let value = match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(n) => {
                    anyhow::ensure!(n.is_finite(), "non-finite SQL result");
                    json!(n)
                }
                ValueRef::Text(v) => {
                    anyhow::ensure!(v.len() <= 65_536, "SQL value exceeds 64 KiB");
                    json!(std::str::from_utf8(v)?)
                }
                ValueRef::Blob(v) => {
                    json!({"blob_bytes":v.len(),"blake3":super::provenance::hash(v)})
                }
            };
            bytes += serde_json::to_vec(&value)?.len();
            anyhow::ensure!(bytes <= 1_048_576, "SQL result exceeds 1 MiB");
            values.push(value);
        }
        rows.push(values);
    }
    let matches = rows == request.expected_rows;
    Ok(
        json!({"rows":rows,"columns":columns,"schema":schema,"matches":matches,"sqlite_version":rusqlite::version(),"integrity":integrity,"query_only":true,"source_database_mutated":false,"scope":"quiescent_snapshot"}),
    )
}

pub async fn run(
    serial: &crate::ids::Serial,
    package: &str,
    database: &str,
    request: &Query,
    out: &Path,
) -> Result<(Status, Value, bool, bool)> {
    let snapshot = out.join("private-snapshot");
    // Snapshot failure is unavailable evidence. A transport error may leave the
    // stop/copy outcome uncertain and is propagated to the coordinator.
    crate::cmd::app_state::verification_snapshot(serial, package, &snapshot, database).await?;
    let path = snapshot.join("data").join(database);
    let request = request.clone();
    let result = tokio::task::spawn_blocking(move || query(&path, &request)).await?;
    let (status, evidence) = match result {
        Ok(evidence) => (
            if evidence["matches"] == true {
                Status::Passed
            } else {
                Status::Failed
            },
            evidence,
        ),
        Err(error) => (
            Status::Blocked,
            json!({"reason":"sqlite_evidence_unavailable","error":format!("{error:#}"),"empty_result_inferred":false}),
        ),
    };
    super::journey::save(&out.join("query.json"), &evidence)?;
    Ok((
        status,
        json!({"adapter":"sqlite","package":package,"database":database,"snapshot":snapshot,"app_state":"force_stopped","contains_private_data":true,"observation":evidence}),
        false,
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wal_rows_are_visible_and_sql_cannot_write_attach_or_hide_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE records(value TEXT); INSERT INTO records VALUES('random-729');").unwrap();
        let before = std::fs::read(&path).unwrap();
        let wal = std::fs::read(dir.path().join("data.db-wal")).unwrap();
        let request = Query {
            sql: "SELECT value FROM records WHERE value=?1".into(),
            parameters: vec![json!("random-729")],
            expected_rows: vec![vec![json!("random-729")]],
            max_rows: 10,
            timeout_ms: 1000,
        };
        assert_eq!(query(&path, &request).unwrap()["matches"], true);
        for sql in [
            "DELETE FROM records",
            "ATTACH ':memory:' AS other",
            "PRAGMA query_only=OFF",
            "SELECT load_extension('x')",
            "SELECT 1; SELECT 2",
            "SELECT 1 UNION ALL SELECT 2",
        ] {
            let mut request = request.clone();
            request.sql = sql.into();
            request.parameters.clear();
            request.max_rows = 1;
            assert!(query(&path, &request).is_err(), "must reject {sql}");
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read(dir.path().join("data.db-wal")).unwrap(), wal);
    }
    #[test]
    fn long_recursive_queries_and_invalid_databases_are_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t(x)")
            .unwrap();
        let request = Query {
            sql:
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT sum(x) FROM c"
                    .into(),
            parameters: vec![],
            expected_rows: vec![],
            max_rows: 1,
            timeout_ms: 1,
        };
        assert!(query(&path, &request).is_err());
        std::fs::write(&path, b"encrypted or corrupt").unwrap();
        assert!(query(&path, &request).is_err());
    }
}
