//! The WebDAV filesystem: a working tree on disk, with `.git` hidden, uploads written
//! atomically through temp files inside `.git`, an upload size limit, symlinks never
//! followed, and every mutation reported to a `ChangeSink`.
//!
//! Contract with `dav-server` 0.11 (verified by tests/davfs.rs): a PUT calls
//! `DavFile::flush` once after the last body chunk, and a recursive DELETE or COPY
//! calls the filesystem once per child. So `flush` is the commit point for an upload,
//! and a request can record many changes. LOCK on a missing path opens a file for
//! writing and drops it without flushing, so no empty "lock-null" file is created.

use std::fmt;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::{future, stream, FutureExt};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::brand::TMP_DIR_NAME;
use crate::change::{Change, ChangeSink, Op};

#[cfg(test)]
mod tests {
    use super::is_dotgit_alias;

    #[test]
    fn aliases_are_recognised_and_ordinary_names_are_not() {
        for name in [
            ".git",
            ".GIT",
            ".Git",
            ".git.",
            ".git ",
            ".git. .",
            "GIT~1",
            "git~1",
            ".g\u{200c}it",
            ".git::$INDEX_ALLOCATION",
            ".GiT\u{feff}",
        ] {
            assert!(is_dotgit_alias(name), "{name:?}");
        }
        for name in [
            ".gitignore",
            ".gitattributes",
            ".github",
            ".git-notes.md",
            "gitfoo",
            "git",
            "my.git",
            ".gi",
            "git~2",
            "..git",
            "",
        ] {
            assert!(!is_dotgit_alias(name), "{name:?}");
        }
    }
}

#[derive(Clone)]
pub struct GitFs {
    root: Arc<PathBuf>,
    tmp_dir: Arc<PathBuf>,
    max_file_size: Option<u64>,
    sink: Arc<dyn ChangeSink>,
}

/// True for names that git, or the filesystem of someone who clones this repository,
/// treats as the `.git` directory: other letter cases (`.GIT`), trailing dots and
/// spaces (`.git.`), the Windows short name (`GIT~1`), NTFS stream suffixes
/// (`.git::$INDEX_ALLOCATION`) and macOS-ignored invisible characters (`.g<ZWNJ>it`).
pub fn is_dotgit_alias(name: &str) -> bool {
    let base = name.split(':').next().unwrap_or(name);
    let visible: String = base
        .chars()
        .filter(|c| {
            !matches!(
                *c,
                '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}'
            )
        })
        .collect();
    let lower = visible.to_lowercase();
    let trimmed = lower.trim_end_matches(['.', ' ']);
    trimmed == ".git" || trimmed == "git~1"
}

impl GitFs {
    /// `root` must be a git working tree (its `.git` directory holds upload temp files).
    /// Leftover temp files from a crash are removed.
    pub fn new(root: PathBuf, max_file_size: Option<u64>, sink: Arc<dyn ChangeSink>) -> GitFs {
        let tmp_dir = root.join(".git").join(TMP_DIR_NAME);
        if let Ok(entries) = std::fs::read_dir(&tmp_dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        GitFs {
            root: Arc::new(root),
            tmp_dir: Arc::new(tmp_dir),
            max_file_size,
            sink,
        }
    }

    /// Repo-relative path with `/` separators ("" for the root). Rejects `.git` components.
    fn rel(&self, path: &DavPath, write: bool) -> FsResult<String> {
        let denied = if write {
            FsError::Forbidden
        } else {
            FsError::NotFound
        };
        let Ok(text) = std::str::from_utf8(path.as_bytes()) else {
            return Err(denied);
        };
        let mut parts = Vec::new();
        for component in text.split('/') {
            match component {
                "" | "." => {}
                ".." => return Err(FsError::Forbidden),
                c if is_dotgit_alias(c) => return Err(denied),
                c => parts.push(c),
            }
        }
        Ok(parts.join("/"))
    }

    /// Filesystem path for `rel`, refusing to pass through any symlink.
    async fn resolve(&self, rel: &str, write: bool) -> FsResult<PathBuf> {
        let mut current = self.root.as_ref().clone();
        for component in rel.split('/').filter(|c| !c.is_empty()) {
            current.push(component);
            match tokio::fs::symlink_metadata(&current).await {
                Ok(md) if md.file_type().is_symlink() => {
                    return Err(if write {
                        FsError::Forbidden
                    } else {
                        FsError::NotFound
                    })
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(&e)),
            }
        }
        Ok(current)
    }

    async fn new_temp_path(&self) -> FsResult<PathBuf> {
        match tokio::fs::create_dir(self.tmp_dir.as_ref()).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(FsError::GeneralFailure),
        }
        Ok(self.tmp_dir.join(uuid::Uuid::new_v4().simple().to_string()))
    }

