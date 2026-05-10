//! `FuseHandler` backed by the host filesystem via `std::fs`.
//!
//! This is the first offline-implementable step toward end-to-end `virtio-fs`:
//! the request parsing and dispatch loop already exist, so a real handler gives
//! us unit-testable semantics before any KVM/virtqueue plumbing lands.
//!
//! Scope:
//! - Rooted at a caller-provided host directory.
//! - Supports the common file-copy path (`Lookup`, `Getattr`, `Open`, `Read`,
//!   `Write`, `Mkdir`, `Rename`, `Unlink`, `Rmdir`, `Symlink`, `Readlink`,
//!   `Readdir`, `Statfs`, `Release`, `Releasedir`, `Flush`, `Fsync`).
//! - Uses internal node IDs / file handles; they do not attempt to mirror the
//!   host inode number / file descriptor values.
//! - Deliberately keeps special files (`mknod` for device nodes, ownership /
//!   timestamp updates in `setattr`, ...) out of scope for now.

#![allow(clippy::struct_excessive_bools)]

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(not(unix))]
use std::time::{Duration, UNIX_EPOCH};

use crate::body::{dt, fattr, FuseDirentWriter};
use crate::dispatch::{split_nul, FuseHandler, EINVAL, ENOSYS};
use crate::{
    FuseAttr, FuseAttrOut, FuseEntryOut, FuseFlushIn, FuseFsyncIn, FuseGetattrIn, FuseInHeader,
    FuseInitIn, FuseInitOut, FuseLinkIn, FuseMkdirIn, FuseMknodIn, FuseOpenIn, FuseOpenOut,
    FuseReadIn, FuseReleaseIn, FuseRenameIn, FuseSetattrIn, FuseStatfsOut, FuseWriteIn,
    FuseWriteOut, FUSE_FSYNC_FDATASYNC, FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION,
};

const ROOT_NODE_ID: u64 = 1;
const ATTR_TTL_SECS: u64 = 1;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EBADF: i32 = 9;
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const ENOTEMPTY: i32 = 39;

const O_ACCMODE: u32 = 0o3;
const O_WRONLY: u32 = 0o1;
const O_RDWR: u32 = 0o2;
const O_APPEND: u32 = 0o2000;
const O_TRUNC: u32 = 0o1000;
const O_CREAT: u32 = 0o100;

#[cfg(unix)]
const S_IFMT: u32 = 0o170000;
#[cfg(unix)]
const S_IFREG: u32 = 0o100000;

#[derive(Debug)]
struct FileHandle {
    file: File,
}

#[derive(Debug)]
struct DirHandle {
    path: PathBuf,
}

/// `std::fs`-backed [`FuseHandler`].
#[derive(Debug)]
pub struct StdFsHandler {
    nodes: HashMap<u64, PathBuf>,
    paths: HashMap<PathBuf, u64>,
    next_node: u64,
    next_handle: u64,
    files: HashMap<u64, FileHandle>,
    dirs: HashMap<u64, DirHandle>,
}

