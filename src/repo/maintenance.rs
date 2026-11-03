//! Repository upkeep without `git gc`. libgit2 cannot repack, and the project does not
//! shell out to git, so this builds the one pack itself.
//!
//! A run packs *every* object in the repository (loose or already packed, reachable or
//! not) into one new pack, proves the new pack holds them all by reading each object back
//! from it in isolation, and only then deletes the loose objects and the old packs it
//! replaces. Nothing is ever pruned, so a run cannot lose an object: if anything fails
//! before the deletion step, the only change is a stray new pack that is removed again.
//!
//! The work uses its own repository handle, not the shared mutex, so commits never wait
//! for it. Objects written while it runs are not in its snapshot and are left alone.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use git2::{Odb, Oid, Repository};
use tracing::{info, warn};

use super::Repo;
use crate::brand::TMP_DIR_NAME;

/// When to repack. A run happens if either limit is exceeded.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceLimits {
    pub loose_objects: usize,
    pub packs: usize,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MaintenanceReport {
    /// False when the repository was below both limits (or empty).
    pub ran: bool,
    pub objects: usize,
    pub loose_before: usize,
    pub packs_before: usize,
    pub loose_removed: usize,
    pub packs_removed: usize,
}

const PACK_SIDE_FILES: [&str; 5] = [".pack", ".idx", ".rev", ".bitmap", ".mtimes"];

impl Repo {
    /// Repack in this process. Used by tests; the server uses `maintain_isolated`.
    pub fn maintain(&self, limits: &MaintenanceLimits) -> Result<MaintenanceReport> {
        let _only_one = self
            .inner
            .maintenance
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let report = maintain_gitdir(&self.inner.gitdir, limits)?;
        if report.ran {
            self.after_maintenance(&report);
        }
        Ok(report)
    }

    /// Repack in a separate process (this same program, run as `maintain-worker`). Packing
    /// needs memory in proportion to what it packs, so a repository that is large for the
    /// memory limit would otherwise get the whole server killed, over and over. In a child
    /// process, running out of memory fails only the repack; the memory also goes back to
    /// the operating system when the child exits.
    pub fn maintain_isolated(&self, limits: &MaintenanceLimits) -> Result<MaintenanceReport> {
        // DAVLOG_WORKER_EXE names another program to run, which tests use because their
        // own executable is not davlog.
        let exe = match std::env::var_os(format!("{}_WORKER_EXE", crate::brand::ENV_PREFIX)) {
            Some(path) => PathBuf::from(path),
            None => std::env::current_exe().context("find the davlog program to run the repack")?,
        };
        self.maintain_with_worker(&exe, limits)
    }

    pub fn maintain_with_worker(
        &self,
        exe: &Path,
        limits: &MaintenanceLimits,
    ) -> Result<MaintenanceReport> {
        use std::os::unix::process::ExitStatusExt;
        let _only_one = self
            .inner
            .maintenance
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let output = std::process::Command::new(exe)
            .arg("maintain-worker")
            .arg("--git-dir")
            .arg(&self.inner.gitdir)
            .arg("--loose-objects")
            .arg(limits.loose_objects.to_string())
            .arg("--packs")
            .arg(limits.packs.to_string())
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("start {} for the repack", exe.display()))?;
        if !output.status.success() {
            if let Some(signal) = output.status.signal() {
                bail!(
                    "the repack process was killed by signal {signal}; it most likely ran out of memory. \
                     Give the container more memory, or raise [maintenance] loose_object_limit so \
                     fewer objects are packed each time"
                );
            }
            let text = String::from_utf8_lossy(&output.stderr);
            bail!("the repack process failed: {}", text.trim());
        }
        let line = String::from_utf8_lossy(&output.stdout);
        let report: MaintenanceReport = serde_json::from_str(line.trim())
            .with_context(|| format!("read the repack result {:?}", line.trim()))?;
        if report.ran {
            self.after_maintenance(&report);
        }
        Ok(report)
    }

    fn after_maintenance(&self, report: &MaintenanceReport) {
        info!(
            objects = report.objects,
            loose_removed = report.loose_removed,
            packs_removed = report.packs_removed,
            "repacked the repository"
        );
        // The handle that commits use still has the replaced packs open (and their caches)
        // until its object store is refreshed, which it only does when it misses an object.
        if let Ok(shared) = self.lock().odb() {
            let _ = shared.refresh();
        }
        super::release_free_memory();
    }
}

/// Entry point of `davlog maintain-worker`: repack one repository and report the result.
pub fn run_maintenance_worker(
    git_dir: &Path,
    limits: &MaintenanceLimits,
) -> Result<MaintenanceReport> {
    super::isolate_from_user_git_config();
    maintain_gitdir(git_dir, limits)
}