    fn record(&self, change: Change) {
        self.sink.record(change);
    }
}

fn io_err(e: &std::io::Error) -> FsError {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound | NotADirectory => FsError::NotFound,
        PermissionDenied | DirectoryNotEmpty | IsADirectory => FsError::Forbidden,
        AlreadyExists => FsError::Exists,
        CrossesDevices => FsError::IsRemote,
        StorageFull => FsError::InsufficientStorage,
        InvalidFilename => FsError::PathTooLong,
        _ => FsError::GeneralFailure,
    }
}

#[derive(Debug, Clone)]
struct Meta {
    len: u64,
    modified: SystemTime,
    dir: bool,
}

impl From<&std::fs::Metadata> for Meta {
    fn from(md: &std::fs::Metadata) -> Self {
        Meta {
            len: md.len(),
            modified: md.modified().unwrap_or(UNIX_EPOCH),
            dir: md.is_dir(),
        }
    }
}

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        self.len
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }

    fn is_dir(&self) -> bool {
        self.dir
    }

    /// Size plus modification time in nanoseconds. Stable until the file changes.
    fn etag(&self) -> Option<String> {
        let nanos = self.modified.duration_since(UNIX_EPOCH).ok()?.as_nanos();
        Some(if self.dir {
            format!("{nanos:x}")
        } else {
            format!("{:x}-{nanos:x}", self.len)
        })
    }
}

struct Entry {
    name: Vec<u8>,
    meta: Meta,
}

impl DavDirEntry for Entry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }

    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        future::ready(Ok(Box::new(self.meta.clone()) as Box<dyn DavMetaData>)).boxed()
    }
}

impl DavFileSystem for GitFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        async move {
            let writing = options.write
                || options.append
                || options.create
                || options.create_new
                || options.truncate;
            let rel = self.rel(path, writing)?;
            let target = self.resolve(&rel, writing).await?;

            if !writing {
                let md = tokio::fs::symlink_metadata(&target)
                    .await
                    .map_err(|e| io_err(&e))?;
                if md.is_dir() {
                    return Err(FsError::Forbidden);
                }
                let file = tokio::fs::File::open(&target)
                    .await
                    .map_err(|e| io_err(&e))?;
                return Ok(Box::new(ReadFile { file }) as Box<dyn DavFile>);
            }

            if rel.is_empty() {
                return Err(FsError::Forbidden);
            }
            if let (Some(max), Some(size)) = (self.max_file_size, options.size) {
                if size > max {
                    return Err(FsError::TooLarge);
                }
            }
            let exists = match tokio::fs::symlink_metadata(&target).await {
                Ok(md) if md.is_dir() => return Err(FsError::Forbidden),
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(io_err(&e)),
            };
            if exists && options.create_new {
                return Err(FsError::Exists);
            }
            if !exists && !options.create {
                return Err(FsError::NotFound);
            }
            let parent_ok = match target.parent() {
                Some(parent) => tokio::fs::metadata(parent)
                    .await
                    .map(|m| m.is_dir())
                    .unwrap_or(false),
                None => false,
            };
            if !parent_ok {
                return Err(FsError::NotFound);
            }