impl StdFsHandler {
    /// Construct a handler rooted at `root`.
    ///
    /// `root` must already exist and be a directory.
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        let meta = fs::metadata(&root)?;
        if !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is not a directory", root.display()),
            ));
        }
        let root = root.canonicalize()?;
        let mut nodes = HashMap::new();
        let mut paths = HashMap::new();
        nodes.insert(ROOT_NODE_ID, root.clone());
        paths.insert(root.clone(), ROOT_NODE_ID);
        Ok(Self {
            nodes,
            paths,
            next_node: ROOT_NODE_ID + 1,
            next_handle: 1,
            files: HashMap::new(),
            dirs: HashMap::new(),
        })
    }

    fn resolve_node(&self, nodeid: u64) -> Result<&PathBuf, i32> {
        self.nodes.get(&nodeid).ok_or(ENOENT)
    }

    fn register_path(&mut self, path: PathBuf) -> u64 {
        if let Some(id) = self.paths.get(&path) {
            return *id;
        }
        let id = self.next_node;
        self.next_node += 1;
        self.nodes.insert(id, path.clone());
        self.paths.insert(path, id);
        id
    }

    fn forget_path(&mut self, path: &Path) {
        if let Some(id) = self.paths.remove(path) {
            self.nodes.remove(&id);
        }
    }

    fn rebase_paths(&mut self, old_base: &Path, new_base: &Path) {
        let old_paths: Vec<PathBuf> = self
            .paths
            .keys()
            .filter(|p| p.starts_with(old_base))
            .cloned()
            .collect();
        for old in old_paths {
            if let Some(id) = self.paths.remove(&old) {
                if let Ok(suffix) = old.strip_prefix(old_base) {
                    let new = new_base.join(suffix);
                    self.paths.insert(new.clone(), id);
                    self.nodes.insert(id, new);
                }
            }
        }
    }

    fn child_path(&self, parent: u64, raw_name: &[u8]) -> Result<PathBuf, i32> {
        let name = match split_nul(raw_name) {
            Ok((name, _)) => name,
            Err(_) => raw_name,
        };
        let name = std::str::from_utf8(name).map_err(|_| EINVAL)?;
        let rel = Path::new(name);
        if rel.components().count() != 1
            || matches!(
                rel.components().next(),
                Some(
                    Component::CurDir
                        | Component::ParentDir
                        | Component::RootDir
                        | Component::Prefix(_)
                ) | None
            )
        {
            return Err(EINVAL);
        }
        Ok(self.resolve_node(parent)?.join(rel))
    }

    fn node_attr(&mut self, path: &Path) -> Result<(u64, FuseAttr), i32> {
        let meta = fs::symlink_metadata(path).map_err(io_to_errno)?;
        let id = self.register_path(path.to_path_buf());
        Ok((id, metadata_to_attr(id, &meta)))
    }

    fn entry_for_path(&mut self, path: &Path) -> Result<FuseEntryOut, i32> {
        let (id, attr) = self.node_attr(path)?;
        Ok(FuseEntryOut {
            nodeid: id,
            generation: 0,
            entry_valid: ATTR_TTL_SECS,
            attr_valid: ATTR_TTL_SECS,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr,
        })
    }

    fn attr_out(&mut self, path: &Path) -> Result<FuseAttrOut, i32> {
        let (_, attr) = self.node_attr(path)?;
        Ok(FuseAttrOut {
            attr_valid: ATTR_TTL_SECS,
            attr_valid_nsec: 0,
            dummy: 0,
            attr,
        })
    }

    fn next_fh(&mut self) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        fh
    }

    fn resolve_file(&mut self, fh: u64) -> Result<&mut FileHandle, i32> {
        self.files.get_mut(&fh).ok_or(EBADF)
    }

    fn resolve_dir(&self, fh: u64) -> Result<&DirHandle, i32> {
        self.dirs.get(&fh).ok_or(EBADF)
    }

    fn build_dirent_payload(
        &mut self,
        dir: &Path,
        start: usize,
        cap: usize,
    ) -> Result<Vec<u8>, i32> {
        let mut entries: Vec<(String, u32, PathBuf)> = Vec::new();
        entries.push((".".into(), dt::DIR, dir.to_path_buf()));
        let parent = dir.parent().unwrap_or(dir).to_path_buf();
        entries.push(("..".into(), dt::DIR, parent));
        let read_dir = fs::read_dir(dir).map_err(io_to_errno)?;
        for entry in read_dir {
            let entry = entry.map_err(io_to_errno)?;
            let path = entry.path();
            let ftype = entry
                .file_type()
                .map_err(io_to_errno)
                .map(file_type_to_dt)?;
            entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                ftype,
                path,
            ));
        }

        let mut writer = FuseDirentWriter::with_capacity(cap);
        for (idx, (name, ftype, path)) in entries.into_iter().enumerate().skip(start) {
            let nodeid = self.register_path(path);
            if writer
                .push(nodeid, (idx + 1) as u64, ftype, name.as_bytes())
                .is_err()
            {
                break;
            }
        }
        Ok(writer.into_bytes())
    }
}

