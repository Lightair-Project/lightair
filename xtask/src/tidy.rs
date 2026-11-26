//! `cargo xtask tidy [--dry-run]`: frees space in `target/` without breaking current work.
//!
//! The same file is in lightair, lightair-live and lightair-editor; keep them identical.
//!
//! Cargo never deletes the hashed copies it leaves behind (linked and pinned lightair-core,
//! Cargo.lock updates, feature variants, check/clippy/test builds), and incremental caches grow
//! without bound. This removes every `incremental` directory and, in each profile directory
//! (`target/<profile>` and `target/<triple>/<profile>`), all but the newest [`KEEP`] copies of
//! each crate's libraries and of each binary, with their `.fingerprint` and `build` entries.
//! A copy that was still in use only costs a rebuild of that crate. Only `target/` is touched.
//!
//! On macOS it also deletes the loose object files no program refers to ([`prune_objects`]),
//! which `cargo xtask prune` does alone.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Copies kept per crate and kind. A dev build, a check/clippy run and a test build (or a
/// second feature set) each leave one, so three keeps everything a normal day uses.
const KEEP: usize = 3;

const LIB_EXTS: [&str; 6] = ["rlib", "rmeta", "a", "dylib", "so", "dll"];

type Result<T = ()> = std::result::Result<T, String>;

/// A hashed copy of a crate's artifacts: hash, newest mtime, files.
type Copy = (String, SystemTime, Vec<PathBuf>);

/// One thing to delete, with what it frees.
struct Item {
    label: String,
    paths: Vec<PathBuf>,
}

pub fn tidy(root: &Path, dry_run: bool) -> Result {
    let target = root.join("target");
    if !target.is_dir() {
        println!("tidy: no target/ directory, nothing to do");
        return Ok(());
    }
    let busy = locked(&target);
    if busy && !dry_run {
        return Err("a cargo build holds the target directory's lock; run tidy when it finishes".into());
    }
    if busy {
        println!("(a cargo build is running; a real run would refuse)");
    }

    // dir -> display name; the host's profile directories are grouped as "host".
    let mut groups: Vec<(String, Vec<PathBuf>)> = Vec::new();
    for entry in subdirs(&target) {
        let name = file_name(&entry);
        if entry.join("deps").is_dir() {
            add(&mut groups, "host", entry);
        } else {
            for profile in subdirs(&entry) {
                if profile.join("deps").is_dir() {
                    add(&mut groups, &name, profile);
                }
            }
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));

    let mut seen = HashSet::new(); // (dev, inode): hard links count once
    let verb = if dry_run { "would free" } else { "freed" };
    let mut total = 0u64;
    for (triple, profiles) in &groups {
        let mut inc = Vec::new();
        let mut stale = Vec::new();
        for p in profiles {
            let i = p.join("incremental");
            if i.is_dir() {
                inc.push(Item { label: rel(&target, &i), paths: vec![i] });
            }
            stale.extend(stale_in(p, &target));
        }
        let (mut inc_bytes, mut stale_bytes) = (0, 0);
        for item in inc.iter().chain(&stale) {
            let bytes: u64 = item.paths.iter().map(|p| size(p, &mut seen)).sum();
            if dry_run {
                println!("  {:>10}  {}", human(bytes), item.label);
            }
            if inc.iter().any(|i| std::ptr::eq(i, item)) {
                inc_bytes += bytes;
            } else {
                stale_bytes += bytes;
            }
            if !dry_run {
                for p in &item.paths {
                    remove(p, &target)?;
                }
            }
        }
        let (mut objects, mut object_bytes) = (0, 0);
        for p in profiles {
            let (n, b) = prune_objects(&p.join("deps"), dry_run);
            objects += n;
            object_bytes += b;
        }
        total += inc_bytes + stale_bytes + object_bytes;
        println!(
            "{triple}: {verb} {} (incremental {}, {} stale copies {}, {objects} unused object files {})",
            human(inc_bytes + stale_bytes + object_bytes),
            human(inc_bytes),
            stale.len(),
            human(stale_bytes),
            human(object_bytes)
        );
    }
    println!("total {verb}: {}", human(total));
    Ok(())
}

/// `cargo xtask prune`: [`prune_objects`] in every profile directory of `target/`. Unlike
/// `tidy`, it never costs a rebuild.
pub fn prune(root: &Path) -> Result {
    let target = root.join("target");
    let mut dirs = Vec::new();
    for entry in subdirs(&target) {
        if entry.join("deps").is_dir() {
            dirs.push(entry.join("deps"));
        } else {
            dirs.extend(subdirs(&entry).into_iter().map(|p| p.join("deps")).filter(|d| d.is_dir()));
        }
    }
    let (mut files, mut bytes) = (0, 0);
    for d in dirs {
        let (n, b) = prune_objects(&d, false);
        if n > 0 {
            println!("  {:>10}  {n} object files in {}", human(b), rel(&target, &d));
        }
        files += n;
        bytes += b;
    }
    // APFS clones share blocks, so the disk gains less than the files list
    println!("prune: deleted {files} unused object files (listed size {})", human(bytes));
    Ok(())
}

