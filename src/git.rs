use crate::{
    error::{Category, Error, Result},
    object::{EntryKind, FlatEntry},
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[derive(Debug, Clone)]
pub struct Git {
    pub root: PathBuf,
}
fn run_at(cwd: &Path, args: &[&str]) -> std::io::Result<Output> {
    Command::new("git").args(args).current_dir(cwd).output()
}
impl Git {
    pub fn discover(cwd: &Path) -> Result<Option<Self>> {
        let out = match run_at(cwd, &["rev-parse", "--show-toplevel"]) {
            Ok(x) => x,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !out.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8(out.stdout)
            .map_err(|_| Error::new(Category::Git, "Git returned a non-UTF-8 project path"))?;
        Ok(Some(Self {
            root: PathBuf::from(text.trim()),
        }))
    }
    fn command(&self, args: &[&str]) -> Result<Output> {
        run_at(&self.root, args)
            .map_err(|e| Error::new(Category::Git, format!("could not run Git: {e}")))
    }
    pub fn head(&self) -> Result<String> {
        let o = self.command(&["rev-parse", "--verify", "HEAD^{commit}"])?;
        if !o.status.success() {
            return Err(git_error("cannot resolve Git HEAD", &o));
        }
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    }
    pub fn ensure_rim_untracked(&self) -> Result<()> {
        let o = self.command(&["ls-files", "-z", "--", ".rim"])?;
        if !o.status.success() {
            return Err(git_error("cannot inspect tracked files", &o));
        }
        if !o.stdout.is_empty() {
            return Err(Error::conflict(
                "cannot initialize: Git already tracks .rim/",
            ));
        }
        Ok(())
    }
    pub fn commit_exists(&self, id: &str) -> Result<bool> {
        let spec = format!("{id}^{{commit}}");
        let o = self.command(&["cat-file", "-e", &spec])?;
        Ok(o.status.success())
    }
    pub fn checkout(&self, id: &str) -> Result<()> {
        let o = self.command(&["checkout", id])?;
        if !o.status.success() {
            return Err(git_error("Git refused to change HEAD", &o));
        }
        Ok(())
    }
    pub fn import_tree(&self, commit: &str) -> Result<Vec<FlatEntry>> {
        let o = self.command(&["ls-tree", "-rz", commit])?;
        if !o.status.success() {
            return Err(git_error("cannot read origin tree", &o));
        }
        let mut out = Vec::new();
        for rec in o.stdout.split(|b| *b == 0).filter(|x| !x.is_empty()) {
            let tab = rec
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| Error::new(Category::Git, "invalid Git tree output"))?;
            let header = std::str::from_utf8(&rec[..tab])
                .map_err(|_| Error::new(Category::Git, "invalid Git tree output"))?;
            let mut h = header.split(' ');
            let mode = h.next().unwrap_or("");
            let kind = h.next().unwrap_or("");
            let oid = h.next().unwrap_or("");
            let path = std::str::from_utf8(&rec[tab + 1..]).map_err(|_| {
                Error::new(
                    Category::Git,
                    "Rim MVP does not support non-UTF-8 Git paths",
                )
            })?;
            if mode == "160000" || kind == "commit" {
                return Err(Error::conflict(format!(
                    "Git submodule is not supported: {path}"
                )));
            }
            if kind != "blob" {
                return Err(Error::conflict(format!(
                    "unsupported Git tree entry: {path}"
                )));
            }
            let b = self.command(&["cat-file", "blob", oid])?;
            if !b.status.success() {
                return Err(git_error("cannot read Git blob", &b));
            }
            let ek = match mode {
                "120000" => EntryKind::Symlink,
                "100755" => EntryKind::Executable,
                "100644" => EntryKind::File,
                _ => {
                    return Err(Error::conflict(format!(
                        "unsupported Git mode {mode}: {path}"
                    )))
                }
            };
            out.push(FlatEntry {
                path: path.into(),
                kind: ek,
                data: b.stdout,
            });
        }
        Ok(out)
    }
}
fn git_error(prefix: &str, o: &Output) -> Error {
    let msg = String::from_utf8_lossy(&o.stderr).trim().to_owned();
    Error::new(
        Category::Git,
        if msg.is_empty() {
            prefix.to_owned()
        } else {
            format!("{prefix}: {msg}")
        },
    )
}