impl FuseHandler for StdFsHandler {
    fn init(&mut self, _hdr: &FuseInHeader, req: &FuseInitIn) -> Result<FuseInitOut, i32> {
        Ok(FuseInitOut::minimal(
            FUSE_KERNEL_VERSION,
            FUSE_KERNEL_MINOR_VERSION,
            req.max_readahead,
            0,
            1024 * 1024,
        ))
    }

    fn getattr(&mut self, hdr: &FuseInHeader, _req: &FuseGetattrIn) -> Result<FuseAttrOut, i32> {
        let path = self.resolve_node(hdr.nodeid)?.clone();
        self.attr_out(&path)
    }

    fn setattr(&mut self, hdr: &FuseInHeader, req: &FuseSetattrIn) -> Result<FuseAttrOut, i32> {
        let path = self.resolve_node(hdr.nodeid)?.clone();
        if req.valid & fattr::SIZE != 0 {
            OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(io_to_errno)?
                .set_len(req.size)
                .map_err(io_to_errno)?;
        }
        #[cfg(unix)]
        if req.valid & fattr::MODE != 0 {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(req.mode & 0o7777);
            fs::set_permissions(&path, perms).map_err(io_to_errno)?;
        }
        if req.valid & !(fattr::SIZE | fattr::MODE) != 0 {
            return Err(ENOSYS);
        }
        self.attr_out(&path)
    }

    fn lookup(&mut self, hdr: &FuseInHeader, name: &[u8]) -> Result<FuseEntryOut, i32> {
        let path = self.child_path(hdr.nodeid, name)?;
        self.entry_for_path(&path)
    }

    fn readlink(&mut self, hdr: &FuseInHeader) -> Result<Vec<u8>, i32> {
        let path = self.resolve_node(hdr.nodeid)?.clone();
        let target = fs::read_link(path).map_err(io_to_errno)?;
        Ok(target.as_os_str().as_encoded_bytes().to_vec())
    }

