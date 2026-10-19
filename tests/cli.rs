use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

struct Project {
    path: PathBuf,
    _temp: TempDir,
}
impl Project {
    fn new(name: &str) -> Self {
        // Standalone fixtures must live outside this repository so they cannot
        // discover the repository's own .rim directory as an ancestor.
        let temp = tempfile::Builder::new()
            .prefix(&format!("rim-{name}-"))
            .tempdir()
            .unwrap();
        let path = temp.path().to_path_buf();
        Self { path, _temp: temp }
    }
    fn rim(&self, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_rim"));
        c.args(args).current_dir(&self.path);
        // Prevent a standalone fixture under this repository from discovering its parent Git repository.
        c.env("GIT_CEILING_DIRECTORIES", &self.path);
        c.env("GIT_DIR", self.path.join(".standalone-no-git"));
        c.output().unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let out = self.rim(args);
        assert!(
            out.status.success(),
            "rim {:?} failed: stderr={} stdout={}",
            args,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn git(&self, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(&self.path)
            .output()
            .unwrap()
    }
}
fn write(root: &Path, name: &str, text: &str) {
    let p = root.join(name);
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).unwrap()
    }
    fs::write(p, text).unwrap();
}
fn data(v: &Value) -> &Value {
    &v["data"]
}

#[test]
fn help_explains_the_model_and_destructive_scope() {
    let help = Command::new(env!("CARGO_BIN_EXE_rim"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("It does not create Git branches or commits"));
    assert!(help.contains("rim punch    Update the active node in place"));
    assert!(help.contains("rim spawn    Capture changes in a new child candidate"));
    assert!(help.contains("Delete an inactive node and its complete subtree"));

    let prune = Command::new(env!("CARGO_BIN_EXE_rim"))
        .args(["prune", "--help"])
        .output()
        .unwrap();
    assert!(prune.status.success());
    let prune = String::from_utf8(prune.stdout).unwrap();
    assert!(prune.contains("including its ancestors, siblings, and descendants"));
    assert!(prune.contains("remove every other root in the repository scope"));

    let switch = Command::new(env!("CARGO_BIN_EXE_rim"))
        .args(["switch", "--help"])
        .output()
        .unwrap();
    assert!(switch.status.success());
    let switch = String::from_utf8(switch.stdout).unwrap();
    assert!(switch.contains("refuses to discard hot changes"));
    assert!(switch.contains("Git to change HEAD"));
}

#[test]
fn standalone_spawn_punch_switch_and_json() {
    let p = Project::new("standalone");
    write(&p.path, "a.txt", "base");
    let init = p.json(&["init", "--json"]);
    assert_eq!(init["version"], 1);
    let parent = data(&init)["node"]["id"].as_str().unwrap().to_owned();
    write(&p.path, "a.txt", "child");
    let spawned = p.json(&["spawn", "--tag", "candidate", "--json"]);
    let child = data(&spawned)["nodes"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    write(&p.path, "a.txt", "punched");
    let punched = p.json(&["punch", "-m", "save", "--json"]);
    assert_eq!(data(&punched)["node"]["id"], child);
    assert_eq!(data(&punched)["changed"], true);
    let status = p.json(&["status", "--json"]);
    assert_eq!(data(&status)["hot"], false);
    assert!(p.rim(&["switch", &parent]).status.success());
    assert_eq!(fs::read_to_string(p.path.join("a.txt")).unwrap(), "base");
    assert!(p.rim(&["switch", &child]).status.success());
    assert_eq!(fs::read_to_string(p.path.join("a.txt")).unwrap(), "punched");
    let diff = p.json(&["diff", &child, "a.txt", "--json"]);
    assert_eq!(data(&diff)["paths"][0], "a.txt");
    assert!(!data(&diff)["changes"].as_array().unwrap().is_empty());
}

#[test]
fn standalone_new_root_fanout_purge_and_prune() {
    let p = Project::new("roots");
    write(&p.path, "x", "one");
    let first = p.json(&["init", "--json"]);
    let old = data(&first)["node"]["id"].as_str().unwrap().to_owned();
    let fan = p.json(&["spawn", "--fan-out", "2", "--tag", "try", "--json"]);
    let candidates = data(&fan)["nodes"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    let doomed = candidates[0]["id"].as_str().unwrap().to_owned();
    assert!(p.rim(&["purge", &doomed, "--yes"]).status.success());
    write(&p.path, "x", "two");
    assert!(p.rim(&["punch"]).status.success());
    let second = p.json(&["init", "--new", "--json"]);
    assert_ne!(data(&second)["node"]["id"], old);
    let pruned = p.json(&["prune", "--all", "--yes", "--json"]);
    assert!(data(&pruned)["removed_roots"].as_u64().unwrap() >= 1);
    let tree = p.json(&["tree", "--json"]);
    assert_eq!(data(&tree)["roots"].as_array().unwrap().len(), 1);
    assert_eq!(data(&tree)["nodes"].as_array().unwrap().len(), 1);
}

#[test]
fn git_base_is_committed_and_staging_remains_git_owned() {
    let p = Project::new("git");
    assert!(p.git(&["init", "-q"]).status.success());
    p.git(&["config", "user.email", "rim@example.invalid"]);
    p.git(&["config", "user.name", "Rim Test"]);
    write(&p.path, "tracked", "committed");
    p.git(&["add", "tracked"]);
    assert!(p.git(&["commit", "-qm", "base"]).status.success());
    write(&p.path, "tracked", "staged");
    p.git(&["add", "tracked"]);
    // Do not apply the ceiling to direct Git setup, but Rim must discover this fixture's own .git.
    let mut c = Command::new(env!("CARGO_BIN_EXE_rim"));
    let init = c
        .args(["init", "--json"])
        .current_dir(&p.path)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let mut c = Command::new(env!("CARGO_BIN_EXE_rim"));
    let status = c
        .args(["status", "--json"])
        .current_dir(&p.path)
        .output()
        .unwrap();
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(data(&status)["hot"], true);
    let mut c = Command::new(env!("CARGO_BIN_EXE_rim"));
    assert!(c
        .args(["punch"])
        .current_dir(&p.path)
        .status()
        .unwrap()
        .success());
    let cached = p.git(&["diff", "--cached", "--", "tracked"]);
    assert!(cached.status.success());
    assert!(String::from_utf8_lossy(&cached.stdout).contains("staged"));
}

#[test]
fn git_roots_follow_commits_and_cross_root_switch_changes_head() {
    let p = Project::new("git-roots");
    assert!(p.git(&["init", "-q"]).status.success());
    p.git(&["config", "user.email", "rim@example.invalid"]);
    p.git(&["config", "user.name", "Rim Test"]);
    write(&p.path, "tracked", "A");
    p.git(&["add", "tracked"]);
    assert!(p.git(&["commit", "-qm", "A"]).status.success());
    let a_commit = String::from_utf8(p.git(&["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let a = run_json_git(&p, &["init", "--json"]);
    let a_node = data(&a)["node"]["id"].as_str().unwrap().to_owned();

    write(&p.path, "tracked", "B");
    p.git(&["add", "tracked"]);
    assert!(p.git(&["commit", "-qm", "B"]).status.success());
    let b_commit = String::from_utf8(p.git(&["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let b = run_json_git(&p, &["init", "--json"]);
    let b_node = data(&b)["node"]["id"].as_str().unwrap().to_owned();
    assert_ne!(a_node, b_node);

    assert!(p.git(&["checkout", "-q", &a_commit]).status.success());
    let refused = run_git(&p, &["switch", &b_node, "--json"]);
    assert_eq!(refused.status.code(), Some(5));
    let switched = run_git(&p, &["switch", &b_node, "--yes"]);
    assert!(
        switched.status.success(),
        "{}",
        String::from_utf8_lossy(&switched.stderr)
    );
    assert_eq!(fs::read_to_string(p.path.join("tracked")).unwrap(), "B");
    let head = String::from_utf8(p.git(&["rev-parse", "HEAD"]).stdout).unwrap();
    assert_eq!(head.trim(), b_commit);
}

#[test]
fn unmanaged_collision_and_active_subtree_are_refused() {
    let p = Project::new("collision");
    write(&p.path, "managed", "base");
    write(&p.path, "collision", "stored");
    let init = p.json(&["init", "--json"]);
    let parent = data(&init)["node"]["id"].as_str().unwrap().to_owned();
    fs::remove_file(p.path.join("collision")).unwrap();
    write(&p.path, ".rimignore", "collision\n");
    let child = p.json(&["spawn", "--json"]);
    let child_id = data(&child)["nodes"][0]["id"].as_str().unwrap().to_owned();
    // The ignored path is not hot, but it would be overwritten by the parent snapshot.
    write(&p.path, "collision", "unmanaged");
    let collision = p.rim(&["switch", &parent, "--json"]);
    assert_eq!(collision.status.code(), Some(4));
    assert_eq!(
        fs::read_to_string(p.path.join("collision")).unwrap(),
        "unmanaged"
    );
    let purge = p.rim(&["purge", &child_id, "--yes", "--json"]);
    assert_eq!(purge.status.code(), Some(4));
}

#[test]
fn interrupted_switch_is_rolled_back_on_next_command() {
    let p = Project::new("recovery");
    write(&p.path, "file", "old");
    let initialized = p.json(&["init", "--json"]);
    let root_id = data(&initialized)["root"]["id"].as_str().unwrap();
    let parent_id = data(&initialized)["node"]["id"].as_str().unwrap();
    let parent_snapshot = data(&initialized)["node"]["snapshot"].as_str().unwrap();
    write(&p.path, "file", "active");
    let spawned = p.json(&["spawn", "--json"]);
    let child = &data(&spawned)["nodes"][0];
    let child_id = child["id"].as_str().unwrap();
    let child_snapshot = child["snapshot"].as_str().unwrap();

    // Simulate interruption after target files were written but before metadata activation.
    write(&p.path, "file", "old");
    let manifest = serde_json::json!({
        "directory":"simulated-interruption",
        "old_tree":child_snapshot,
        "new_tree":parent_snapshot,
        "root_id":root_id,
        "node_id":parent_id
    });
    let db = rusqlite::Connection::open(p.path.join(".rim/state.db")).unwrap();
    db.execute(
        "INSERT OR REPLACE INTO operation(id,kind,manifest) VALUES(1,'switch',?)",
        [manifest.to_string()],
    )
    .unwrap();
    drop(db);

    let recovered = p.rim(&["tree"]);
    assert!(recovered.status.success());
    assert_eq!(fs::read_to_string(p.path.join("file")).unwrap(), "active");
    let tree = p.json(&["tree", "--json"]);
    let active = data(&tree)["roots"][0]["active_node"].as_str().unwrap();
    assert_eq!(active, child_id);
}

#[test]
fn committed_gitlink_is_rejected_as_a_submodule() {
    let p = Project::new("submodule");
    assert!(p.git(&["init", "-q"]).status.success());
    p.git(&["config", "user.email", "rim@example.invalid"]);
    p.git(&["config", "user.name", "Rim Test"]);
    write(&p.path, "file", "base");
    p.git(&["add", "file"]);
    assert!(p.git(&["commit", "-qm", "base"]).status.success());
    let head = String::from_utf8(p.git(&["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert!(p
        .git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},vendor")
        ])
        .status
        .success());
    assert!(p.git(&["commit", "-qm", "gitlink"]).status.success());
    let init = run_git(&p, &["init", "--json"]);
    assert_eq!(init.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&init.stdout).contains("submodule"));
}

#[cfg(unix)]
#[test]
fn symbolic_links_and_executable_modes_survive_switching() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let p = Project::new("unix-modes");
    write(&p.path, "script", "#!/bin/sh\n");
    let mut permissions = fs::metadata(p.path.join("script")).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(p.path.join("script"), permissions).unwrap();
    symlink("script", p.path.join("link")).unwrap();
    let init = p.json(&["init", "--json"]);
    let parent = data(&init)["node"]["id"].as_str().unwrap().to_owned();
    fs::remove_file(p.path.join("script")).unwrap();
    fs::remove_file(p.path.join("link")).unwrap();
    p.json(&["spawn", "--json"]);
    assert!(p.rim(&["switch", &parent]).status.success());
    assert_ne!(
        fs::metadata(p.path.join("script"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        fs::read_link(p.path.join("link")).unwrap(),
        PathBuf::from("script")
    );
}

#[cfg(unix)]
#[test]
fn unsupported_socket_is_rejected() {
    use std::os::unix::net::UnixListener;
    let p = Project::new("socket");
    let _listener = UnixListener::bind(p.path.join("service.sock")).unwrap();
    let init = p.rim(&["init", "--json"]);
    assert_eq!(init.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&init.stdout).contains("special file"));
}

fn run_git(p: &Project, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rim"))
        .args(args)
        .current_dir(&p.path)
        .output()
        .unwrap()
}
fn run_json_git(p: &Project, args: &[&str]) -> Value {
    let out = run_git(p, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
