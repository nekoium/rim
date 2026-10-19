use crate::{
    error::{Error, Result},
    object::{EntryKind, FlatEntry, ObjectStore},
};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

pub fn scan(root: &Path, preserve: &BTreeSet<PathBuf>) -> Result<Vec<FlatEntry>> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .follow_links(false)
        .add_custom_ignore_filename(".rimignore");
    let scan_root = root.to_path_buf();
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        entry
            .path()
            .strip_prefix(&scan_root)
            .ok()
            .is_some_and(|rel| !first_is(rel, ".rim") && !first_is(rel, ".git"))
    });
    let mut found = BTreeSet::new();
    let mut out = Vec::new();
    for item in builder.build() {
        let ent = item.map_err(|e| Error::conflict(format!("cannot scan project: {e}")))?;
        let p = ent.path();
        if p == root {
            continue;
        }
        let rel = p
            .strip_prefix(root)
            .map_err(|_| Error::conflict("project path escaped root"))?
            .to_path_buf();
        if first_is(&rel, ".rim") || first_is(&rel, ".git") {
            continue;
        }
        let meta = fs::symlink_metadata(p)?;
        if meta.file_type().is_dir() {
            if p.join(".git").exists() {
                return Err(Error::conflict(format!(
                    "nested Git repositories and submodules are not supported: {}",
                    rel.display()
                )));
            }
            continue;
        }
        if meta.file_type().is_file() || meta.file_type().is_symlink() {
            out.push(read_entry(p, rel.clone(), &meta)?);
            found.insert(rel);
        } else {
            return Err(Error::conflict(format!(
                "unsupported special file: {}",
                rel.display()
            )));
        }
    }
    // A file already managed by Rim remains in the snapshot even when a new ignore rule hides it.
    for rel in preserve {
        if found.contains(rel) || first_is(rel, ".rim") || first_is(rel, ".git") {
            continue;
        }
        let p = root.join(rel);
        if let Ok(meta) = fs::symlink_metadata(&p) {
            if meta.file_type().is_file() || meta.file_type().is_symlink() {
                out.push(read_entry(&p, rel.clone(), &meta)?);
            } else if !meta.file_type().is_dir() {
                return Err(Error::conflict(format!(
                    "unsupported special file: {}",
                    rel.display()
                )));
            }
        }
    }
    Ok(out)
}
fn first_is(p: &Path, name: &str) -> bool {
    matches!(p.components().next(),Some(Component::Normal(x)) if x==name)
}
fn read_entry(full: &Path, rel: PathBuf, meta: &fs::Metadata) -> Result<FlatEntry> {
    if meta.file_type().is_symlink() {
        let target = fs::read_link(full)?;
        let data = os_bytes(&target)?;
        Ok(FlatEntry {
            path: rel,
            kind: EntryKind::Symlink,
            data,
        })
    } else {
        let executable = is_executable(meta);
        Ok(FlatEntry {
            path: rel,
            kind: if executable {
                EntryKind::Executable
            } else {
                EntryKind::File
            },
            data: fs::read(full)?,
        })
    }
}
#[cfg(unix)]
fn is_executable(m: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}
#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    false
}
#[cfg(unix)]
fn os_bytes(p: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(p.as_os_str().as_bytes().to_vec())
}
#[cfg(not(unix))]
fn os_bytes(p: &Path) -> Result<Vec<u8>> {
    Ok(p.to_str()
        .ok_or_else(|| Error::conflict("non-UTF-8 symbolic link target"))?
        .as_bytes()
        .to_vec())
}

#[derive(Serialize, Deserialize)]
pub struct SwitchManifest {
    pub directory: String,
    pub old_tree: String,
    pub new_tree: String,
    pub root_id: String,
    pub node_id: String,
    #[serde(default)]
    pub cross_root: bool,
}

pub fn check_collisions(
    root: &Path,
    old: &BTreeMap<PathBuf, (EntryKind, String)>,
    new: &BTreeMap<PathBuf, (EntryKind, String)>,
) -> Result<()> {
    for path in new.keys() {
        let full = root.join(path);
        if fs::symlink_metadata(&full).is_ok() && !old.contains_key(path) {
            return Err(Error::conflict(format!(
                "unmanaged path would be overwritten: {}",
                path.display()
            )));
        }
        let mut parent = path.parent();
        while let Some(p) = parent {
            if p.as_os_str().is_empty() {
                break;
            }
            if let Ok(m) = fs::symlink_metadata(root.join(p)) {
                if !m.is_dir() && !old.contains_key(p) {
                    return Err(Error::conflict(format!(
                        "unmanaged path blocks target: {}",
                        p.display()
                    )));
                }
            }
            parent = p.parent();
        }
    }
    // Replacing a directory with a file is safe only when it contains no unmanaged paths.
    for path in new.keys() {
        let full = root.join(path);
        if full.is_dir() {
            for e in walk_all(&full)? {
                let rel = e
                    .strip_prefix(root)
                    .map_err(|_| Error::conflict("path escaped root"))?;
                if !old.contains_key(rel) {
                    return Err(Error::conflict(format!(
                        "unmanaged path would be removed: {}",
                        rel.display()
                    )));
                }
            }
        }
    }
    Ok(())
}
fn walk_all(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        let m = fs::symlink_metadata(&p)?;
        if m.is_dir() {
            out.extend(walk_all(&p)?)
        } else {
            out.push(p)
        }
    }
    Ok(out)
}