    fn mknod(
        &mut self,
        hdr: &FuseInHeader,
        req: &FuseMknodIn,
        name: &[u8],
    ) -> Result<FuseEntryOut, i32> {
        let path = self.child_path(hdr.nodeid, name)?;
        #[cfg(unix)]
        let kind = req.mode & S_IFMT;
        #[cfg(unix)]
        if kind != 0 && kind != S_IFREG {
            return Err(ENOSYS);
        }
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(io_to_errno)?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(req.mode & 0o7777))
                .map_err(io_to_errno)?;
        }
        self.entry_for_path(&path)
    }

    fn mkdir(
        &mut self,
        hdr: &FuseInHeader,
        req: &FuseMkdirIn,
        name: &[u8],
    ) -> Result<FuseEntryOut, i32> {
        let path = self.child_path(hdr.nodeid, name)?;
        fs::create_dir(&path).map_err(io_to_errno)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(req.mode & 0o7777))
                .map_err(io_to_errno)?;
        }
        self.entry_for_path(&path)
    }

    fn unlink(&mut self, hdr: &FuseInHeader, name: &[u8]) -> Result<(), i32> {
        let path = self.child_path(hdr.nodeid, name)?;
        fs::remove_file(&path).map_err(io_to_errno)?;
        self.forget_path(&path);
        Ok(())
    }

    fn rmdir(&mut self, hdr: &FuseInHeader, name: &[u8]) -> Result<(), i32> {
        let path = self.child_path(hdr.nodeid, name)?;
        fs::remove_dir(&path).map_err(io_to_errno)?;
        self.forget_path(&path);
        Ok(())
    }

    fn symlink(
        &mut self,
        hdr: &FuseInHeader,
        name: &[u8],
        target: &[u8],
    ) -> Result<FuseEntryOut, i32> {
        let link = self.child_path(hdr.nodeid, name)?;
        let target = PathBuf::from(std::str::from_utf8(target).map_err(|_| EINVAL)?);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link).map_err(io_to_errno)?;
            self.entry_for_path(&link)
        }
        #[cfg(not(unix))]
        {
            let _ = link;
            let _ = target;
            Err(ENOSYS)
        }
    }

    fn rename(
        &mut self,
        hdr: &FuseInHeader,
        req: &FuseRenameIn,
        old_name: &[u8],
        new_name: &[u8],
    ) -> Result<(), i32> {
        let old_path = self.child_path(hdr.nodeid, old_name)?;
        let new_path = self.child_path(req.newdir, new_name)?;
        fs::rename(&old_path, &new_path).map_err(io_to_errno)?;
        self.rebase_paths(&old_path, &new_path);
        Ok(())
    }

    fn link(
        &mut self,
        hdr: &FuseInHeader,
        req: &FuseLinkIn,
        name: &[u8],
    ) -> Result<FuseEntryOut, i32> {
        let src = self.resolve_node(req.oldnodeid)?.clone();
        let dst = self.child_path(hdr.nodeid, name)?;
        fs::hard_link(&src, &dst).map_err(io_to_errno)?;
        self.entry_for_path(&dst)
    }

    fn open(&mut self, hdr: &FuseInHeader, req: &FuseOpenIn) -> Result<FuseOpenOut, i32> {
        let path = self.resolve_node(hdr.nodeid)?.clone();
        let mut opts = OpenOptions::new();
        match req.flags & O_ACCMODE {
            O_WRONLY => {
                opts.write(true);
            }
            O_RDWR => {
                opts.read(true).write(true);
            }
            _ => {
                opts.read(true);
            }
        }
        if req.flags & O_APPEND != 0 {
            opts.append(true);
        }
        if req.flags & O_TRUNC != 0 {
            opts.truncate(true);
        }
        if req.flags & O_CREAT != 0 {
            opts.create(true);
        }
        let file = opts.open(&path).map_err(io_to_errno)?;
        let fh = self.next_fh();
        self.files.insert(fh, FileHandle { file });
        Ok(FuseOpenOut {
            fh,
            open_flags: 0,
            padding: 0,
        })
    }

    fn read(&mut self, _hdr: &FuseInHeader, req: &FuseReadIn) -> Result<Vec<u8>, i32> {
        let handle = self.resolve_file(req.fh)?;
        let file = &mut handle.file;
        file.seek(SeekFrom::Start(req.offset))
            .map_err(io_to_errno)?;
        let mut buf = vec![0u8; req.size as usize];
        let n = file.read(&mut buf).map_err(io_to_errno)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn write(
        &mut self,
        _hdr: &FuseInHeader,
        req: &FuseWriteIn,
        data: &[u8],
    ) -> Result<FuseWriteOut, i32> {
        let handle = self.resolve_file(req.fh)?;
        let file = &mut handle.file;
        file.seek(SeekFrom::Start(req.offset))
            .map_err(io_to_errno)?;
        let written = file.write(data).map_err(io_to_errno)?;
        Ok(FuseWriteOut {
            size: written as u32,
            padding: 0,
        })
    }

    fn statfs(&mut self, _hdr: &FuseInHeader) -> Result<FuseStatfsOut, i32> {
        Ok(FuseStatfsOut {
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: self.paths.len() as u64,
            ffree: 0,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
            padding: 0,
            spare: [0; 6],
        })
    }

    fn release(&mut self, _hdr: &FuseInHeader, req: &FuseReleaseIn) -> Result<(), i32> {
        self.files.remove(&req.fh).ok_or(EBADF)?;
        Ok(())
    }

    fn fsync(&mut self, _hdr: &FuseInHeader, req: &FuseFsyncIn) -> Result<(), i32> {
        let handle = self.resolve_file(req.fh)?;
        if req.fsync_flags & FUSE_FSYNC_FDATASYNC != 0 {
            handle.file.sync_data().map_err(io_to_errno)?;
        } else {
            handle.file.sync_all().map_err(io_to_errno)?;
        }
        Ok(())
    }

    fn flush(&mut self, _hdr: &FuseInHeader, req: &FuseFlushIn) -> Result<(), i32> {
        let handle = self.resolve_file(req.fh)?;
        handle.file.flush().map_err(io_to_errno)?;
        Ok(())
    }

    fn opendir(&mut self, hdr: &FuseInHeader, _req: &FuseOpenIn) -> Result<FuseOpenOut, i32> {
        let path = self.resolve_node(hdr.nodeid)?.clone();
        if !fs::metadata(&path).map_err(io_to_errno)?.is_dir() {
            return Err(ENOTDIR);
        }
        let fh = self.next_fh();
        self.dirs.insert(fh, DirHandle { path });
        Ok(FuseOpenOut {
            fh,
            open_flags: 0,
            padding: 0,
        })
    }

    fn readdir(&mut self, _hdr: &FuseInHeader, req: &FuseReadIn) -> Result<Vec<u8>, i32> {
        let dir = self.resolve_dir(req.fh)?.path.clone();
        self.build_dirent_payload(&dir, req.offset as usize, req.size as usize)
    }

    fn releasedir(&mut self, _hdr: &FuseInHeader, req: &FuseReleaseIn) -> Result<(), i32> {
        self.dirs.remove(&req.fh).ok_or(EBADF)?;
        Ok(())
    }
}