            let temp = self.new_temp_path().await?;
            if exists && !options.truncate {
                tokio::fs::copy(&target, &temp)
                    .await
                    .map_err(|e| io_err(&e))?;
            }
            let file = match tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&temp)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&temp).await;
                    return Err(io_err(&e));
                }
            };
            Ok(Box::new(WriteFile {
                file: Some(file),
                temp,
                target,
                rel,
                sink: self.sink.clone(),
                max: self.max_file_size,
                expected: options.size,
                written: 0,
                pos: 0,
                committed: false,
            }) as Box<dyn DavFile>)
        }
        .boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        async move {
            let rel = self.rel(path, false)?;
            let dir = self.resolve(&rel, false).await?;
            let mut reader = tokio::fs::read_dir(&dir).await.map_err(|e| io_err(&e))?;
            let mut entries: Vec<FsResult<Box<dyn DavDirEntry>>> = Vec::new();
            while let Some(entry) = reader.next_entry().await.map_err(|e| io_err(&e))? {
                let name = entry.file_name();
                if is_dotgit_alias(&name.to_string_lossy()) {
                    continue;
                }
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };
                if file_type.is_symlink() {
                    continue;
                }
                let Ok(md) = entry.metadata().await else {
                    continue;
                };
                use std::os::unix::ffi::OsStrExt;
                entries.push(Ok(Box::new(Entry {
                    name: name.as_bytes().to_vec(),
                    meta: Meta::from(&md),
                }) as Box<dyn DavDirEntry>));
            }
            Ok(Box::pin(stream::iter(entries)) as FsStream<Box<dyn DavDirEntry>>)
        }
        .boxed()
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        async move {
            let rel = self.rel(path, false)?;
            let target = self.resolve(&rel, false).await?;
            let md = tokio::fs::symlink_metadata(&target)
                .await
                .map_err(|e| io_err(&e))?;
            if md.file_type().is_symlink() {
                return Err(FsError::NotFound);
            }
            Ok(Box::new(Meta::from(&md)) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel = self.rel(path, true)?;
            if rel.is_empty() {
                return Err(FsError::Exists);
            }
            let target = self.resolve(&rel, true).await?;
            tokio::fs::create_dir(&target)
                .await
                .map_err(|e| io_err(&e))?;
            self.record(Change::new(Op::Mkcol, rel));
            Ok(())
        }
        .boxed()
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel = self.rel(path, true)?;
            if rel.is_empty() {
                return Err(FsError::Forbidden);
            }
            let target = self.resolve(&rel, true).await?;
            tokio::fs::remove_dir(&target)
                .await
                .map_err(|e| io_err(&e))?;
            self.record(Change::new(Op::Delete, rel));
            Ok(())
        }
        .boxed()
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let rel = self.rel(path, true)?;
            let target = self.resolve(&rel, true).await?;
            tokio::fs::remove_file(&target)
                .await
                .map_err(|e| io_err(&e))?;
            self.record(Change::new(Op::Delete, rel));
            Ok(())
        }
        .boxed()
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let from_rel = self.rel(from, true)?;
            let to_rel = self.rel(to, true)?;
            if from_rel.is_empty() || to_rel.is_empty() {
                return Err(FsError::Forbidden);
            }
            if from_rel == to_rel {
                return Ok(());
            }
            let source = self.resolve(&from_rel, true).await?;
            let dest = self.resolve(&to_rel, true).await?;
            tokio::fs::rename(&source, &dest)
                .await
                .map_err(|e| io_err(&e))?;
            self.record(Change::with_target(Op::Move, from_rel, to_rel));
            Ok(())
        }
        .boxed()
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        async move {
            let from_rel = self.rel(from, false)?;
            let to_rel = self.rel(to, true)?;
            if to_rel.is_empty() {
                return Err(FsError::Forbidden);
            }
            let source = self.resolve(&from_rel, false).await?;
            let dest = self.resolve(&to_rel, true).await?;
            let md = tokio::fs::symlink_metadata(&source)
                .await
                .map_err(|e| io_err(&e))?;
            if !md.is_file() {
                return Err(FsError::Forbidden);
            }
            let temp = self.new_temp_path().await?;
            let result = async {
                tokio::fs::copy(&source, &temp).await?;
                tokio::fs::rename(&temp, &dest).await
            }
            .await;
            if let Err(e) = result {
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(io_err(&e));
            }
            self.record(Change::with_target(Op::Copy, from_rel, to_rel));
            Ok(())
        }
        .boxed()
    }
}

#[derive(Debug)]
struct ReadFile {
    file: tokio::fs::File,
}

