use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::protocol::TerminalInfo;

#[derive(Clone, Debug)]
pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let db = Self { path };
        db.with_connection(|connection| {
            connection.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS terminals (
                    id          TEXT PRIMARY KEY,
                    name        TEXT NOT NULL,
                    argv_json   TEXT NOT NULL,
                    cwd         TEXT NOT NULL,
                    status      TEXT NOT NULL,
                    exit_code   INTEGER,
                    rows        INTEGER NOT NULL,
                    cols        INTEGER NOT NULL,
                    created_at  INTEGER NOT NULL,
                    updated_at  INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS terminals_updated_at
                    ON terminals(updated_at DESC);",
            )?;
            Ok(())
        })?;
        secure_database_file(&db.path)?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn mark_interrupted(&self) -> Result<usize> {
        self.with_connection(|connection| {
            Ok(connection.execute(
                "UPDATE terminals
                 SET status = 'lost', updated_at = ?1
                 WHERE status IN ('creating', 'running')",
                params![now_seconds()],
            )?)
        })
    }

    pub fn insert_terminal(&self, terminal: &TerminalInfo) -> Result<()> {
        let argv = serde_json::to_string(&terminal.argv)?;
        let now = now_seconds();
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO terminals
                 (id, name, argv_json, cwd, status, exit_code, rows, cols, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                params![
                    terminal.id,
                    terminal.name,
                    argv,
                    terminal.cwd,
                    terminal.status,
                    terminal.exit_code,
                    terminal.rows,
                    terminal.cols,
                    now,
                ],
            )?;
            Ok(())
        })
    }

    pub fn update_status(&self, id: &str, status: &str, exit_code: Option<i32>) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE terminals
                 SET status = ?2, exit_code = ?3, updated_at = ?4
                 WHERE id = ?1",
                params![id, status, exit_code, now_seconds()],
            )?;
            Ok(())
        })
    }

    pub fn update_size(&self, id: &str, rows: u32, cols: u32) -> Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE terminals SET rows = ?2, cols = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, rows, cols, now_seconds()],
            )?;
            Ok(())
        })
    }

    pub fn list_terminals(&self) -> Result<Vec<TerminalInfo>> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, name, argv_json, cwd, status, exit_code, rows, cols
                 FROM terminals ORDER BY created_at, id",
            )?;
            let rows = statement.query_map([], |row| {
                let argv_json: String = row.get(2)?;
                let argv = serde_json::from_str(&argv_json).unwrap_or_default();
                Ok(TerminalInfo {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    argv,
                    cwd: row.get(3)?,
                    status: row.get(4)?,
                    exit_code: row.get(5)?,
                    rows: row.get(6)?,
                    cols: row.get(7)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn get_terminal(&self, id: &str) -> Result<Option<TerminalInfo>> {
        self.with_connection(|connection| {
            connection
                .query_row(
                    "SELECT id, name, argv_json, cwd, status, exit_code, rows, cols
                     FROM terminals WHERE id = ?1",
                    params![id],
                    |row| {
                        let argv_json: String = row.get(2)?;
                        Ok(TerminalInfo {
                            id: row.get(0)?,
                            name: row.get(1)?,
                            argv: serde_json::from_str(&argv_json).unwrap_or_default(),
                            cwd: row.get(3)?,
                            status: row.get(4)?,
                            exit_code: row.get(5)?,
                            rows: row.get(6)?,
                            cols: row.get(7)?,
                        })
                    },
                )
                .optional()
                .map_err(Into::into)
        })
    }

    fn with_connection<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open SQLite database {}", self.path.display()))?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        operation(&connection)
    }
}

#[cfg(unix)]
fn secure_database_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_database_file(_path: &Path) -> Result<()> {
    Ok(())
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_updates_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(dir.path().join("astra.db")).unwrap();
        let terminal = TerminalInfo {
            id: "one".into(),
            name: "shell".into(),
            argv: vec!["sh".into()],
            cwd: "/tmp".into(),
            status: "running".into(),
            exit_code: None,
            rows: 24,
            cols: 80,
        };
        database.insert_terminal(&terminal).unwrap();
        database.update_status("one", "exited", Some(0)).unwrap();
        let loaded = database.get_terminal("one").unwrap().unwrap();
        assert_eq!(loaded.status, "exited");
        assert_eq!(loaded.exit_code, Some(0));
    }
}
