use crate::{
    error::{Category, Error, Result},
    fs::{self as project_fs, SwitchManifest},
    git::Git,
    meta::{Meta, Node, Root},
    object::{EntryKind, ObjectStore},
};
use fs4::fs_std::FileExt;
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Repo {
    pub root: PathBuf,
    pub rim: PathBuf,
    pub git: Option<Git>,
    pub meta: Meta,
    pub objects: ObjectStore,
    lock: Option<fs::File>,
}
#[derive(Serialize)]
pub struct Change {
    pub path: String,
    pub status: String,
    pub old_kind: Option<EntryKind>,
    pub new_kind: Option<EntryKind>,
}
impl Repo {
    pub fn discover(cwd: &Path, create: bool) -> Result<Self> {
        let git = Git::discover(cwd)?;
        let root = if let Some(g) = &git {
            g.root.clone()
        } else {
            find_standalone(cwd).unwrap_or_else(|| cwd.to_path_buf())
        };
        let rim = root.join(".rim");
        if !rim.exists() {
            if !create {
                return Err(Error::state(
                    "Rim is not initialized; run `rim init` or `rim spawn`",
                ));
            }
            if let Some(g) = &git {
                g.ensure_rim_untracked()?
            }
            create_layout(&rim)?;
        }
        let format = fs::read_to_string(rim.join("format"))
            .map_err(|e| Error::corruption(format!("cannot read .rim/format: {e}")))?;
        if format.trim() != "rim 1" {
            return Err(Error::corruption("unsupported Rim repository format"));
        }
        let meta = Meta::open(&rim.join("state.db"))?;
        meta.schema()?;
        let objects = ObjectStore::new(&rim);
        Ok(Self {
            root,
            rim,
            git,
            meta,
            objects,
            lock: None,
        })
    }
    pub fn lock(&mut self) -> Result<()> {
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.rim.join("lock"))?;
        f.try_lock_exclusive()
            .map_err(|_| Error::conflict("another Rim mutation is in progress"))?;
        self.lock = Some(f);
        self.recover()
    }
    fn recover(&mut self) -> Result<()> {
        if let Some((_kind, text)) = self.meta.operation()? {
            let m: SwitchManifest = serde_json::from_str(&text)?;
            let backup = self.rim.join("transactions").join(&m.directory);
            project_fs::restore(&self.objects, &self.root, &m.old_tree, &m.new_tree, &backup)?;
            self.meta
                .conn
                .execute("DELETE FROM operation WHERE id=1", [])?;
            let _ = fs::remove_dir_all(backup);
            if m.cross_root {
                return Err(Error::state("recovered an interrupted cross-root switch, but Git HEAD already changed; Rim's active pointer was not changed. Review the Git context and retry the switch"));
            }
            eprintln!("rim: recovered an interrupted switch to the previous complete state");
        }
        Ok(())
    }
    pub fn current_root(&self) -> Result<Option<Root>> {
        if let Some(g) = &self.git {
            self.meta.root_by_origin(&g.head()?)
        } else {
            self.meta.standalone_active()
        }
    }
    pub fn require_current(&self) -> Result<Root> {
        self.current_root()?.ok_or_else(|| {
            Error::state(
                "no Rim root exists for the current project context; run `rim init` or `rim spawn`",
            )
        })
    }
    pub fn initialize(&mut self, new: bool) -> Result<(Root, Node, bool)> {
        if self.git.is_some() && new {
            return Err(Error::new(
                Category::Usage,
                "--new is only valid outside Git",
            ));
        }
        if let Some(r) = self.current_root()? {
            if !new {
                return Ok((r.clone(), self.node(&r.active_node)?, false));
            }
            self.ensure_clean(&r)?
        }
        let (kind, commit, snapshot) = if let Some(g) = &self.git {
            let head = g.head()?;
            if let Some(r) = self.meta.root_by_origin(&head)? {
                return Ok((r.clone(), self.node(&r.active_node)?, false));
            }
            let entries = g.import_tree(&head)?;
            ("git", Some(head), self.objects.snapshot(entries)?)
        } else {
            let entries = project_fs::scan(&self.root, &BTreeSet::new())?;
            ("standalone", None, self.objects.snapshot(entries)?)
        };
        let now = now();
        let rid = id("r");
        let nid = id("n");
        self.meta
            .create_root(&rid, kind, commit.as_deref(), &snapshot, &nid, now)?;
        Ok((self.meta.root(&rid)?.unwrap(), self.node(&nid)?, true))
    }
    pub fn spawn(&mut self, tag: Option<String>, fan: u32) -> Result<Vec<Node>> {
        if fan == 0 {
            return Err(Error::new(Category::Usage, "--fan-out must be at least 1"));
        }
        let (root, _, _) = self.initialize(false)?;
        let parent = self.node(&root.active_node)?;
        let snap = self.workspace_snapshot(&parent.snapshot)?;
        let t = now();
        let mut out = Vec::new();
        for i in 0..fan {
            let actual_tag = tag.as_ref().map(|x| {
                if fan > 1 {
                    format!("{x}-{}", i + 1)
                } else {
                    x.clone()
                }
            });
            out.push(Node {
                id: id("n"),
                root_id: root.id.clone(),
                parent_id: Some(parent.id.clone()),
                parent_snapshot: Some(parent.snapshot.clone()),
                snapshot: snap.clone(),
                tag: actual_tag,
                punch_message: None,
                created_at: t,
                updated_at: t,
            });
        }
        let activate = if fan == 1 {
            Some((root.id.as_str(), out[0].id.as_str()))
        } else {
            None
        };
        self.meta.create_nodes(&out, activate)?;
        Ok(out)
    }
    pub fn punch(&mut self, msg: Option<&str>) -> Result<(Node, bool)> {
        let root = self.require_current()?;
        let n = self.node(&root.active_node)?;
        let snap = self.workspace_snapshot(&n.snapshot)?;
        let changed = snap != n.snapshot;
        if changed || msg.is_some() {
            self.meta.update_snapshot(&n.id, &snap, msg, now())?
        }
        Ok((self.node(&n.id)?, changed))
    }
    pub fn ensure_clean(&self, r: &Root) -> Result<()> {
        let n = self.node(&r.active_node)?;
        let snap = self.workspace_snapshot(&n.snapshot)?;
        if snap != n.snapshot {
            return Err(Error::conflict(
                "the active node has hot changes; use `rim punch` or `rim spawn` first",
            ));
        }
        Ok(())
    }
    pub fn switch(&mut self, target_id: &str, allow_git: bool) -> Result<Node> {
        let target = self.node(target_id)?;
        let target_root = self
            .meta
            .root(&target.root_id)?
            .ok_or_else(|| Error::corruption("node has no root"))?;
        let current = self.require_current()?;
        self.ensure_clean(&current)?;
        if target.id == current.active_node && target.root_id == current.id {
            return Ok(target);
        }
        let old = self.node(&current.active_node)?;
        let mut materialized_old_tree = old.snapshot.clone();
        if target.root_id != current.id {
            match (&self.git, &target_root.origin_commit) {
                (Some(g), Some(commit)) => {
                    if !allow_git {
                        return Err(Error::new(Category::Confirmation,format!("target root originates at Git commit {commit}, which differs from HEAD; rerun with --yes to allow Rim to ask Git to change HEAD, or change HEAD manually")));
                    }
                    if !g.commit_exists(commit)? {
                        return Err(Error::new(
                            Category::Git,
                            format!("target origin commit no longer exists: {commit}"),
                        ));
                    }
                    g.checkout(commit)?;
                    // Git has now materialized the target root's committed baseline. Use that
                    // independent Rim copy as the rollback/materialization baseline; using the
                    // former root here could remove paths Git just checked out.
                    materialized_old_tree = target_root.base_snapshot.clone();
                }
                (None, None) => {
                    // Standalone cross-root switching starts from the currently visible node.
                }
                _ => return Err(Error::state("target belongs to a different project mode")),
            }
        }
        let old_map = self.objects.flatten(&materialized_old_tree)?;
        let new_map = self.objects.flatten(&target.snapshot)?;
        project_fs::check_collisions(&self.root, &old_map, &new_map)?;
        let dir = format!("switch-{}", id("t"));
        let manifest = SwitchManifest {
            directory: dir.clone(),
            old_tree: materialized_old_tree.clone(),
            new_tree: target.snapshot.clone(),
            root_id: target_root.id.clone(),
            node_id: target.id.clone(),
            cross_root: target.root_id != current.id && self.git.is_some(),
        };
        self.meta
            .set_operation("switch", &serde_json::to_string(&manifest)?)?;
        if let Err(e) = project_fs::apply(
            &self.objects,
            &self.root,
            &self.rim,
            &materialized_old_tree,
            &target.snapshot,
            &dir,
        ) {
            return Err(Error::new(
                e.category,
                format!("switch materialization failed and the old files were restored: {e}"),
            ));
        }
        self.meta
            .clear_operation_and_activate(&target_root.id, &target.id)?;
        let _ = fs::remove_dir_all(self.rim.join("transactions").join(dir));
        Ok(target)
    }
    pub fn status(&self, node_id: Option<&str>) -> Result<(Node, Vec<Change>)> {
        let root = self.require_current()?;
        let n = if let Some(id) = node_id {
            self.node(id)?
        } else {
            self.node(&root.active_node)?
        };
        let current = self.workspace_snapshot(&n.snapshot)?;
        let changes = self.diff_trees(&n.snapshot, &current, None)?;
        Ok((n, changes))
    }
    pub fn diff(&self, a: &str, b: Option<&str>, filters: &[PathBuf]) -> Result<Vec<Change>> {
        let n = self.node(a)?;
        let base = if let Some(x) = b {
            self.node(x)?.snapshot
        } else {
            n.parent_snapshot
                .clone()
                .ok_or_else(|| Error::state("the node has no recorded parent snapshot"))?
        };
        let f = if filters.is_empty() {
            None
        } else {
            Some(filters)
        };
        self.diff_trees(&base, &n.snapshot, f)
    }
    fn diff_trees(&self, old: &str, new: &str, filters: Option<&[PathBuf]>) -> Result<Vec<Change>> {
        if old == new {
            return Ok(Vec::new());
        }
        let a = self.objects.flatten(old)?;
        let b = self.objects.flatten(new)?;
        let paths: BTreeSet<_> = a.keys().chain(b.keys()).cloned().collect();
        let mut out = Vec::new();
        for p in paths {
            if filters.is_some_and(|f| !f.iter().any(|x| p == *x || p.starts_with(x))) {
                continue;
            }
            let av = a.get(&p);
            let bv = b.get(&p);
            if av == bv {
                continue;
            }
            let status = match (av, bv) {
                (None, Some(_)) => "added",
                (Some(_), None) => "deleted",
                _ => "modified",
            };
            out.push(Change {
                path: path_json(&p),
                status: status.into(),
                old_kind: av.map(|x| x.0.clone()),
                new_kind: bv.map(|x| x.0.clone()),
            })
        }
        Ok(out)
    }
    pub fn purge(&mut self, id: &str) -> Result<usize> {
        let target = self.node(id)?;
        let root = self
            .meta
            .root(&target.root_id)?
            .ok_or_else(|| Error::corruption("node has no root"))?;
        let nodes = self.meta.nodes()?;
        let mut stack = vec![id.to_owned()];
        let mut sub = BTreeSet::new();
        while let Some(x) = stack.pop() {
            sub.insert(x.clone());
            for n in &nodes {
                if n.parent_id.as_deref() == Some(&x) {
                    stack.push(n.id.clone())
                }
            }
        }
        if sub.contains(&root.active_node) {
            return Err(Error::conflict(
                "cannot purge a subtree containing the active node; switch outside it first",
            ));
        }
        self.meta.delete_subtree(id)
    }
    pub fn prune(&mut self, all: bool) -> Result<(usize, usize)> {
        let r = self.require_current()?;
        if all {
            self.meta.prune_all(&r)
        } else {
            Ok((self.meta.prune_root(&r)?, 0))
        }
    }
    pub fn tree(&self) -> Result<(Option<String>, Vec<Root>, Vec<Node>)> {
        let current = self.current_root()?.map(|x| x.id);
        Ok((current, self.meta.roots()?, self.meta.nodes()?))
    }
    pub fn node(&self, id: &str) -> Result<Node> {
        self.meta
            .node(id)?
            .ok_or_else(|| Error::state(format!("unknown node: {id}")))
    }
    fn workspace_snapshot(&self, preserve_tree: &str) -> Result<String> {
        let preserve = self.objects.flatten(preserve_tree)?.into_keys().collect();
        self.objects
            .snapshot(project_fs::scan(&self.root, &preserve)?)
    }
}
fn create_layout(rim: &Path) -> Result<()> {
    fs::create_dir_all(rim.join("objects"))?;
    fs::create_dir_all(rim.join("transactions"))?;
    fs::create_dir_all(rim.join("tmp"))?;
    fs::write(rim.join("format"), "rim 1\n")?;
    fs::write(rim.join(".gitignore"), "*\n")?;
    Ok(())
}
fn find_standalone(cwd: &Path) -> Option<PathBuf> {
    for p in cwd.ancestors() {
        if p.join(".rim/format").is_file() {
            return Some(p.to_path_buf());
        }
    }
    None
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    let instant = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let text = format!(
        "{}:{}:{}:{}",
        prefix,
        instant,
        std::process::id(),
        C.fetch_add(1, Ordering::Relaxed)
    );
    format!("{prefix}{}", &blake3::hash(text.as_bytes()).to_hex()[..15])
}
fn path_json(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