impl DavFile for ReadFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            let md = self.file.metadata().await.map_err(|e| io_err(&e))?;
            Ok(Box::new(Meta::from(&md)) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn write_buf(&'_ mut self, _buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        future::ready(Err(FsError::Forbidden)).boxed()
    }

    fn write_bytes(&'_ mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        future::ready(Err(FsError::Forbidden)).boxed()
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        async move {
            let mut buf = vec![0u8; count];
            let n = self.file.read(&mut buf).await.map_err(|e| io_err(&e))?;
            buf.truncate(n);
            Ok(Bytes::from(buf))
        }
        .boxed()
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        async move { self.file.seek(pos).await.map_err(|e| io_err(&e)) }.boxed()
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        future::ready(Ok(())).boxed()
    }
}

/// An upload in progress. Data goes to a temp file; `flush` renames it over the target.
/// Dropping it without a successful `flush` deletes the temp file.
struct WriteFile {
    file: Option<tokio::fs::File>,
    temp: PathBuf,
    target: PathBuf,
    rel: String,
    sink: Arc<dyn ChangeSink>,
    max: Option<u64>,
    /// Bytes the client announced (Content-Length or X-Expected-Entity-Length).
    expected: Option<u64>,
    written: u64,
    pos: u64,
    committed: bool,
}

impl fmt::Debug for WriteFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteFile")
            .field("target", &self.target)
            .field("pos", &self.pos)
            .finish()
    }
}

impl WriteFile {
    async fn write_slice(&mut self, data: &[u8]) -> FsResult<()> {
        let end = self.pos + data.len() as u64;
        if self.max.is_some_and(|max| end > max) {
            self.file = None;
            let _ = tokio::fs::remove_file(&self.temp).await;
            return Err(FsError::TooLarge);
        }
        let file = self.file.as_mut().ok_or(FsError::GeneralFailure)?;
        file.write_all(data).await.map_err(|e| io_err(&e))?;
        self.pos = end;
        self.written += data.len() as u64;
        Ok(())
    }
}

impl Drop for WriteFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

impl DavFile for WriteFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        async move {
            let file = self.file.as_ref().ok_or(FsError::GeneralFailure)?;
            let md = file.metadata().await.map_err(|e| io_err(&e))?;
            Ok(Box::new(Meta::from(&md)) as Box<dyn DavMetaData>)
        }
        .boxed()
    }

    fn write_buf(&'_ mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        async move {
            while buf.has_remaining() {
                let n = {
                    let chunk = buf.chunk();
                    self.write_slice(chunk).await?;
                    chunk.len()
                };
                buf.advance(n);
            }
            Ok(())
        }
        .boxed()
    }

    fn write_bytes(&'_ mut self, buf: Bytes) -> FsFuture<'_, ()> {
        async move { self.write_slice(&buf).await }.boxed()
    }

    fn read_bytes(&'_ mut self, _count: usize) -> FsFuture<'_, Bytes> {
        future::ready(Err(FsError::Forbidden)).boxed()
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        async move {
            let file = self.file.as_mut().ok_or(FsError::GeneralFailure)?;
            let new = file.seek(pos).await.map_err(|e| io_err(&e))?;
            self.pos = new;
            Ok(new)
        }
        .boxed()
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        async move {
            if self.committed {
                return Ok(());
            }
            let mut file = self.file.take().ok_or(FsError::GeneralFailure)?;
            // dav-server checks the length only after flush, so a short upload (a chunked
            // body that ends early) would already have replaced the file. Refuse it here.
            if self.expected.is_some_and(|n| n != self.written) {
                drop(file);
                let _ = tokio::fs::remove_file(&self.temp).await;
                return Err(FsError::GeneralFailure);
            }
            file.flush().await.map_err(|e| io_err(&e))?;
            file.sync_all().await.map_err(|e| io_err(&e))?;
            drop(file);
            tokio::fs::rename(&self.temp, &self.target)
                .await
                .map_err(|e| io_err(&e))?;
            self.committed = true;
            self.sink.record(Change::new(Op::Put, self.rel.clone()));
            Ok(())
        }
        .boxed()
    }
}
