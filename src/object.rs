use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub type ObjectId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Executable,
    Symlink,
    Tree,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub id: ObjectId,
}

#[derive(Debug, Clone)]
pub struct FlatEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
    pub data: Vec<u8>,
}

#[derive(Clone)]
pub struct ObjectStore {
    root: PathBuf,
}

impl ObjectStore {
    pub fn new(rim: &Path) -> Self {
        Self {
            root: rim.join("objects"),
        }
    }
    fn canonical(kind: &str, payload: &[u8]) -> Vec<u8> {
        let mut out = format!("RIMOBJ\0v1\0{kind}\0{}\0", payload.len()).into_bytes();
        out.extend(payload);
        out
    }
    fn object_path(&self, id: &str) -> Result<PathBuf> {
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::corruption("invalid object ID"));
        }
        Ok(self.root.join(&id[..2]).join(&id[2..]))
    }
    pub fn put(&self, kind: &str, payload: &[u8]) -> Result<ObjectId> {
        let bytes = Self::canonical(kind, payload);
        let id = blake3::hash(&bytes).to_hex().to_string();
        let path = self.object_path(&id)?;
        if path.exists() {
            // Never trust an existing loose object merely because its name matches.
            // This also prevents new metadata from committing a reference to a
            // previously truncated or corrupted object.
            let existing = self.get(&id, kind)?;
            if existing != payload {
                return Err(Error::corruption(format!(
                    "object {id} has conflicting content"
                )));
            }
            return Ok(id);
        }
        let parent = path
            .parent()
            .ok_or_else(|| Error::corruption("invalid object path"))?;
        fs::create_dir_all(parent)?;
        let tmp = self
            .root
            .parent()
            .ok_or_else(|| Error::corruption("invalid store"))?
            .join("tmp")
            .join(format!("object-{id}-{}", std::process::id()));
        let encoded = zstd::stream::encode_all(bytes.as_slice(), 3)?;
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&encoded)?;
            f.sync_all()?;
        }
        match fs::rename(&tmp, &path) {
            Ok(()) => {}
            Err(_e) if path.exists() => {
                let _ = fs::remove_file(&tmp);
            }
            Err(e) => return Err(e.into()),
        }
        Ok(id)
    }
    pub fn get(&self, id: &str, expected: &str) -> Result<Vec<u8>> {
        let compressed = fs::read(self.object_path(id)?)?;
        let bytes = zstd::stream::decode_all(compressed.as_slice())
            .map_err(|e| Error::corruption(format!("cannot decompress object {id}: {e}")))?;
        if blake3::hash(&bytes).to_hex().as_str() != id {
            return Err(Error::corruption(format!(
                "object {id} failed integrity check"
            )));
        }
        let prefix = format!("RIMOBJ\0v1\0{expected}\0");
        if !bytes.starts_with(prefix.as_bytes()) {
            return Err(Error::corruption(format!(
                "object {id} has unexpected type"
            )));
        }
        let rest = &bytes[prefix.len()..];
        let split = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| Error::corruption("invalid object header"))?;
        let len: usize = std::str::from_utf8(&rest[..split])
            .map_err(|_| Error::corruption("invalid object length"))?
            .parse()
            .map_err(|_| Error::corruption("invalid object length"))?;
        let payload = &rest[split + 1..];
        if payload.len() != len {
            return Err(Error::corruption(format!("object {id} has invalid length")));
        }
        Ok(payload.to_vec())
    }
    fn tree_payload(entries: &[TreeEntry]) -> Result<Vec<u8>> {
        // JSON is canonical here: fields are fixed and entries are bytewise-name sorted.
        Ok(serde_json::to_vec(entries)?)
    }
    pub fn put_tree(&self, entries: &[TreeEntry]) -> Result<ObjectId> {
        self.put("tree", &Self::tree_payload(entries)?)
    }
    pub fn get_tree(&self, id: &str) -> Result<Vec<TreeEntry>> {
        let entries: Vec<TreeEntry> = serde_json::from_slice(&self.get(id, "tree")?)?;
        let mut prior: Option<&str> = None;
        for e in &entries {
            validate_name(&e.name)?;
            if prior.is_some_and(|p| p.as_bytes() >= e.name.as_bytes()) {
                return Err(Error::corruption("tree entries are not canonical"));
            }
            prior = Some(&e.name);
        }
        Ok(entries)
    }
    pub fn snapshot(&self, flat: Vec<FlatEntry>) -> Result<ObjectId> {
        #[derive(Default)]
        struct Dir {
            files: BTreeMap<String, (EntryKind, Vec<u8>)>,
            dirs: BTreeMap<String, Dir>,
        }
        fn insert(dir: &mut Dir, parts: &[String], kind: EntryKind, data: Vec<u8>) -> Result<()> {
            if parts.is_empty() {
                return Err(Error::conflict("empty snapshot path"));
            }
            if parts.len() == 1 {
                if dir.dirs.contains_key(&parts[0]) {
                    return Err(Error::conflict("file/directory collision"));
                }
                dir.files.insert(parts[0].clone(), (kind, data));
            } else {
                if dir.files.contains_key(&parts[0]) {
                    return Err(Error::conflict("file/directory collision"));
                }
                insert(
                    dir.dirs.entry(parts[0].clone()).or_default(),
                    &parts[1..],
                    kind,
                    data,
                )?;
            }
            Ok(())
        }
        fn save(store: &ObjectStore, d: Dir) -> Result<ObjectId> {
            let mut entries = Vec::new();
            for (name, child) in d.dirs {
                let id = save(store, child)?;
                entries.push(TreeEntry {
                    name,
                    kind: EntryKind::Tree,
                    id,
                });
            }
            for (name, (kind, data)) in d.files {
                let typ = if kind == EntryKind::Symlink {
                    "link"
                } else {
                    "blob"
                };
                let id = store.put(typ, &data)?;
                entries.push(TreeEntry { name, kind, id });
            }
            entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
            store.put_tree(&entries)
        }
        let mut root = Dir::default();
        for e in flat {
            let parts: Vec<String> = e
                .path
                .components()
                .map(|c| {
                    c.as_os_str()
                        .to_str()
                        .map(ToOwned::to_owned)
                        .ok_or_else(|| Error::conflict("Rim MVP does not support non-UTF-8 paths"))
                })
                .collect::<Result<_>>()?;
            for p in &parts {
                validate_name(p)?;
            }
            insert(&mut root, &parts, e.kind, e.data)?;
        }
        save(self, root)
    }
    pub fn flatten(&self, tree: &str) -> Result<BTreeMap<PathBuf, (EntryKind, ObjectId)>> {
        fn walk(
            s: &ObjectStore,
            id: &str,
            base: &Path,
            out: &mut BTreeMap<PathBuf, (EntryKind, ObjectId)>,
            seen: &mut BTreeSet<String>,
        ) -> Result<()> {
            if !seen.insert(id.to_owned()) {
                return Err(Error::corruption("tree cycle detected"));
            }
            for e in s.get_tree(id)? {
                let p = base.join(&e.name);
                if e.kind == EntryKind::Tree {
                    walk(s, &e.id, &p, out, seen)?;
                } else {
                    out.insert(p, (e.kind, e.id));
                }
            }
            seen.remove(id);
            Ok(())
        }
        let mut out = BTreeMap::new();
        walk(self, tree, Path::new(""), &mut out, &mut BTreeSet::new())?;
        Ok(out)
    }
    pub fn entry_data(&self, kind: &EntryKind, id: &str) -> Result<Vec<u8>> {
        self.get(
            id,
            if kind == &EntryKind::Symlink {
                "link"
            } else {
                "blob"
            },
        )
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        Err(Error::corruption(format!("unsafe tree name: {name:?}")))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    #[test]
    fn deterministic_and_round_trip() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("objects")).unwrap();
        fs::create_dir_all(d.path().join("tmp")).unwrap();
        let s = ObjectStore::new(d.path());
        let a = s
            .snapshot(vec![
                FlatEntry {
                    path: "b/x".into(),
                    kind: EntryKind::File,
                    data: b"x".to_vec(),
                },
                FlatEntry {
                    path: "a".into(),
                    kind: EntryKind::File,
                    data: b"a".to_vec(),
                },
            ])
            .unwrap();
        let b = s
            .snapshot(vec![
                FlatEntry {
                    path: "a".into(),
                    kind: EntryKind::File,
                    data: b"a".to_vec(),
                },
                FlatEntry {
                    path: "b/x".into(),
                    kind: EntryKind::File,
                    data: b"x".to_vec(),
                },
            ])
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(s.flatten(&a).unwrap().len(), 2)
    }
    #[test]
    fn unchanged_subtrees_are_structurally_shared() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("objects")).unwrap();
        fs::create_dir_all(d.path().join("tmp")).unwrap();
        let s = ObjectStore::new(d.path());
        let first = s
            .snapshot(vec![
                FlatEntry {
                    path: "stable/x".into(),
                    kind: EntryKind::File,
                    data: b"same".to_vec(),
                },
                FlatEntry {
                    path: "changed".into(),
                    kind: EntryKind::File,
                    data: b"one".to_vec(),
                },
            ])
            .unwrap();
        let second = s
            .snapshot(vec![
                FlatEntry {
                    path: "stable/x".into(),
                    kind: EntryKind::File,
                    data: b"same".to_vec(),
                },
                FlatEntry {
                    path: "changed".into(),
                    kind: EntryKind::File,
                    data: b"two".to_vec(),
                },
            ])
            .unwrap();
        let stable = |root: &str| {
            s.get_tree(root)
                .unwrap()
                .into_iter()
                .find(|e| e.name == "stable")
                .unwrap()
                .id
        };
        assert_eq!(stable(&first), stable(&second));
        assert_ne!(first, second);
    }

    #[test]
    fn detects_corruption() {
        let d = tempdir().unwrap();
        fs::create_dir_all(d.path().join("objects")).unwrap();
        fs::create_dir_all(d.path().join("tmp")).unwrap();
        let s = ObjectStore::new(d.path());
        let id = s.put("blob", b"hello").unwrap();
        fs::write(s.object_path(&id).unwrap(), b"bad").unwrap();
        assert!(s.get(&id, "blob").is_err())
    }
}
