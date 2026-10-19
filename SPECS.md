# SPECS.md — Rim Technical Specification

This document records technical implementation decisions for Rim. Product
behavior and user-facing semantics belong in [`PRODUCT.md`](PRODUCT.md).

## 1. Confirmed Decisions

### Implementation Language

Rim will use Rust.

The implementation will produce a native CLI executable for Windows, macOS,
and Linux.

### Storage Ownership

Rim will implement and manage its own storage engine. It will not call Git to
create, read, compact, or remove Rim storage objects.

Rim will store its data under `.rim/`. The Rim object store will remain
independent from the enclosing Git object database.

The storage model will use these Git-style principles:

- immutable content-addressed objects;
- complete logical directory trees;
- structural sharing between snapshots;
- mutable Rim node records that point to immutable snapshots;
- reachability-based cleanup.

Rim will not copy Git source code. The implementation can study Git's public
documentation, storage concepts, and observable behavior.

### Dependency Policy

Rim will prefer the Rust standard library and a small set of focused crates.
A dependency must provide clear value in correctness, portability, security, or
maintenance cost.

Before adoption, each production dependency must have:

- a compatible license;
- an active maintenance history;
- a limited and reviewed dependency tree;
- a clear purpose that Rim should not implement itself.

Rim will not adopt a large framework when a small crate or direct
implementation is sufficient.

### Runtime Dependencies

The initial implementation will use these focused crates:

| Crate | Purpose |
|---|---|
| `clap` | Parse CLI commands, options, and help. |
| `serde` | Serialize and deserialize typed data. |
| `serde_json` | Produce JSON command output. |
| `blake3` | Create content-addressed object IDs. |
| `zstd` | Compress stored objects. |
| `rusqlite` | Store roots, nodes, active pointers, and operation metadata. |
| `ignore` | Walk project files and apply ignore rules. |
| `fs4` | Lock a Rim repository during mutations. |

Rim will use SQLite only for mutable metadata. Project file contents and tree
objects will remain in the Rim-owned content-addressed object store.

The initial private layout is:

```text
.rim/
├── format
├── .gitignore
├── state.db
├── lock
├── objects/
├── transactions/
└── tmp/
```

The internal `.gitignore` excludes all `.rim/` contents from normal Git
tracking. Rim also rejects initialization if the enclosing Git repository
already tracks `.rim/`.

Rim writes immutable objects before a SQLite transaction points to them. A
crash can therefore leave an unreachable object, but it cannot leave metadata
that refers to an incomplete object. Later garbage collection can remove
unreachable objects.

Rim will not initially use an async runtime, ORM, Git library, terminal UI
framework, logging framework, or plugin framework.

### Git Integration

The Git adapter can invoke the installed Git executable to inspect or change
the enclosing Git context. It must not use Git to manage Rim objects.
Standalone mode must not require Git.

### Snapshot Boundaries

Rim will always exclude `.rim/` and `.git/` from snapshots. In a Git project,
Rim will apply `.gitignore` rules. A `.rimignore` file can add Rim-specific
exclusions.

A Git root snapshot includes the files in its origin commit even if a later
ignore rule matches them. New ignored files do not enter Rim snapshots. Rim
will store symbolic-link targets without following them and will preserve the
executable-file bit.

The MVP will reject Git submodules, sockets, devices, and other unsupported
special files.

### Safe Switching

A switch within the current Git root will not change Git `HEAD` or the Git
index. For a cross-root switch, Rim asks for permission and invokes Git without
force, stash, or reset behavior. Rim does not reject the operation because of
staged changes. Git owns its safety checks and index behavior.

Rim runs the Git operation before it changes the active Rim node. If Git fails,
Rim reports the error and stops. If Git succeeds, Rim materializes the target
snapshot without staging it. If later materialization fails, Rim keeps its old
active-node metadata and reports the resulting context mismatch on the next
command.

Before file materialization, Rim will check for hot changes and unmanaged path
collisions. A mutating switch will record recovery metadata and back up each
path that it replaces or removes. A failed or interrupted same-root switch must
recover to a complete old or new state.

### Machine Interface

JSON output will use a versioned result envelope, structured errors, and stable
exit-code categories. JSON and noninteractive commands must not wait for an
interactive prompt. A confirmation-required command must fail clearly unless
the caller supplies an explicit confirmation option.

### License

Rim uses the MIT License. Distributed copies or substantial portions must keep
the copyright and license notice in [`LICENSE`](LICENSE).

## 2. Decisions Still Open

The following choices are not final:

- the canonical object encoding;
- the test and fuzzing libraries;
- the initial loose-object and pack strategy.

These choices will be added only after their requirements and tradeoffs are
clear.

## 3. Initial Architecture Boundary

The implementation will keep these responsibilities separate:

```text
CLI
    parses commands and renders results

Rim core
    applies node and tree rules

Rim store
    owns objects, snapshots, integrity, and cleanup

Project filesystem
    scans and materializes project states

Git adapter
    observes and changes Git context with user permission
```

The Rim store must not depend on the Git adapter. Standalone projects and Git
projects must use the same Rim snapshot semantics.