/// Files directly in a `deps` folder; prune first when there are more (see [`prune_objects`]).
pub const CROWDED: usize = 50_000;

/// The number of entries in `target/debug/deps`.
pub fn deps_files(root: &Path) -> usize {
    fs::read_dir(root.join("target/debug/deps")).map_or(0, |d| d.count())
}

/// Deletes the object files in one `deps` folder that no program or dynamic library there
/// refers to. Returns (files, bytes).
///
/// On Apple targets debug info stays in the object files (`split-debuginfo = "unpacked"`), so
/// rustc keeps every program's objects beside it, and each incremental build writes them under
/// new names (`name-hash.<unit>.<build>.rcgu.o`) without deleting the old ones. rustc lists the
/// whole folder each time it starts: 650,000 of them made every rustc start take 4–34 s. A
/// program's debug map (`nm -ap`, its OSO entries) names the objects it still uses; static
/// libraries carry their objects inside, so none of theirs are needed.
pub fn prune_objects(deps: &Path, dry_run: bool) -> (usize, u64) {
    if !cfg!(target_os = "macos") {
        return (0, 0);
    }
    let mut objects = Vec::new();
    let mut programs = Vec::new();
    for e in read(deps) {
        let name = file_name(&e);
        if name.ends_with(".o") {
            objects.push(e);
        } else if name.ends_with(".dylib")
            || (!name.contains('.') && name.rsplit_once('-').is_some_and(|(_, h)| h.len() == 16))
        {
            programs.push(e);
        }
    }
    if objects.is_empty() {
        return (0, 0);
    }
    let used: HashSet<String> = std::thread::scope(|s| {
        let jobs: Vec<_> = programs
            .chunks(programs.len().div_ceil(8).max(1))
            .map(|chunk| {
                s.spawn(move || {
                    let mut names = Vec::new();
                    for p in chunk {
                        let Ok(out) = Command::new("nm").arg("-ap").arg(p).output() else { continue };
                        for line in String::from_utf8_lossy(&out.stdout).lines() {
                            if let Some((_, path)) = line.split_once(" OSO ") {
                                names.push(path.rsplit('/').next().unwrap_or(path).to_string());
                            }
                        }
                    }
                    names
                })
            })
            .collect();
        jobs.into_iter().flat_map(|j| j.join().unwrap_or_default()).collect()
    });
    let (mut files, mut bytes) = (0, 0);
    for o in objects {
        if used.contains(&file_name(&o)) {
            continue;
        }
        let size = fs::symlink_metadata(&o).map_or(0, |m| disk(&m).0);
        if dry_run || fs::remove_file(&o).is_ok() {
            files += 1;
            bytes += size;
        }
    }
    (files, bytes)
}

fn add(groups: &mut Vec<(String, Vec<PathBuf>)>, name: &str, dir: PathBuf) {
    match groups.iter_mut().find(|g| g.0 == name) {
        Some(g) => g.1.push(dir),
        None => groups.push((name.to_string(), vec![dir])),
    }
}

/// True when something holds cargo's lock in any profile directory (cargo takes it exclusively).
fn locked(target: &Path) -> bool {
    let mut dirs = vec![target.to_path_buf()];
    for d in subdirs(target) {
        dirs.push(d.clone());
        dirs.extend(subdirs(&d));
    }
    dirs.iter().any(|d| {
        let lock = d.join(".cargo-lock");
        match File::options().read(true).write(true).open(&lock) {
            Ok(f) => matches!(f.try_lock(), Err(TryLockError::WouldBlock)),
            Err(_) => false,
        }
    })
}