/// The repack itself, on a `.git` directory, with its own repository handle.
///
/// Normally it packs only the loose objects into a new pack, which needs memory in
/// proportion to the loose objects (a few thousand). Only when there are more packs than
/// `limits.packs` does it rewrite everything into one pack, which is the expensive case.
fn maintain_gitdir(gitdir: &Path, limits: &MaintenanceLimits) -> Result<MaintenanceReport> {
    let objects = gitdir.join("objects");
    let pack_dir = objects.join("pack");
    remove_stale_verify_dirs(gitdir);

    // Take the lists of what to delete BEFORE the snapshot of ids: everything in them is
    // then guaranteed to be in the snapshot, while objects and packs that appear later
    // are never touched.
    let loose = loose_objects(&objects)?;
    let old_packs = unkept_packs(&pack_dir)?;
    let mut report = MaintenanceReport {
        loose_before: loose.len(),
        packs_before: old_packs.len(),
        ..MaintenanceReport::default()
    };
    let consolidate = old_packs.len() > limits.packs;
    if loose.len() <= limits.loose_objects && !consolidate {
        return Ok(report);
    }

    let handle = Repository::open(gitdir).context("open the repository for maintenance")?;
    let odb = handle.odb()?;
    let mut ids: Vec<Oid> = Vec::new();
    if consolidate {
        odb.foreach(|id| {
            ids.push(*id);
            true
        })?;
    } else {
        ids.extend(loose.iter().map(|(id, _)| *id));
    }
    if ids.is_empty() {
        return Ok(report);
    }
    report.objects = ids.len();

    let mut builder = handle.packbuilder()?;
    // One thread: the delta search runs in the background, and each extra worker thread
    // gets its own allocator arena that keeps the memory it used.
    builder.set_threads(1);
    if consolidate {
        // Reachable objects first, with their paths, so deltas pair up like files.
        if handle.references()?.next().is_some() {
            let mut walk = handle.revwalk()?;
            walk.push_glob("refs/*")?;
            builder.insert_walk(&mut walk)?;
        }
    }
    for id in &ids {
        builder.insert_object(*id, None)?;
    }
    std::fs::create_dir_all(&pack_dir)?;
    builder.write(&pack_dir, 0o444)?;
    let written = builder
        .name()?
        .context("the pack has no name")?
        .trim_end_matches(".pack")
        .to_string();
    let new_base = if written.starts_with("pack-") {
        written
    } else {
        format!("pack-{written}")
    };
    let already_there = old_packs.contains(&new_base);
    if !pack_dir.join(format!("{new_base}.pack")).exists() {
        bail!("the new pack {new_base} was not written");
    }

    if let Err(e) = verify_pack(gitdir, &pack_dir, &new_base, &ids) {
        if !already_there {
            remove_pack_files(&pack_dir, &new_base);
        }
        return Err(e.context("the new pack failed verification; nothing was deleted"));
    }

    // The new pack provably holds every object that existed at the snapshot.
    if consolidate {
        for base in old_packs.iter().filter(|b| **b != new_base) {
            remove_pack_files(&pack_dir, base);
            report.packs_removed += 1;
        }
        if report.packs_removed > 0 {
            // a stale multi-pack-index would name packs that no longer exist
            let _ = std::fs::remove_file(pack_dir.join("multi-pack-index"));
        }
    }
    for (_, path) in &loose {
        // The two-letter directories are left in place even when empty: removing one
        // can race with a commit that is writing a new object into it, and fail that
        // commit. Git leaves empty ones too.
        if std::fs::remove_file(path).is_ok() {
            report.loose_removed += 1;
        }
    }
    let _ = odb.refresh();
    report.ran = true;
    Ok(report)
}

/// Loose objects as (id, file). Files that are not `objects/xx/<38 hex digits>` are skipped.
fn loose_objects(objects: &Path) -> Result<Vec<(Oid, PathBuf)>> {
    let mut found = Vec::new();
    for dir in std::fs::read_dir(objects)? {
        let dir = dir?;
        let prefix = dir.file_name().to_string_lossy().to_string();
        if prefix.len() != 2 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let rest = file.file_name().to_string_lossy().to_string();
            if rest.len() != 38 || !rest.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            if let Ok(id) = Oid::from_str(&format!("{prefix}{rest}")) {
                found.push((id, file.path()));
            }
        }
    }
    Ok(found)
}

/// Base names (`pack-<hash>`) of packs without a `.keep` file.
fn unkept_packs(pack_dir: &Path) -> Result<Vec<String>> {
    let Ok(entries) = std::fs::read_dir(pack_dir) else {
        return Ok(Vec::new());
    };
    let mut bases = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(base) = name.strip_suffix(".pack") {
            if !pack_dir.join(format!("{base}.keep")).exists() {
                bases.push(base.to_string());
            }
        }
    }
    Ok(bases)
}

fn remove_pack_files(pack_dir: &Path, base: &str) {
    for suffix in PACK_SIDE_FILES {
        let _ = std::fs::remove_file(pack_dir.join(format!("{base}{suffix}")));
    }
}

/// Read every id back from the new pack alone. libgit2 checks each object's hash on read.
fn verify_pack(gitdir: &Path, pack_dir: &Path, base: &str, ids: &[Oid]) -> Result<()> {
    let root = gitdir
        .join(TMP_DIR_NAME)
        .join(format!("verify-{}", uuid::Uuid::new_v4().simple()));
    let objects = root.join("objects");
    let isolated = objects.join("pack");
    std::fs::create_dir_all(&isolated)?;
    let result = (|| -> Result<()> {
        for suffix in [".pack", ".idx"] {
            let from = pack_dir.join(format!("{base}{suffix}"));
            let to = isolated.join(format!("{base}{suffix}"));
            if std::fs::hard_link(&from, &to).is_err() {
                std::fs::copy(&from, &to)?;
            }
        }
        let odb = Odb::new()?;
        odb.add_disk_alternate(objects.to_str().context("non-UTF-8 path")?)?;
        for id in ids {
            odb.read(*id)
                .with_context(|| format!("object {id} is missing or corrupt in the new pack"))?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&root);
    result
}

fn remove_stale_verify_dirs(gitdir: &Path) {
    let Ok(entries) = std::fs::read_dir(gitdir.join(TMP_DIR_NAME)) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("verify-") {
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                warn!(error = %e, "could not remove a stale verification directory");
            }
        }
    }
}
