use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct Root {
    pub id: String,
    pub origin_kind: String,
    pub origin_commit: Option<String>,
    pub base_snapshot: String,
    pub active_node: String,
    pub created_at: i64,
}
#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: String,
    pub root_id: String,
    pub parent_id: Option<String>,
    pub parent_snapshot: Option<String>,
    pub snapshot: String,
    pub tag: Option<String>,
    pub punch_message: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct Meta {
    pub conn: Connection,
}
impl Meta {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { conn })
    }
    pub fn schema(&self) -> Result<()> {
        self.conn.execute_batch("BEGIN;
 CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
 CREATE TABLE IF NOT EXISTS roots(id TEXT PRIMARY KEY,origin_kind TEXT NOT NULL,origin_commit TEXT UNIQUE,base_snapshot TEXT NOT NULL,active_node TEXT NOT NULL,created_at INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS nodes(id TEXT PRIMARY KEY,root_id TEXT NOT NULL REFERENCES roots(id) ON DELETE CASCADE,parent_id TEXT REFERENCES nodes(id) ON DELETE CASCADE,parent_snapshot TEXT,snapshot TEXT NOT NULL,tag TEXT,punch_message TEXT,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS operation(id INTEGER PRIMARY KEY CHECK(id=1),kind TEXT NOT NULL,manifest TEXT NOT NULL);
 INSERT OR IGNORE INTO settings VALUES('schema_version','1'); COMMIT;")?;
        Ok(())
    }
    pub fn root_by_origin(&self, commit: &str) -> Result<Option<Root>> {
        self.conn.query_row("SELECT id,origin_kind,origin_commit,base_snapshot,active_node,created_at FROM roots WHERE origin_commit=?",[commit],row_root).optional().map_err(Into::into)
    }
    pub fn root(&self, id: &str) -> Result<Option<Root>> {
        self.conn.query_row("SELECT id,origin_kind,origin_commit,base_snapshot,active_node,created_at FROM roots WHERE id=?",[id],row_root).optional().map_err(Into::into)
    }
    pub fn standalone_active(&self) -> Result<Option<Root>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key='standalone_active_root'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match id {
            Some(x) => self.root(&x),
            None => Ok(None),
        }
    }
    pub fn node(&self, id: &str) -> Result<Option<Node>> {
        self.conn.query_row("SELECT id,root_id,parent_id,parent_snapshot,snapshot,tag,punch_message,created_at,updated_at FROM nodes WHERE id=?",[id],row_node).optional().map_err(Into::into)
    }
    pub fn nodes(&self) -> Result<Vec<Node>> {
        let mut q=self.conn.prepare("SELECT id,root_id,parent_id,parent_snapshot,snapshot,tag,punch_message,created_at,updated_at FROM nodes ORDER BY created_at,id")?;
        let rows = q
            .query_map([], row_node)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
    pub fn roots(&self) -> Result<Vec<Root>> {
        let mut q=self.conn.prepare("SELECT id,origin_kind,origin_commit,base_snapshot,active_node,created_at FROM roots ORDER BY created_at,id")?;
        let rows = q
            .query_map([], row_root)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
    pub fn create_root(
        &mut self,
        id: &str,
        kind: &str,
        commit: Option<&str>,
        snapshot: &str,
        node_id: &str,
        now: i64,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("INSERT INTO roots(id,origin_kind,origin_commit,base_snapshot,active_node,created_at)VALUES(?,?,?,?,?,?)",params![id,kind,commit,snapshot,node_id,now])?;
        tx.execute("INSERT INTO nodes(id,root_id,parent_id,parent_snapshot,snapshot,created_at,updated_at)VALUES(?,?,NULL,NULL,?,?,?)",params![node_id,id,snapshot,now,now])?;
        if kind == "standalone" {
            tx.execute("INSERT INTO settings(key,value)VALUES('standalone_active_root',?) ON CONFLICT(key)DO UPDATE SET value=excluded.value",[id])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn create_nodes(&mut self, nodes: &[Node], new_active: Option<(&str, &str)>) -> Result<()> {
        let tx = self.conn.transaction()?;
        for n in nodes {
            tx.execute("INSERT INTO nodes(id,root_id,parent_id,parent_snapshot,snapshot,tag,punch_message,created_at,updated_at)VALUES(?,?,?,?,?,?,?,?,?)",params![n.id,n.root_id,n.parent_id,n.parent_snapshot,n.snapshot,n.tag,n.punch_message,n.created_at,n.updated_at])?;
        }
        if let Some((root, node)) = new_active {
            tx.execute(
                "UPDATE roots SET active_node=? WHERE id=?",
                params![node, root],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn update_snapshot(
        &self,
        node: &str,
        snapshot: &str,
        msg: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE nodes SET snapshot=?,punch_message=?,updated_at=? WHERE id=?",
            params![snapshot, msg, now, node],
        )?;
        Ok(())
    }
    pub fn operation(&self) -> Result<Option<(String, String)>> {
        self.conn
            .query_row("SELECT kind,manifest FROM operation WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .map_err(Into::into)
    }
    pub fn set_operation(&self, kind: &str, manifest: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO operation(id,kind,manifest)VALUES(1,?,?)",
            params![kind, manifest],
        )?;
        Ok(())
    }
    pub fn clear_operation_and_activate(&mut self, root: &str, node: &str) -> Result<()> {
        let standalone = self
            .root(root)?
            .is_some_and(|r| r.origin_kind == "standalone");
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE roots SET active_node=? WHERE id=?",
            params![node, root],
        )?;
        if standalone {
            tx.execute("INSERT INTO settings(key,value)VALUES('standalone_active_root',?) ON CONFLICT(key)DO UPDATE SET value=excluded.value", [root])?;
        }
        tx.execute("DELETE FROM operation WHERE id=1", [])?;
        tx.commit()?;
        Ok(())
    }
    pub fn delete_subtree(&mut self, id: &str) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let count = tx.query_row(
            "WITH RECURSIVE d(id) AS(SELECT ? UNION ALL SELECT nodes.id FROM nodes JOIN d ON nodes.parent_id=d.id) SELECT count(*) FROM d",
            [id],
            |r| r.get::<_, usize>(0),
        )?;
        tx.execute("WITH RECURSIVE d(id) AS(SELECT ? UNION ALL SELECT nodes.id FROM nodes JOIN d ON nodes.parent_id=d.id) DELETE FROM nodes WHERE id IN d",[id])?;
        tx.commit()?;
        Ok(count)
    }
    pub fn prune_root(&mut self, root: &Root) -> Result<usize> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE nodes SET parent_id=NULL,parent_snapshot=NULL WHERE id=?",
            [&root.active_node],
        )?;
        let removed = tx.execute(
            "DELETE FROM nodes WHERE root_id=? AND id<>?",
            params![root.id, root.active_node],
        )?;
        tx.commit()?;
        Ok(removed)
    }
    pub fn prune_all(&mut self, root: &Root) -> Result<(usize, usize)> {
        let nodes = self.conn.query_row(
            "SELECT count(*) FROM nodes WHERE id<>?",
            [&root.active_node],
            |r| r.get::<_, usize>(0),
        )?;
        let roots =
            self.conn
                .query_row("SELECT count(*) FROM roots WHERE id<>?", [&root.id], |r| {
                    r.get::<_, usize>(0)
                })?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE nodes SET parent_id=NULL,parent_snapshot=NULL WHERE id=?",
            [&root.active_node],
        )?;
        tx.execute("DELETE FROM roots WHERE id<>?", [&root.id])?;
        tx.execute(
            "DELETE FROM nodes WHERE root_id=? AND id<>?",
            params![root.id, root.active_node],
        )?;
        if root.origin_kind == "standalone" {
            tx.execute("INSERT INTO settings(key,value)VALUES('standalone_active_root',?) ON CONFLICT(key)DO UPDATE SET value=excluded.value",[&root.id])?;
        }
        tx.commit()?;
        Ok((nodes, roots))
    }
}
fn row_root(r: &rusqlite::Row<'_>) -> rusqlite::Result<Root> {
    Ok(Root {
        id: r.get(0)?,
        origin_kind: r.get(1)?,
        origin_commit: r.get(2)?,
        base_snapshot: r.get(3)?,
        active_node: r.get(4)?,
        created_at: r.get(5)?,
    })
}
fn row_node(r: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        id: r.get(0)?,
        root_id: r.get(1)?,
        parent_id: r.get(2)?,
        parent_snapshot: r.get(3)?,
        snapshot: r.get(4)?,
        tag: r.get(5)?,
        punch_message: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}