pub fn apply(
    store: &ObjectStore,
    root: &Path,
    rim: &Path,
    old_tree: &str,
    new_tree: &str,
    dir_name: &str,
) -> Result<()> {
    let old = store.flatten(old_tree)?;
    let new = store.flatten(new_tree)?;
    check_collisions(root, &old, &new)?;
    let backup = rim.join("transactions").join(dir_name);
    fs::create_dir_all(&backup)?;
    for p in old.keys() {
        let src = root.join(p);
        if fs::symlink_metadata(&src).is_ok() {
            copy_backup(&src, &backup.join(p))?;
        }
    }
    let result = (|| {
        for p in old.keys() {
            remove_path(&root.join(p))?;
        }
        clean_empty(root, old.keys());
        for (p, (k, id)) in &new {
            write_entry(store, &root.join(p), k, id)?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = restore(store, root, old_tree, new_tree, &backup);
        return Err(e);
    }
    Ok(())
}
pub fn restore(
    store: &ObjectStore,
    root: &Path,
    old_tree: &str,
    new_tree: &str,
    backup: &Path,
) -> Result<()> {
    let new = store.flatten(new_tree)?;
    for p in new.keys() {
        remove_path(&root.join(p))?;
    }
    clean_empty(root, new.keys());
    let old = store.flatten(old_tree)?;
    for (p, (k, id)) in old {
        let saved = backup.join(&p);
        if fs::symlink_metadata(&saved).is_ok() {
            copy_path(&saved, &root.join(&p))?
        } else {
            write_entry(store, &root.join(p), &k, &id)?
        }
    }
    Ok(())
}
fn copy_backup(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let metadata = fs::symlink_metadata(src)?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        create_symlink_os(&target, dst)?;
    } else {
        let name = dst
            .file_name()
            .ok_or_else(|| Error::corruption("invalid backup path"))?;
        let tmp = dst.with_file_name(format!(
            ".{}.tmp-{}",
            name.to_string_lossy(),
            std::process::id()
        ));
        fs::copy(src, &tmp)?;
        fs::set_permissions(&tmp, metadata.permissions())?;
        fs::OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
        fs::rename(tmp, dst)?;
    }
    Ok(())
}
fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    if let Some(p) = dst.parent() {
        fs::create_dir_all(p)?
    }
    let m = fs::symlink_metadata(src)?;
    if m.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        create_symlink_os(&target, dst)?
    } else {
        fs::copy(src, dst)?;
        fs::set_permissions(dst, m.permissions())?
    }
    Ok(())
}
fn write_entry(store: &ObjectStore, path: &Path, kind: &EntryKind, id: &str) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?
    }
    let data = store.entry_data(kind, id)?;
    match kind {
        EntryKind::Symlink => create_symlink(&data, path)?,
        EntryKind::File | EntryKind::Executable => {
            fs::write(path, data)?;
            set_exec(path, kind == &EntryKind::Executable)?
        }
        EntryKind::Tree => return Err(Error::corruption("tree appeared as file")),
    }
    Ok(())
}
fn remove_path(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p).map_err(Into::into),
        Ok(_) => fs::remove_file(p).map_err(Into::into),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn clean_empty<'a>(root: &Path, paths: impl Iterator<Item = &'a PathBuf>) {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for p in paths {
        let mut q = p.parent();
        while let Some(x) = q {
            if x.as_os_str().is_empty() {
                break;
            }
            dirs.insert(x.to_path_buf());
            q = x.parent();
        }
    }
    for d in dirs.iter().rev() {
        let _ = fs::remove_dir(root.join(d));
    }
}
#[cfg(unix)]
fn set_exec(p: &Path, yes: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(p)?.permissions();
    let mode = perm.mode();
    perm.set_mode(if yes { mode | 0o111 } else { mode & !0o111 });
    fs::set_permissions(p, perm)?;
    Ok(())
}
#[cfg(not(unix))]
fn set_exec(_: &Path, _: bool) -> Result<()> {
    Ok(())
}
#[cfg(unix)]
fn create_symlink(data: &[u8], p: &Path) -> Result<()> {
    use std::{
        ffi::OsStr,
        os::unix::{ffi::OsStrExt, fs::symlink},
    };
    symlink(OsStr::from_bytes(data), p)?;
    Ok(())
}
#[cfg(windows)]
fn create_symlink(data: &[u8], p: &Path) -> Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    let target =
        std::str::from_utf8(data).map_err(|_| Error::conflict("non-UTF-8 symbolic link target"))?;
    symlink_file(target, p).or_else(|_| symlink_dir(target, p))?;
    Ok(())
}
#[cfg(unix)]
fn create_symlink_os(target: &Path, p: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, p)?;
    Ok(())
}
#[cfg(windows)]
fn create_symlink_os(target: &Path, p: &Path) -> Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    symlink_file(target, p).or_else(|_| symlink_dir(target, p))?;
    Ok(())
}
