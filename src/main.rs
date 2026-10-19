mod core;
mod error;
mod fs;
mod git;
mod meta;
mod object;
use clap::{Parser, Subcommand};
use core::Repo;
use error::{Category, Error, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

#[derive(Parser)]
#[command(
    name = "rim",
    version,
    about = "Temporary solution trees for AI-assisted development",
    long_about = "Temporary solution trees for AI-assisted development.\n\nRim stores editable candidate solutions between your working files and the next Git commit. It does not create Git branches or commits. The active node's saved state plus hot workspace changes is the state visible in your project.",
    after_long_help = "HOW CHANGES ARE SAVED:\n  rim punch    Update the active node in place\n  rim spawn    Capture changes in a new child candidate\n\nTYPICAL WORKFLOW:\n  rim spawn --tag approach-a\n  # edit files\n  rim punch -m \"working implementation\"\n  rim spawn --tag variant --fan-out 3\n  rim tree\n  rim switch <node-id>\n  rim prune\n  git add . && git commit\n\nRun 'rim help <command>' for command-specific details."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Emit a versioned JSON result envelope; never prompt"
    )]
    json: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[command(
        about = "Create the baseline node",
        long_about = "Create the baseline node without creating a child candidate.\n\nInitialization is optional because the first 'rim spawn' initializes Rim automatically. In Git, the baseline is anchored to the current commit. Outside Git, it captures the current project state.",
        after_long_help = "EXAMPLES:\n  rim init\n  rim init --new    # start another standalone root"
    )]
    Init {
        #[arg(
            long,
            help = "Start another root from the current state (standalone mode only)"
        )]
        new: bool,
    },
    #[command(
        about = "Create child candidates from the active node",
        long_about = "Create one or more child candidates from the active node.\n\nHot workspace changes are captured in each child while the parent keeps its saved state. One child becomes active immediately. With --fan-out greater than one, Rim creates siblings and leaves the parent active. If necessary, Rim creates the baseline first.",
        after_long_help = "EXAMPLES:\n  rim spawn --tag first-approach\n  rim spawn --tag variant --fan-out 3"
    )]
    Spawn {
        #[arg(
            long,
            help = "Label the new candidate; fan-out labels receive numeric suffixes"
        )]
        tag: Option<String>,
        #[arg(
            long,
            default_value_t = 1,
            value_name = "COUNT",
            help = "Create COUNT sibling candidates"
        )]
        fan_out: u32,
    },
    #[command(
        about = "Save hot changes into the active node",
        long_about = "Save hot workspace changes into the active node.\n\nPunch updates the existing node. It does not create a child, a Git commit, or a Git staging entry."
    )]
    Punch {
        #[arg(short = 'm', long, help = "Describe this update")]
        message: Option<String>,
    },
    #[command(
        about = "Make another node active in the project directory",
        long_about = "Make another Rim node active and materialize its files in the project directory.\n\nSwitch refuses to discard hot changes; save them first with 'rim punch' or 'rim spawn'. Switching to a root based on another Git commit requires permission to ask Git to change HEAD."
    )]
    Switch {
        #[arg(help = "Node to make active")]
        node_id: String,
        #[arg(long, help = "Allow a required Git HEAD change without prompting")]
        yes: bool,
    },
    #[command(
        about = "Delete an inactive node and its complete subtree",
        long_about = "Delete an inactive node and all of its descendants.\n\nRim refuses the operation if the subtree contains the active node. Switch outside that subtree first.",
        after_long_help = "EXAMPLE:\n  rim purge <rejected-node-id> --yes"
    )]
    Purge {
        #[arg(help = "Root node of the subtree to delete")]
        node_id: String,
        #[arg(long, help = "Confirm deletion without prompting")]
        yes: bool,
    },
    #[command(
        about = "Keep only the active node and remove other candidates",
        long_about = "Keep only the active node and remove every other node in the current root, including its ancestors, siblings, and descendants.\n\nWith --all, also remove every other root in the repository scope. This operation requires confirmation.",
        after_long_help = "EXAMPLES:\n  rim prune --yes\n  rim prune --all --yes"
    )]
    Prune {
        #[arg(long, help = "Also remove every other root in the repository scope")]
        all: bool,
        #[arg(long, help = "Confirm deletion without prompting")]
        yes: bool,
    },
    #[command(
        about = "Show roots, candidate nodes, and the active node",
        long_about = "Show all Rim roots and candidate nodes, including the current root and its active node. This command does not modify the tree."
    )]
    Tree,
    #[command(
        about = "Show workspace changes relative to a node",
        long_about = "Compare the current project files with a node's saved state. Without a node ID, use the active node. This is Rim status, independent of the Git index and Git status.",
        after_long_help = "EXAMPLES:\n  rim status\n  rim status <node-id>"
    )]
    Status {
        #[arg(help = "Saved node to compare with the current project files")]
        node_id: Option<String>,
    },
    #[command(
        about = "Compare a node with its parent or another node",
        long_about = "Compare saved Rim node states.\n\nWith one node, compare it with the parent state from which it was spawned. With two nodes, show the first node's changes relative to the second. Optional paths restrict the output.",
        after_long_help = "EXAMPLES:\n  rim diff <node-id>\n  rim diff <node-id> <other-node-id>\n  rim diff <node-id> -- src tests\n  rim diff <node-id> <other-node-id> -- src tests"
    )]
    Diff {
        #[arg(help = "Node whose changes are shown")]
        node_id: String,
        #[arg(help = "Node to compare against; a non-node value is treated as a path")]
        other_node_id: Option<String>,
        #[arg(num_args = 0.., help = "Paths to include in the comparison")]
        paths: Vec<PathBuf>,
    },
}
#[derive(Serialize)]
struct Envelope<T: Serialize> {
    version: u32,
    ok: bool,
    data: T,
}
fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let wants_json = args.iter().any(|x| x == "--json");
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            if wants_json {
                print_error(&Error::new(Category::Usage, e.to_string()), true);
                std::process::exit(2)
            }
            e.exit()
        }
    };
    match run(&cli) {
        Ok(data) => {
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string(&Envelope {
                        version: 1,
                        ok: true,
                        data
                    })
                    .unwrap()
                )
            }
        }
        Err(e) => {
            let code = e.code();
            print_error(&e, cli.json);
            std::process::exit(code)
        }
    }
}
fn run(cli: &Cli) -> Result<Value> {
    let cwd = std::env::current_dir()?;
    match &cli.command {
        Command::Init { new } => {
            let mut r = Repo::discover(&cwd, true)?;
            r.lock()?;
            let (root, node, created) = r.initialize(*new)?;
            success(
                cli,
                json!({"created":created,"root":root,"node":node}),
                if created {
                    format!("initialized root {} with base node {}", root.id, node.id)
                } else {
                    format!(
                        "Rim is already initialized (root {}, active node {})",
                        root.id, node.id
                    )
                },
            )
        }
        Command::Spawn { tag, fan_out } => {
            let mut r = Repo::discover(&cwd, true)?;
            r.lock()?;
            let nodes = r.spawn(tag.clone(), *fan_out)?;
            let text = if nodes.len() == 1 {
                format!("spawned and activated node {}", nodes[0].id)
            } else {
                format!(
                    "spawned {} sibling nodes; parent remains active\n{}",
                    nodes.len(),
                    nodes
                        .iter()
                        .map(|n| format!(
                            "  {}{}",
                            n.id,
                            n.tag
                                .as_ref()
                                .map(|t| format!(" [{t}]"))
                                .unwrap_or_default()
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            success(
                cli,
                json!({"nodes":nodes,"parent_remains_active":nodes.len()>1}),
                text,
            )
        }
        Command::Punch { message } => {
            let mut r = Repo::discover(&cwd, false)?;
            r.lock()?;
            let (node, changed) = r.punch(message.as_deref())?;
            success(
                cli,
                json!({"node":node,"changed":changed}),
                if changed {
                    format!("punched changes into node {}", node.id)
                } else {
                    format!("node {} has no hot changes", node.id)
                },
            )
        }
        Command::Switch { node_id, yes } => {
            let mut r = Repo::discover(&cwd, false)?;
            r.lock()?;
            let target = r.node(node_id)?;
            let current = r.require_current()?;
            let target_root = r
                .meta
                .root(&target.root_id)?
                .ok_or_else(|| Error::state("target node has no root"))?;
            let cross_git = target.root_id != current.id && target_root.origin_commit.is_some();
            let permit = if cross_git {
                confirm(
                    *yes,
                    cli.json,
                    "Target has a different Git origin. Allow Rim to ask Git to change HEAD?",
                )?
            } else {
                *yes
            };
            let n = r.switch(node_id, permit)?;
            success(cli, json!({"node":n}), format!("switched to node {}", n.id))
        }
        Command::Purge { node_id, yes } => {
            let mut r = Repo::discover(&cwd, false)?;
            r.lock()?;
            confirm(
                *yes,
                cli.json,
                &format!("Purge node {node_id} and all descendants?"),
            )?;
            let count = r.purge(node_id)?;
            success(
                cli,
                json!({"removed_nodes":count}),
                format!("purged {count} node(s)"),
            )
        }
        Command::Prune { all, yes } => {
            let mut r = Repo::discover(&cwd, false)?;
            r.lock()?;
            let current = r.require_current()?;
            let roots = r.meta.roots()?;
            let nodes = r.meta.nodes()?;
            let (planned_nodes, planned_roots) = if *all {
                (nodes.len().saturating_sub(1), roots.len().saturating_sub(1))
            } else {
                (
                    nodes
                        .iter()
                        .filter(|n| n.root_id == current.id && n.id != current.active_node)
                        .count(),
                    0,
                )
            };
            confirm(*yes,cli.json,&format!("Remove {planned_nodes} node(s) and {planned_roots} root(s), keeping only the active node{}?",if *all{" across the repository"}else{" in this root"}))?;
            let (n, roots) = r.prune(*all)?;
            success(
                cli,
                json!({"removed_nodes":n,"removed_roots":roots,"all":all}),
                format!("pruned {n} node(s) and {roots} root(s)"),
            )
        }
        Command::Tree => {
            let mut r = Repo::discover(&cwd, false)?;
            r.lock()?;
            let (current, roots, nodes) = r.tree()?;
            if !cli.json {
                render_tree(current.as_deref(), &roots, &nodes)
            }
            Ok(json!({"current_root":current,"roots":roots,"nodes":nodes}))
        }
        Command::Status { node_id } => {
            let mut r = Repo::discover(&cwd, false)?;
            r.lock()?;
            let (node, changes) = r.status(node_id.as_deref())?;
            let hot = !changes.is_empty();
            if !cli.json {
                println!(
                    "node {}: {}",
                    node.id,
                    if hot { "hot changes" } else { "clean" }
                );
                for c in &changes {
                    println!("{}\t{}", mark(&c.status), c.path)
                }
            }
            Ok(json!({"node":node,"hot":hot,"changes":changes}))
        }
        Command::Diff {
            node_id,
            other_node_id,
            paths,
        } => {
            let mut r = Repo::discover(&cwd, false)?;
            r.lock()?;
            let mut effective_paths = paths.clone();
            let effective_other = if let Some(candidate) = other_node_id {
                if r.meta.node(candidate)?.is_some() {
                    Some(candidate.as_str())
                } else {
                    // Permit `rim diff NODE path` despite the positional ambiguity:
                    // a value that is not an existing node is a path filter.
                    effective_paths.insert(0, PathBuf::from(candidate));
                    None
                }
            } else {
                None
            };
            let changes = r.diff(node_id, effective_other, &effective_paths)?;
            if !cli.json {
                if changes.is_empty() {
                    println!("no differences")
                } else {
                    for c in &changes {
                        println!("{}\t{}", mark(&c.status), c.path)
                    }
                }
            }
            Ok(
                json!({"from":effective_other,"to":node_id,"paths":effective_paths.iter().map(|p|p.to_string_lossy()).collect::<Vec<_>>(),"changes":changes}),
            )
        }
    }
}
fn success(cli: &Cli, data: Value, text: String) -> Result<Value> {
    if !cli.json {
        println!("{text}")
    }
    Ok(data)
}
fn confirm(yes: bool, json_mode: bool, prompt: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if json_mode || !io::stdin().is_terminal() {
        return Err(Error::new(
            Category::Confirmation,
            "confirmation required; rerun with --yes",
        ));
    }
    eprint!("{prompt} [y/N] ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(true)
    } else {
        Err(Error::new(Category::Confirmation, "operation cancelled"))
    }
}
fn print_error(e: &Error, json_mode: bool) {
    if json_mode {
        println!(
            "{}",
            json!({"version":1,"ok":false,"error":{"category":e.category,"message":e.message,"exit_code":e.code()}})
        )
    } else {
        eprintln!("rim: {}", e.message)
    }
}
fn mark(s: &str) -> &str {
    match s {
        "added" => "A",
        "deleted" => "D",
        _ => "M",
    }
}
fn render_tree(current: Option<&str>, roots: &[meta::Root], nodes: &[meta::Node]) {
    for root in roots {
        println!(
            "{}root {} ({}){}",
            if current == Some(root.id.as_str()) {
                "> "
            } else {
                "  "
            },
            root.id,
            root.origin_commit.as_deref().unwrap_or("standalone"),
            if current == Some(root.id.as_str()) {
                " [current]"
            } else {
                ""
            }
        );
        let by_parent: BTreeMap<Option<&str>, Vec<&meta::Node>> = {
            let mut m: BTreeMap<Option<&str>, Vec<&meta::Node>> = BTreeMap::new();
            for n in nodes.iter().filter(|n| n.root_id == root.id) {
                m.entry(n.parent_id.as_deref()).or_default().push(n)
            }
            m
        };
        fn rec(
            parent: Option<&str>,
            prefix: &str,
            map: &BTreeMap<Option<&str>, Vec<&meta::Node>>,
            active: &str,
        ) {
            if let Some(list) = map.get(&parent) {
                for n in list {
                    println!(
                        "{prefix}{} {}{}{}",
                        if n.id == active { "*" } else { "-" },
                        n.id,
                        n.tag
                            .as_ref()
                            .map(|t| format!(" [{t}]"))
                            .unwrap_or_default(),
                        if n.id == active { " [active]" } else { "" }
                    );
                    rec(Some(&n.id), &format!("{prefix}  "), map, active)
                }
            }
        }
        rec(None, "    ", &by_parent, &root.active_node)
    }
}