fn io_to_errno(err: std::io::Error) -> i32 {
    match err.kind() {
        std::io::ErrorKind::NotFound => ENOENT,
        std::io::ErrorKind::AlreadyExists => EEXIST,
        std::io::ErrorKind::PermissionDenied => EIO,
        std::io::ErrorKind::InvalidInput => EINVAL,
        std::io::ErrorKind::DirectoryNotEmpty => ENOTEMPTY,
        std::io::ErrorKind::IsADirectory => EISDIR,
        std::io::ErrorKind::NotADirectory => ENOTDIR,
        _ => EIO,
    }
}

#[cfg(unix)]
fn metadata_to_attr(nodeid: u64, meta: &fs::Metadata) -> FuseAttr {
    use std::os::unix::fs::MetadataExt;
    FuseAttr {
        ino: nodeid,
        size: meta.size(),
        blocks: meta.blocks(),
        atime: meta.atime() as u64,
        mtime: meta.mtime() as u64,
        ctime: meta.ctime() as u64,
        atimensec: meta.atime_nsec() as u32,
        mtimensec: meta.mtime_nsec() as u32,
        ctimensec: meta.ctime_nsec() as u32,
        mode: meta.mode(),
        nlink: meta.nlink() as u32,
        uid: meta.uid(),
        gid: meta.gid(),
        rdev: meta.rdev() as u32,
        blksize: meta.blksize() as u32,
        flags: 0,
    }
}

