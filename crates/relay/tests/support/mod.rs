#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use conductor_remote::db::ConductorDb;
use rusqlite::Connection;
use tempfile::TempDir;

/// Conductor's table and index definitions, copied from its real database (schema only).
pub const SCHEMA: &str = include_str!("conductor_schema.sql");

/// A synthetic Conductor database in a temporary directory.
pub struct TestDb {
    dir: TempDir,
    path: PathBuf,
    root: PathBuf,
}

impl TestDb {
    /// A fresh database file in WAL mode, with the schema applied and no rows.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("conductor.db");
        let root = dir.path().join("workspaces");
        std::fs::create_dir(&root).expect("workspaces directory");
        {
            let conn = Connection::open(&path).expect("create database");
            conn.pragma_update(None, "journal_mode", "wal")
                .expect("enable WAL");
            conn.execute_batch(SCHEMA).expect("apply schema");
        }
        Self { dir, path, root }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A writable connection, for inserting rows.
    pub fn conn(&self) -> Connection {
        Connection::open(&self.path).expect("open database for writing")
    }

    /// The relay's read-only handle on the same file.
    pub fn db(&self) -> Arc<ConductorDb> {
        Arc::new(ConductorDb::new(&self.path))
    }

    /// A temporary directory to use as the workspaces root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The temporary directory holding everything of this database.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }
}