/// The stale copies in one profile directory: units in `deps`, then orphaned `.fingerprint`
/// and `build` entries.
fn stale_in(profile: &Path, target: &Path) -> Vec<Item> {
    struct File_ {
        path: PathBuf,
        ext: String,
        mtime: SystemTime,
    }
    // (stem, hash) -> files
    let mut units: HashMap<(String, String), Vec<File_>> = HashMap::new();
    for e in read(&profile.join("deps")) {
        let name = file_name(&e);
        let (base, ext) = match name.rsplit_once('.') {
            Some((b, x)) => (b.to_string(), x.to_string()),
            None => (name.clone(), String::new()),
        };
        let Some((stem, hash)) = base.rsplit_once('-') else { continue };
        if hash.len() != 16 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let mtime = fs::symlink_metadata(&e).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        units.entry((stem.to_string(), hash.to_string())).or_default().push(File_ { path: e, ext, mtime });
    }

    // Group units by (stem, kind): a unit with a library file is a lib, the rest are binaries.
    let mut grouped: HashMap<(String, bool), Vec<Copy>> = HashMap::new();
    for ((stem, hash), files) in units {
        let lib = files.iter().any(|f| LIB_EXTS.contains(&f.ext.as_str()));
        let mtime = files.iter().map(|f| f.mtime).max().unwrap_or(SystemTime::UNIX_EPOCH);
        let paths = files.into_iter().map(|f| f.path).collect();
        grouped.entry((stem, lib)).or_default().push((hash, mtime, paths));
    }

    let mut items = Vec::new();
    let mut dead_hashes = HashSet::new();
    let mut live_hashes = HashSet::new();
    for ((stem, _), mut copies) in grouped {
        copies.sort_by_key(|c| std::cmp::Reverse(c.1));
        for (i, (hash, _, paths)) in copies.into_iter().enumerate() {
            if i < KEEP {
                live_hashes.insert(hash);
            } else {
                dead_hashes.insert(hash.clone());
                items.push(Item { label: format!("{}/{stem}-{hash}", rel(target, profile)), paths });
            }
        }
    }
    // Their fingerprints and build directories (named <package>-<hash>, hyphens not underscores).
    for sub in [".fingerprint", "build"] {
        for e in read(&profile.join(sub)) {
            let name = file_name(&e);
            let Some((_, hash)) = name.rsplit_once('-') else { continue };
            if dead_hashes.contains(hash) && !live_hashes.contains(hash) {
                items.push(Item { label: rel(target, &e), paths: vec![e] });
            }
        }
    }
    // Entries no deps file points at (build-script runs, removed crates): per package, keep the
    // newest 2 * KEEP (a build script leaves a compile and a run directory per variant).
    for sub in [".fingerprint", "build"] {
        let mut by_pkg: HashMap<String, Vec<(SystemTime, PathBuf)>> = HashMap::new();
        for e in read(&profile.join(sub)) {
            let name = file_name(&e);
            let Some((pkg, hash)) = name.rsplit_once('-') else { continue };
            if dead_hashes.contains(hash) || live_hashes.contains(hash) {
                continue;
            }
            by_pkg.entry(pkg.to_string()).or_default().push((newest_inside(&e), e));
        }
        for (_, mut v) in by_pkg {
            v.sort_by_key(|c| std::cmp::Reverse(c.0));
            for (_, e) in v.into_iter().skip(2 * KEEP) {
                items.push(Item { label: rel(target, &e), paths: vec![e] });
            }
        }
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
}

/// The newest mtime of a directory and the files directly in it (cargo rewrites files in place,
/// which doesn't touch the directory's own time).
fn newest_inside(dir: &Path) -> SystemTime {
    let m = |p: &Path| fs::symlink_metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    read(dir).iter().map(|p| m(p)).chain([m(dir)]).max().unwrap_or(SystemTime::UNIX_EPOCH)
}

fn read(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

/// Child directories, symlinks not followed.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    read(dir).into_iter().filter(|p| fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false)).collect()
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn rel(target: &Path, p: &Path) -> String {
    p.strip_prefix(target).unwrap_or(p).display().to_string()
}

/// The disk a file takes, and (on Unix) its device and inode, so hard links count once.
#[cfg(unix)]
fn disk(m: &fs::Metadata) -> (u64, Option<(u64, u64)>) {
    use std::os::unix::fs::MetadataExt;
    (m.blocks() * 512, Some((m.dev(), m.ino())))
}

/// The disk a file takes: its length (no inodes to tell hard links apart).
#[cfg(not(unix))]
fn disk(m: &fs::Metadata) -> (u64, Option<(u64, u64)>) {
    (m.len(), None)
}

/// Disk used by a file or tree, counting each hard-linked inode once per run.
fn size(p: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let Ok(m) = fs::symlink_metadata(p) else { return 0 };
    if m.is_dir() {
        return read(p).iter().map(|c| size(c, seen)).sum();
    }
    match disk(&m) {
        (_, Some(id)) if !seen.insert(id) => 0,
        (bytes, _) => bytes,
    }
}

fn remove(p: &Path, target: &Path) -> Result {
    if !p.starts_with(target) {
        return Err(format!("refusing to delete {} outside target/", p.display()));
    }
    let m = match fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(_) => return Ok(()), // already gone (a hard link's other name, or a parent removed)
    };
    let r = if m.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) };
    r.map_err(|e| format!("{}: {e}", p.display()))
}

fn human(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}