#[cfg(not(unix))]
fn metadata_to_attr(nodeid: u64, meta: &fs::Metadata) -> FuseAttr {
    let mode = if meta.is_dir() {
        0o040755
    } else if meta.file_type().is_symlink() {
        0o120777
    } else {
        0o100644
    };
    let secs = meta
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    FuseAttr {
        ino: nodeid,
        size: meta.len(),
        blocks: 0,
        atime: secs,
        mtime: secs,
        ctime: secs,
        atimensec: 0,
        mtimensec: 0,
        ctimensec: 0,
        mode,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn file_type_to_dt(ft: fs::FileType) -> u32 {
    if ft.is_dir() {
        dt::DIR
    } else if ft.is_symlink() {
        dt::LNK
    } else if ft.is_file() {
        dt::REG
    } else {
        dt::UNKNOWN
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::{dispatch, parse_request};
    use crate::{
        FuseDirentIter, FuseInHeader, FuseOpcode, FuseOutHeader, FUSE_IN_HDR_LEN, FUSE_OPEN_IN_LEN,
        FUSE_OUT_HDR_LEN, FUSE_READ_IN_LEN, FUSE_RELEASE_IN_LEN, FUSE_WRITE_IN_LEN,
    };

    fn temp_root(slug: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rust-nano-vm-virtiofs-{}-{}",
            slug,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn packet(hdr: FuseInHeader, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&hdr.to_bytes());
        out.extend_from_slice(body);
        out
    }

    fn hdr(op: FuseOpcode, nodeid: u64, body_len: usize) -> FuseInHeader {
        FuseInHeader {
            len: (FUSE_IN_HDR_LEN + body_len) as u32,
            opcode: op,
            unique: 7,
            nodeid,
            uid: 0,
            gid: 0,
            pid: 0,
            total_extlen: 0,
            padding: 0,
        }
    }

    fn decode_ok_body(resp: &[u8]) -> &[u8] {
        let out = FuseOutHeader::from_bytes(resp).unwrap();
        assert_eq!(out.error, 0);
        &resp[FUSE_OUT_HDR_LEN..]
    }

    #[test]
    fn init_dispatches_to_real_handler() {
        let dir = temp_root("init");
        let mut handler = StdFsHandler::new(&dir).unwrap();
        let req = FuseInitIn {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            max_readahead: 8192,
            flags: 0,
        };
        let pkt = packet(
            hdr(FuseOpcode::Init, ROOT_NODE_ID, crate::FUSE_INIT_IN_LEN),
            &req.to_bytes(),
        );
        let (in_hdr, req) = parse_request(&pkt).unwrap();
        let resp = dispatch(&in_hdr, req, &mut handler).unwrap();
        let body = FuseInitOut::from_bytes(decode_ok_body(&resp)).unwrap();
        assert_eq!(body.major, FUSE_KERNEL_VERSION);
        assert_eq!(body.minor, FUSE_KERNEL_MINOR_VERSION);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lookup_open_read_release_roundtrip() {
        let dir = temp_root("read");
        fs::write(dir.join("hello.txt"), b"hello from std-fs handler").unwrap();
        let mut handler = StdFsHandler::new(&dir).unwrap();

        let lookup_pkt = packet(hdr(FuseOpcode::Lookup, ROOT_NODE_ID, 10), b"hello.txt\0");
        let (lookup_hdr, lookup_req) = parse_request(&lookup_pkt).unwrap();
        let lookup_resp = dispatch(&lookup_hdr, lookup_req, &mut handler).unwrap();
        let entry = FuseEntryOut::from_bytes(decode_ok_body(&lookup_resp)).unwrap();

        let open = FuseOpenIn {
            flags: 0,
            open_flags: 0,
        };
        let open_pkt = packet(
            hdr(FuseOpcode::Open, entry.nodeid, FUSE_OPEN_IN_LEN),
            &open.to_bytes(),
        );
        let (open_hdr, open_req) = parse_request(&open_pkt).unwrap();
        let open_resp = dispatch(&open_hdr, open_req, &mut handler).unwrap();
        let opened = FuseOpenOut::from_bytes(decode_ok_body(&open_resp)).unwrap();

        let read = FuseReadIn {
            fh: opened.fh,
            offset: 0,
            size: 1024,
            read_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };
        let read_pkt = packet(
            hdr(FuseOpcode::Read, entry.nodeid, FUSE_READ_IN_LEN),
            &read.to_bytes(),
        );
        let (read_hdr, read_req) = parse_request(&read_pkt).unwrap();
        let read_resp = dispatch(&read_hdr, read_req, &mut handler).unwrap();
        assert_eq!(decode_ok_body(&read_resp), b"hello from std-fs handler");

        let release = FuseReleaseIn {
            fh: opened.fh,
            flags: 0,
            release_flags: 0,
            lock_owner: 0,
        };
        let rel_pkt = packet(
            hdr(FuseOpcode::Release, entry.nodeid, FUSE_RELEASE_IN_LEN),
            &release.to_bytes(),
        );
        let (rel_hdr, rel_req) = parse_request(&rel_pkt).unwrap();
        let rel_resp = dispatch(&rel_hdr, rel_req, &mut handler).unwrap();
        assert_eq!(decode_ok_body(&rel_resp), b"");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn open_write_and_readdir_work() {
        let dir = temp_root("write-readdir");
        fs::write(dir.join("existing.txt"), b"seed").unwrap();
        let mut handler = StdFsHandler::new(&dir).unwrap();

        let created = handler
            .mknod(
                &hdr(FuseOpcode::Mknod, ROOT_NODE_ID, crate::FUSE_MKNOD_IN_LEN),
                &FuseMknodIn {
                    mode: 0o100644,
                    rdev: 0,
                    umask: 0,
                    padding: 0,
                },
                b"new.txt\0",
            )
            .unwrap();
        let opened = handler
            .open(
                &hdr(FuseOpcode::Open, created.nodeid, FUSE_OPEN_IN_LEN),
                &FuseOpenIn {
                    flags: O_RDWR | O_CREAT,
                    open_flags: 0,
                },
            )
            .unwrap();
        let wrote = handler
            .write(
                &hdr(FuseOpcode::Write, created.nodeid, FUSE_WRITE_IN_LEN),
                &FuseWriteIn {
                    fh: opened.fh,
                    offset: 0,
                    size: 5,
                    write_flags: 0,
                    lock_owner: 0,
                    flags: 0,
                    padding: 0,
                },
                b"hello",
            )
            .unwrap();
        assert_eq!(wrote.size, 5);
        handler
            .release(
                &hdr(FuseOpcode::Release, created.nodeid, FUSE_RELEASE_IN_LEN),
                &FuseReleaseIn {
                    fh: opened.fh,
                    flags: 0,
                    release_flags: 0,
                    lock_owner: 0,
                },
            )
            .unwrap();
        assert_eq!(fs::read(dir.join("new.txt")).unwrap(), b"hello");

        let dir_open = handler
            .opendir(
                &hdr(FuseOpcode::Opendir, ROOT_NODE_ID, FUSE_OPEN_IN_LEN),
                &FuseOpenIn {
                    flags: 0,
                    open_flags: 0,
                },
            )
            .unwrap();
        let payload = handler
            .readdir(
                &hdr(FuseOpcode::Readdir, ROOT_NODE_ID, FUSE_READ_IN_LEN),
                &FuseReadIn {
                    fh: dir_open.fh,
                    offset: 0,
                    size: 4096,
                    read_flags: 0,
                    lock_owner: 0,
                    flags: 0,
                    padding: 0,
                },
            )
            .unwrap();
        let names: Vec<String> = FuseDirentIter::new(&payload)
            .map(|res| String::from_utf8(res.unwrap().1.to_vec()).unwrap())
            .collect();
        assert!(names.iter().any(|n| n == "new.txt"));
        assert!(names.iter().any(|n| n == "existing.txt"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rename_symlink_and_readlink_work() {
        let dir = temp_root("rename-link");
        fs::write(dir.join("old.txt"), b"payload").unwrap();
        let mut handler = StdFsHandler::new(&dir).unwrap();
        let old = handler
            .lookup(&hdr(FuseOpcode::Lookup, ROOT_NODE_ID, 8), b"old.txt\0")
            .unwrap();
        handler
            .rename(
                &hdr(FuseOpcode::Rename, ROOT_NODE_ID, crate::FUSE_RENAME_IN_LEN),
                &FuseRenameIn {
                    newdir: ROOT_NODE_ID,
                },
                b"old.txt\0",
                b"renamed.txt\0",
            )
            .unwrap();
        assert!(dir.join("renamed.txt").exists());
        assert_eq!(
            handler.resolve_node(old.nodeid).unwrap(),
            &dir.join("renamed.txt")
        );

        #[cfg(unix)]
        {
            let link = handler
                .symlink(
                    &hdr(FuseOpcode::Symlink, ROOT_NODE_ID, 0),
                    b"link.txt",
                    b"renamed.txt",
                )
                .unwrap();
            let target = handler
                .readlink(&hdr(FuseOpcode::Readlink, link.nodeid, 0))
                .unwrap();
            assert_eq!(target, b"renamed.txt");
        }

        let _ = fs::remove_dir_all(dir);
    }
}
