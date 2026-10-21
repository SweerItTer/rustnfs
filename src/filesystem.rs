//! Directory-capability backend. No client-controlled absolute path is used for I/O.
//!
//! File handles are kept for the lifetime of a registered object. This prevents OS
//! inode reuse from redirecting an old NFS handle to a newly created file. Names
//! are also revalidated before operations. All mutations are serialized.
use crate::config::{Config, ExportConfig};
use anyhow::{Context, Result};
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt, SystemTimeSpec};
use cap_std::{ambient_authority, fs::{Dir, File, Metadata, OpenOptions}};
use nfs3_server::{nfs3_types::nfs3::*, vfs::*};
use std::{collections::HashMap, io::{self, Read, Seek, SeekFrom, Write}, path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

pub const MAX_IO: u32 = 8192;
const MAX_FILE_SIZE: u64 = 1 << 40;
const MAX_HANDLES: usize = 32768;
const MAX_DIR_ENTRIES: usize = 20000;
const MAX_SNAPSHOTS: usize = 8;
type NfsResult<T> = std::result::Result<T, nfsstat3>;
type Handle = FileHandleU64;

struct Export { config: ExportConfig, dir: Dir }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity { dev: u64, ino: u64 }
#[cfg(windows)]
type DirectoryPin = std::fs::File;
#[cfg(not(windows))]
type DirectoryPin = Dir;
enum Pinned { File(File), Directory { _pin: DirectoryPin } }
#[derive(Clone)]
struct Node { export: usize, path: PathBuf, identity: Identity, pinned: Arc<Pinned>, mode: Option<u32> }
struct Snapshot { dir: u64, created: Instant, entries: Arc<Vec<DirEntryPlus<Handle>>> }
struct State {
    nodes: HashMap<u64, Node>, paths: HashMap<(usize, PathBuf), u64>, next_id: u64,
    snapshots: HashMap<u32, Snapshot>, next_snapshot: u32,
}

pub struct LocalFs { exports: Vec<Export>, state: Mutex<State>, started: SystemTime }

impl LocalFs {
    pub fn new(config: &Config) -> Result<Self> {
        config.validate()?;
        let exports = config.exports.iter().map(|config| {
            let dir = Dir::open_ambient_dir(&config.path, ambient_authority())
                .with_context(|| format!("无法打开共享目录 {}", config.path))?;
            Ok(Export { config: config.clone(), dir })
        }).collect::<Result<Vec<_>>>()?;
        let fs = Self { exports, state: Mutex::new(State {
            nodes: HashMap::new(), paths: HashMap::new(), next_id: 2,
            snapshots: HashMap::new(), next_snapshot: 1,
        }), started: SystemTime::now() };
        {
            let mut state = fs.lock().map_err(|s| anyhow::anyhow!("{s}"))?;
            for index in 0..fs.exports.len() {
                let export = &fs.exports[index].config;
                let context = || format!("初始化共享 '{}'（{}）失败", export.name, export.path);
                let (pinned, metadata) = fs.observe_root(index).with_context(context)?;
                fs.register_observed(&mut state, index, PathBuf::new(), None, pinned, metadata)
                    .map_err(|s| anyhow::anyhow!("{s}")).with_context(context)?;
            }
        }
        Ok(fs)
    }

    fn lock(&self) -> NfsResult<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| nfsstat3::NFS3ERR_SERVERFAULT)
    }

    fn parent(&self, export: usize, path: &Path) -> NfsResult<(Dir, String)> {
        let name = path.file_name().and_then(|n| n.to_str()).ok_or(nfsstat3::NFS3ERR_INVAL)?;
        validate_name(name)?;
        let mut dir = self.exports[export].dir.try_clone().map_err(nfs_error)?;
        if let Some(parent) = path.parent() {
            for component in parent.components() {
                let Component::Normal(component) = component else { return Err(nfsstat3::NFS3ERR_ACCES); };
                let name = component.to_str().ok_or(nfsstat3::NFS3ERR_INVAL)?;
                validate_name(name)?;
                dir = dir.open_dir_nofollow(name).map_err(nfs_error)?;
                regular_type(&dir.dir_metadata().map_err(nfs_error)?)?;
            }
        }
        Ok((dir, name.into()))
    }

    /// Reopens the expected directory through the export capability. The
    /// returned cap-std Dir retains its Windows no-FILE_SHARE_DELETE protection
    /// for the entire operation; the long-lived identity pin is never used for
    /// traversal or directory I/O.
    fn open_directory(&self, node: &Node) -> NfsResult<Dir> {
        let dir = if node.path.as_os_str().is_empty() {
            self.exports[node.export].dir.try_clone().map_err(nfs_error)?
        } else {
            let (parent, name) = self.parent(node.export, &node.path)?;
            parent.open_dir_nofollow(name).map_err(nfs_error)?
        };
        let metadata = dir.dir_metadata().map_err(nfs_error)?;
        regular_type(&metadata)?;
        if identity(&metadata) != node.identity { return Err(nfsstat3::NFS3ERR_STALE); }
        Ok(dir)
    }

    // Keep the original OS error and the failing step until startup has
    // finished; converting straight to an NFS status hid Windows API mistakes.
    fn observe_root(&self, export: usize) -> Result<(Arc<Pinned>, Metadata)> {
        let dir = self.exports[export].dir.try_clone().context("复制共享目录句柄失败")?;
        let metadata = dir.dir_metadata().context("读取共享目录属性失败")?;
        let pinned = pin_directory(dir, &metadata).context("创建共享目录属性句柄失败")?;
        Ok((pinned, metadata))
    }

    fn observe(&self, export: usize, path: &Path) -> NfsResult<(Arc<Pinned>, Metadata)> {
        if path.as_os_str().is_empty() {
            return self.observe_root(export).map_err(nfs_context_error);
        }
        let (parent, name) = self.parent(export, path)?;
        let before = parent.symlink_metadata(&name).map_err(nfs_error)?;
        regular_type(&before)?;
        if before.is_dir() {
            let dir = parent.open_dir_nofollow(&name).map_err(nfs_error)?;
            let metadata = dir.dir_metadata().map_err(nfs_error)?;
            regular_type(&metadata)?;
            Ok((pin_directory(dir, &metadata).map_err(nfs_context_error)?, metadata))
        } else {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            #[cfg(unix)] {
                use cap_std::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NONBLOCK);
            }
            let file = parent.open_with(&name, &options).map_err(nfs_error)?;
            let metadata = file.metadata().map_err(nfs_error)?;
            regular_type(&metadata)?;
            if !metadata.is_file() { return Err(nfsstat3::NFS3ERR_INVAL); }
            Ok((Arc::new(Pinned::File(file)), metadata))
        }
    }

    fn checked(&self, state: &State, id: u64) -> NfsResult<(Node, Metadata)> {
        let node = state.nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?.clone();
        let (_, metadata) = self.observe(node.export, &node.path).map_err(|error| {
            if error == nfsstat3::NFS3ERR_NOENT { nfsstat3::NFS3ERR_STALE } else { error }
        })?;
        if identity(&metadata) != node.identity { return Err(nfsstat3::NFS3ERR_STALE); }
        Ok((node, metadata))
    }

    fn register(&self, state: &mut State, export: usize, path: PathBuf) -> NfsResult<(u64, Metadata)> {
        self.register_matching(state, export, path, None)
    }

    /// When an operation already opened an object, never substitute a newer
    /// object that a local process has put at the same name in the meantime.
    fn register_matching(&self, state: &mut State, export: usize, path: PathBuf, expected: Option<Identity>) -> NfsResult<(u64, Metadata)> {
        let (pinned, metadata) = self.observe(export, &path)?;
        self.register_observed(state, export, path, expected, pinned, metadata)
    }

    fn register_observed(&self, state: &mut State, export: usize, path: PathBuf, expected: Option<Identity>,
        pinned: Arc<Pinned>, metadata: Metadata) -> NfsResult<(u64, Metadata)> {
        if expected.is_some_and(|expected| expected != identity(&metadata)) {
            return Err(nfsstat3::NFS3ERR_STALE);
        }
        let key = (export, path_key(&path));
        if let Some(id) = state.paths.get(&key).copied() {
            if state.nodes.get(&id).is_some_and(|node| node.identity == identity(&metadata)) {
                return Ok((id, metadata));
            }
            state.nodes.remove(&id);
            state.paths.remove(&key);
        }
        if state.nodes.len() >= MAX_HANDLES { return Err(nfsstat3::NFS3ERR_JUKEBOX); }
        let id = state.next_id;
        state.next_id = state.next_id.checked_add(1).ok_or(nfsstat3::NFS3ERR_SERVERFAULT)?;
        state.nodes.insert(id, Node { export, path, identity: identity(&metadata), pinned, mode: None });
        state.paths.insert(key, id);
        Ok((id, metadata))
    }

    fn root_attr(&self) -> fattr3 {
        let time = nfs_time(self.started);
        fattr3 { type_: ftype3::NF3DIR, mode: 0o555, nlink: 2, uid: 0, gid: 0,
            size: 0, used: 0, rdev: specdata3::default(), fsid: 1, fileid: 1,
            atime: time, mtime: time, ctime: time }
    }

    fn attrs(&self, state: &State, id: u64, metadata: &Metadata) -> NfsResult<fattr3> {
        let node = state.nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let config = &self.exports[node.export].config;
        let mut mode = node.mode.unwrap_or(if metadata.is_dir() { config.dir_mode } else { config.file_mode });
        if config.read_only || metadata.permissions().readonly() { mode &= !0o222; }
        Ok(fattr3 {
            type_: if metadata.is_dir() { ftype3::NF3DIR } else { ftype3::NF3REG },
            mode, nlink: metadata.nlink().min(u64::from(u32::MAX)) as u32,
            uid: config.uid, gid: config.gid, size: metadata.len(), used: metadata.len(),
            rdev: specdata3::default(), fsid: node.export as u64 + 2, fileid: id,
            atime: nfs_time(metadata.accessed().map(|t| t.into_std()).unwrap_or(self.started)),
            mtime: nfs_time(metadata.modified().map(|t| t.into_std()).unwrap_or(self.started)),
            ctime: metadata_ctime(metadata, self.started),
        })
    }

    fn writable(&self, node: &Node) -> NfsResult<()> {
        if self.exports[node.export].config.read_only { Err(nfsstat3::NFS3ERR_ROFS) } else { Ok(()) }
    }

    /// Validate the whole attribute request before any filesystem mutation.
    fn validate_attrs(&self, export: usize, attrs: &sattr3, directory: bool) -> NfsResult<(Option<SystemTime>, Option<SystemTime>)> {
        let config = &self.exports[export].config;
        if matches!(attrs.uid, Nfs3Option::Some(uid) if uid != config.uid)
            || matches!(attrs.gid, Nfs3Option::Some(gid) if gid != config.gid) {
            return Err(nfsstat3::NFS3ERR_PERM);
        }
        let atime = attr_atime(attrs.atime)?;
        let mtime = attr_mtime(attrs.mtime)?;
        if let Nfs3Option::Some(size) = attrs.size {
            if size > MAX_FILE_SIZE { return Err(nfsstat3::NFS3ERR_FBIG); }
            if directory { return Err(nfsstat3::NFS3ERR_ISDIR); }
        }
        Ok((atime, mtime))
    }

    fn apply_attrs(&self, state: &mut State, id: u64, attrs: sattr3) -> NfsResult<fattr3> {
        let (node, metadata) = self.checked(state, id)?;
        self.writable(&node)?;
        let (atime, mtime) = self.validate_attrs(node.export, &attrs, metadata.is_dir())?;
        if metadata.is_file() && (matches!(attrs.size, Nfs3Option::Some(_)) || atime.is_some() || mtime.is_some()) {
            let file = self.open_writable(&node)?;
            if let Nfs3Option::Some(size) = attrs.size { file.set_len(size).map_err(nfs_error)?; }
            if atime.is_some() || mtime.is_some() {
                let mut times = std::fs::FileTimes::new();
                if let Some(time) = atime { times = times.set_accessed(time); }
                if let Some(time) = mtime { times = times.set_modified(time); }
                file.try_clone().map_err(nfs_error)?.into_std().set_times(times).map_err(nfs_error)?;
            }
            file.sync_all().map_err(nfs_error)?;
        } else if metadata.is_dir() && (atime.is_some() || mtime.is_some()) {
            let dir = self.open_directory(&node)?;
            let absolute = |time| SystemTimeSpec::Absolute(cap_std::time::SystemTime::from_std(time));
            dir.set_times(".", atime.map(absolute), mtime.map(absolute)).map_err(nfs_error)?;
        }
        if let Nfs3Option::Some(mode) = attrs.mode {
            state.nodes.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?.mode = Some(mode & 0o777);
        }
        let (_, after) = self.checked(state, id)?;
        self.attrs(state, id, &after)
    }

    fn open_writable(&self, node: &Node) -> NfsResult<File> {
        let (parent, name) = self.parent(node.export, &node.path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).follow(FollowSymlinks::No);
        #[cfg(unix)] {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = parent.open_with(name, &options).map_err(nfs_error)?;
        let metadata = file.metadata().map_err(nfs_error)?;
        if !metadata.is_file() { return Err(nfsstat3::NFS3ERR_ISDIR); }
        if identity(&metadata) != node.identity { return Err(nfsstat3::NFS3ERR_STALE); }
        Ok(file)
    }

    fn create_file(&self, dirid: &Handle, name: &filename3<'_>, attrs: sattr3, guarded: bool) -> NfsResult<(Handle, fattr3)> {
        let name = filename(name)?;
        let mut state = self.lock()?;
        let (node, metadata) = self.checked(&state, dirid.as_u64())?;
        self.writable(&node)?;
        if !metadata.is_dir() { return Err(nfsstat3::NFS3ERR_NOTDIR); }
        self.validate_attrs(node.export, &attrs, false)?;
        let path = node.path.join(name);
        let parent = self.open_directory(&node)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).follow(FollowSymlinks::No);
        if guarded { options.create_new(true); } else { options.create(true); }
        #[cfg(unix)] {
            use cap_std::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = parent.open_with(name, &options).map_err(nfs_error)?;
        let metadata = file.metadata().map_err(nfs_error)?;
        regular_type(&metadata)?;
        if !metadata.is_file() { return Err(nfsstat3::NFS3ERR_ISDIR); }
        file.sync_all().map_err(nfs_error)?;
        let (id, _) = self.register_matching(&mut state, node.export, path, Some(identity(&metadata)))?;
        let attr = self.apply_attrs(&mut state, id, attrs)?;
        Ok((Handle::new(id), attr))
    }

    fn create_directory(&self, dirid: &Handle, name: &filename3<'_>, attrs: sattr3) -> NfsResult<(Handle, fattr3)> {
        let name = filename(name)?;
        let mut state = self.lock()?;
        let (node, meta) = self.checked(&state, dirid.as_u64())?;
        self.writable(&node)?;
        if !meta.is_dir() { return Err(nfsstat3::NFS3ERR_NOTDIR); }
        self.validate_attrs(node.export, &attrs, true)?;
        let path = node.path.join(name);
        let parent = self.open_directory(&node)?;
        parent.create_dir(name).map_err(nfs_error)?;
        let created = parent.open_dir_nofollow(name).map_err(nfs_error)?;
        let metadata = created.dir_metadata().map_err(nfs_error)?;
        regular_type(&metadata)?;
        let (id, _) = self.register_matching(&mut state, node.export, path, Some(identity(&metadata)))?;
        let attr = self.apply_attrs(&mut state, id, attrs)?;
        Ok((Handle::new(id), attr))
    }

    fn remove_typed(&self, dirid: u64, name: &filename3<'_>, directory: bool) -> NfsResult<()> {
        let name = filename(name)?;
        let mut state = self.lock()?;
        let (parent_node, parent_meta) = self.checked(&state, dirid)?;
        self.writable(&parent_node)?;
        if !parent_meta.is_dir() { return Err(nfsstat3::NFS3ERR_NOTDIR); }
        let parent = self.open_directory(&parent_node)?;
        let path = parent_node.path.join(name);
        let (object, meta) = self.register(&mut state, parent_node.export, path.clone())?;
        if directory && !meta.is_dir() { return Err(nfsstat3::NFS3ERR_NOTDIR); }
        if !directory && meta.is_dir() { return Err(nfsstat3::NFS3ERR_ISDIR); }
        if directory { parent.remove_dir(name) } else { parent.remove_file(name) }.map_err(nfs_error)?;
        state.nodes.remove(&object);
        state.paths.remove(&(parent_node.export, path_key(&path)));
        Ok(())
    }
}

pub struct DirectoryIterator { entries: Arc<Vec<DirEntryPlus<Handle>>>, position: usize }
impl ReadDirPlusIterator<Handle> for DirectoryIterator {
    async fn next(&mut self) -> NextResult<DirEntryPlus<Handle>> {
        match self.entries.get(self.position) {
            Some(entry) => { self.position += 1; NextResult::Ok(entry.clone()) }
            None => NextResult::Eof,
        }
    }
}

impl NfsReadFileSystem for LocalFs {
    type Handle = Handle;
    fn root_dir(&self) -> Handle { Handle::new(1) }

    async fn lookup(&self, dirid: &Handle, name: &filename3<'_>) -> NfsResult<Handle> {
        let text = std::str::from_utf8(name.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let mut state = self.lock()?;
        if dirid.as_u64() == 1 {
            if text == "." || text == ".." { return Ok(*dirid); }
            validate_name(text)?;
            let index = self.exports.iter().position(|export| export.config.name == text).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            return self.register(&mut state, index, PathBuf::new()).map(|(id, _)| Handle::new(id));
        }
        let (dir, metadata) = self.checked(&state, dirid.as_u64())?;
        if !metadata.is_dir() { return Err(nfsstat3::NFS3ERR_NOTDIR); }
        let path = if text == "." { dir.path.clone() }
            else if text == ".." { dir.path.parent().unwrap_or(Path::new("")).to_owned() }
            else { validate_name(text)?; dir.path.join(text) };
        self.register(&mut state, dir.export, path).map(|(id, _)| Handle::new(id))
    }

    async fn getattr(&self, id: &Handle) -> NfsResult<fattr3> {
        if id.as_u64() == 1 { return Ok(self.root_attr()); }
        let state = self.lock()?;
        let (_, meta) = self.checked(&state, id.as_u64())?;
        self.attrs(&state, id.as_u64(), &meta)
    }

    async fn read(&self, id: &Handle, offset: u64, count: u32) -> NfsResult<(Vec<u8>, bool)> {
        let state = self.lock()?;
        let (node, metadata) = self.checked(&state, id.as_u64())?;
        let Pinned::File(file) = node.pinned.as_ref() else { return Err(nfsstat3::NFS3ERR_ISDIR); };
        if offset >= metadata.len() { return Ok((Vec::new(), true)); }
        let mut file = file.try_clone().map_err(nfs_error)?;
        file.seek(SeekFrom::Start(offset)).map_err(nfs_error)?;
        let size = u64::from(count.min(MAX_IO)).min(metadata.len() - offset) as usize;
        let mut data = vec![0; size];
        let mut length = 0;
        while length < size {
            match file.read(&mut data[length..]) {
                Ok(0) => break,
                Ok(read) => length += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(nfs_error(error)),
            }
        }
        data.truncate(length);
        let eof = offset + length as u64 >= file.metadata().map_err(nfs_error)?.len();
        Ok((data, eof))
    }

    async fn readdirplus(&self, dirid: &Handle, cookie: u64) -> NfsResult<impl ReadDirPlusIterator<Handle>> {
        let mut state = self.lock()?;
        if dirid.as_u64() != 1 {
            let (_, meta) = self.checked(&state, dirid.as_u64())?;
            if !meta.is_dir() { return Err(nfsstat3::NFS3ERR_NOTDIR); }
        }
        state.snapshots.retain(|_, snapshot| snapshot.created.elapsed() < Duration::from_secs(120));
        if cookie != 0 {
            let snapshot_id = (cookie >> 32) as u32;
            let position = (cookie & 0xFFFF_FFFF) as usize;
            let snapshot = state.snapshots.get(&snapshot_id).ok_or(nfsstat3::NFS3ERR_BAD_COOKIE)?;
            if snapshot.dir != dirid.as_u64() || position > snapshot.entries.len() {
                return Err(nfsstat3::NFS3ERR_BAD_COOKIE);
            }
            return Ok(DirectoryIterator { entries: snapshot.entries.clone(), position });
        }
        while state.snapshots.len() >= MAX_SNAPSHOTS {
            let Some(oldest) = state.snapshots.iter().min_by_key(|(_, s)| s.created).map(|(id, _)| *id) else { break; };
            state.snapshots.remove(&oldest);
        }
        let snapshot_id = state.next_snapshot;
        state.next_snapshot = state.next_snapshot.checked_add(1).ok_or(nfsstat3::NFS3ERR_SERVERFAULT)?;
        let mut names = Vec::new();
        let node = if dirid.as_u64() == 1 { None } else { Some(self.checked(&state, dirid.as_u64())?.0) };
        let directory = node.as_ref().map(|node| self.open_directory(node)).transpose()?;
        if let Some(ref dir) = directory {
            for entry in dir.entries().map_err(nfs_error)? {
                let entry = entry.map_err(nfs_error)?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue; };
                if validate_name(&name).is_ok() { names.push(name); }
                if names.len() > MAX_DIR_ENTRIES { return Err(nfsstat3::NFS3ERR_JUKEBOX); }
            }
        } else {
            names.extend(self.exports.iter().map(|export| export.config.name.clone()));
        }
        names.sort();
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            let (export, path) = match &node {
                Some(node) => (node.export, node.path.join(&name)),
                None => (self.exports.iter().position(|e| e.config.name == name).ok_or(nfsstat3::NFS3ERR_NOENT)?, PathBuf::new()),
            };
            let (id, metadata) = match self.register(&mut state, export, path) {
                Ok(result) => result,
                Err(nfsstat3::NFS3ERR_NOTSUPP | nfsstat3::NFS3ERR_ACCES | nfsstat3::NFS3ERR_NOENT) => continue,
                Err(error) => return Err(error),
            };
            entries.push(DirEntryPlus {
                fileid: id, name: name.into_bytes().into(),
                cookie: (u64::from(snapshot_id) << 32) | (entries.len() + 1) as u64,
                name_attributes: Some(self.attrs(&state, id, &metadata)?), name_handle: Some(Handle::new(id)),
            });
        }
        let entries = Arc::new(entries);
        state.snapshots.insert(snapshot_id, Snapshot { dir: dirid.as_u64(), created: Instant::now(), entries: entries.clone() });
        Ok(DirectoryIterator { entries, position: 0 })
    }

    async fn readlink(&self, _id: &Handle) -> NfsResult<nfspath3<'_>> { Err(nfsstat3::NFS3ERR_NOTSUPP) }

    async fn fsinfo(&self, id: &Handle) -> NfsResult<FSINFO3resok> {
        Ok(FSINFO3resok {
            obj_attributes: post_op_attr::Some(self.getattr(id).await?),
            rtmax: MAX_IO, rtpref: MAX_IO, rtmult: 4096,
            wtmax: MAX_IO, wtpref: MAX_IO, wtmult: 4096,
            dtpref: 8192, maxfilesize: MAX_FILE_SIZE,
            time_delta: nfstime3 { seconds: 0, nseconds: 1_000_000 },
            properties: FSF3_CANSETTIME,
        })
    }

    async fn fsstat(&self, id: &Handle) -> NfsResult<FSSTAT3resok> {
        let state = self.lock()?;
        let (export, attr) = if id.as_u64() == 1 { (0, self.root_attr()) } else {
            let (node, metadata) = self.checked(&state, id.as_u64())?;
            (node.export, self.attrs(&state, id.as_u64(), &metadata)?)
        };
        filesystem_stats(&self.exports[export], attr)
    }
}

impl NfsFileSystem for LocalFs {
    fn capabilities(&self) -> VFSCapabilities { VFSCapabilities::ReadWrite }

    async fn setattr(&self, id: &Handle, attrs: sattr3) -> NfsResult<fattr3> {
        if id.as_u64() == 1 { return Err(nfsstat3::NFS3ERR_ROFS); }
        let mut state = self.lock()?;
        self.apply_attrs(&mut state, id.as_u64(), attrs)
    }

    async fn write(&self, id: &Handle, offset: u64, data: &[u8], _stable: stable_how) -> NfsResult<(fattr3, stable_how)> {
        if data.len() > MAX_IO as usize { return Err(nfsstat3::NFS3ERR_INVAL); }
        if offset.checked_add(data.len() as u64).is_none_or(|end| end > MAX_FILE_SIZE) { return Err(nfsstat3::NFS3ERR_FBIG); }
        let state = self.lock()?;
        let (node, _) = self.checked(&state, id.as_u64())?;
        self.writable(&node)?;
        let mut file = self.open_writable(&node)?;
        file.seek(SeekFrom::Start(offset)).map_err(nfs_error)?;
        file.write_all(data).map_err(nfs_error)?;
        file.sync_all().map_err(nfs_error)?;
        let metadata = file.metadata().map_err(nfs_error)?;
        Ok((self.attrs(&state, id.as_u64(), &metadata)?, stable_how::FILE_SYNC))
    }

    async fn create(&self, dirid: &Handle, name: &filename3<'_>, attrs: sattr3) -> NfsResult<(Handle, fattr3)> {
        self.create_file(dirid, name, attrs, false)
    }

    async fn create_exclusive(&self, _dirid: &Handle, _name: &filename3<'_>, _verifier: createverf3) -> NfsResult<Handle> {
        // RFC 1813 explicitly permits NOTSUPP if the verifier cannot be durably stored.
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn create_guarded(&self, dirid: &Handle, name: &filename3<'_>, attrs: sattr3) -> NfsResult<(Handle, fattr3)> {
        self.create_file(dirid, name, attrs, true)
    }

    async fn mkdir(&self, dirid: &Handle, name: &filename3<'_>) -> NfsResult<(Handle, fattr3)> {
        self.create_directory(dirid, name, sattr3::default())
    }

    async fn mkdir_with_attrs(&self, dirid: &Handle, name: &filename3<'_>, attrs: sattr3) -> NfsResult<(Handle, fattr3)> {
        self.create_directory(dirid, name, attrs)
    }

    async fn remove(&self, dirid: &Handle, name: &filename3<'_>) -> NfsResult<()> {
        self.remove_typed(dirid.as_u64(), name, false)
    }

    async fn rmdir(&self, dirid: &Handle, name: &filename3<'_>) -> NfsResult<()> {
        self.remove_typed(dirid.as_u64(), name, true)
    }

    async fn rename<'a>(&self, from_dirid: &Handle, from_name: &filename3<'a>, to_dirid: &Handle, to_name: &filename3<'a>) -> NfsResult<()> {
        let from_name = filename(from_name)?;
        let to_name = filename(to_name)?;
        let mut state = self.lock()?;
        let (from_dir, from_meta) = self.checked(&state, from_dirid.as_u64())?;
        let (to_dir, to_meta) = self.checked(&state, to_dirid.as_u64())?;
        self.writable(&from_dir)?;
        self.writable(&to_dir)?;
        if from_dir.export != to_dir.export { return Err(nfsstat3::NFS3ERR_XDEV); }
        if !from_meta.is_dir() || !to_meta.is_dir() { return Err(nfsstat3::NFS3ERR_NOTDIR); }
        let from_parent = self.open_directory(&from_dir)?;
        let to_parent = self.open_directory(&to_dir)?;
        let from_path = from_dir.path.join(from_name);
        let to_path = to_dir.path.join(to_name);
        let (_, source_meta) = self.register(&mut state, from_dir.export, from_path.clone())?;
        let from_key = path_key(&from_path);
        let to_key = path_key(&to_path);
        if from_key == to_key && from_path == to_path { return Ok(()); }
        if let Ok((_, destination_meta)) = self.observe(to_dir.export, &to_path) {
            if source_meta.is_dir() != destination_meta.is_dir() {
                return Err(if source_meta.is_dir() { nfsstat3::NFS3ERR_NOTDIR } else { nfsstat3::NFS3ERR_ISDIR });
            }
        }
        from_parent.rename(from_name, &to_parent, to_name).map_err(nfs_error)?;
        let prefix_components = from_path.components().count();
        let changed = state.nodes.iter().filter_map(|(id, node)| {
            (node.export == from_dir.export && path_key(&node.path).starts_with(&from_key))
                .then(|| (*id, node.path.clone(), to_path.join(node.path.components().skip(prefix_components).collect::<PathBuf>())))
        }).collect::<Vec<_>>();
        let overwritten = state.nodes.iter().filter(|(id, node)| {
            node.export == to_dir.export && path_key(&node.path).starts_with(&to_key) && !changed.iter().any(|(changed_id, _, _)| changed_id == *id)
        }).map(|(id, _)| *id).collect::<Vec<_>>();
        for id in overwritten {
            if let Some(node) = state.nodes.remove(&id) { state.paths.remove(&(node.export, path_key(&node.path))); }
        }
        for (id, old, new) in changed {
            state.paths.remove(&(from_dir.export, path_key(&old)));
            if let Some(node) = state.nodes.get_mut(&id) { node.path = new.clone(); }
            state.paths.insert((from_dir.export, path_key(&new)), id);
        }
        Ok(())
    }

    async fn symlink<'a>(&self, _dirid: &Handle, _name: &filename3<'a>, _target: &nfspath3<'a>, _attrs: &sattr3) -> NfsResult<(Handle, fattr3)> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn commit(&self, id: &Handle, _offset: u64, _count: u32) -> NfsResult<()> {
        let state = self.lock()?;
        let (node, metadata) = self.checked(&state, id.as_u64())?;
        self.writable(&node)?;
        if !metadata.is_file() { return Err(nfsstat3::NFS3ERR_INVAL); }
        self.open_writable(&node)?.sync_all().map_err(nfs_error)
    }
}

/// Keeps an object identity alive without preventing directory rename/removal.
/// Windows ReOpenFile cannot open directories. Reopen the OS-resolved path
/// with CreateFile semantics, then check that it is still the same directory.
/// The original cap-std Dir stays alive throughout, and the metadata-only pin
/// is never used for traversal or filesystem operations.
fn pin_directory(dir: Dir, _metadata: &Metadata) -> Result<Arc<Pinned>> {
    #[cfg(windows)]
    let pin = {
        use std::os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle};
        use cap_std::fs::MetadataExt as WindowsMetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFinalPathNameByHandleW, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FILE_SHARE_DELETE, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
        };
        let mut path = vec![0u16; 512];
        let path = loop {
            // SAFETY: dir owns the source handle, and path is a writable UTF-16
            // buffer of the size passed. The handle remains alive below too.
            let length = unsafe { GetFinalPathNameByHandleW(dir.as_raw_handle(),
                path.as_mut_ptr(), path.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS) } as usize;
            if length == 0 {
                return Err(io::Error::last_os_error()).context("GetFinalPathNameByHandleW 获取目录路径失败");
            }
            if length < path.len() {
                break std::ffi::OsString::from_wide(&path[..length]);
            }
            if length > 65536 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Windows 返回的目录路径过长").into());
            }
            // An insufficient-buffer result includes the terminating NUL.
            path.resize(length, 0);
        };
        let pin = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path).context("CreateFileW 打开目录属性句柄失败")?;
        let pinned_metadata = Metadata::from_file(&pin).context("读取目录属性句柄失败")?;
        if !pinned_metadata.is_dir()
            || WindowsMetadataExt::file_attributes(&pinned_metadata) & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || identity(&pinned_metadata) != identity(_metadata) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "目录在打开过程中发生变化，请重试").into());
        }
        // Release the no-delete-sharing capability handle only after checking
        // the new handle. The new handle alone must not lock directory names.
        drop(dir);
        pin
    };
    #[cfg(not(windows))]
    let pin = dir;
    Ok(Arc::new(Pinned::Directory { _pin: pin }))
}

fn identity(metadata: &Metadata) -> Identity { Identity { dev: metadata.dev(), ino: metadata.ino() } }
fn path_key(path: &Path) -> PathBuf {
    #[cfg(windows)] { PathBuf::from(path.to_string_lossy().to_lowercase()) }
    #[cfg(not(windows))] { path.to_owned() }
}

fn regular_type(metadata: &Metadata) -> NfsResult<()> {
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        return Err(nfsstat3::NFS3ERR_NOTSUPP);
    }
    #[cfg(windows)] {
        use cap_std::fs::MetadataExt as WindowsMetadataExt;
        if WindowsMetadataExt::file_attributes(metadata) & 0x400 != 0 { return Err(nfsstat3::NFS3ERR_ACCES); }
    }
    Ok(())
}

fn filename<'a>(name: &'a filename3<'_>) -> NfsResult<&'a str> {
    let name = std::str::from_utf8(name.as_ref()).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
    validate_name(name)?;
    Ok(name)
}

pub fn validate_name(name: &str) -> NfsResult<()> {
    if name.is_empty() || name == "." || name == ".." { return Err(nfsstat3::NFS3ERR_ACCES); }
    if name.encode_utf16().count() > 255 || name.len() > 255 { return Err(nfsstat3::NFS3ERR_NAMETOOLONG); }
    if name.ends_with(['.', ' ']) || name.chars().any(|c| c < ' ' || "\\/:*?\"<>|".contains(c)) {
        return Err(nfsstat3::NFS3ERR_ACCES);
    }
    let base = name.split('.').next().unwrap_or("").to_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&base.as_str())
        || ((base.starts_with("COM") || base.starts_with("LPT")) &&
            base[3..].chars().count() == 1 && "123456789¹²³".contains(&base[3..])) {
        return Err(nfsstat3::NFS3ERR_ACCES);
    }
    Ok(())
}

fn nfs_time(time: SystemTime) -> nfstime3 {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    nfstime3 { seconds: duration.as_secs().min(u64::from(u32::MAX)) as u32, nseconds: duration.subsec_nanos() }
}

fn metadata_ctime(metadata: &Metadata, _fallback: SystemTime) -> nfstime3 {
    #[cfg(unix)] {
        use cap_std::fs::MetadataExt as UnixMetadataExt;
        nfstime3 { seconds: UnixMetadataExt::ctime(metadata).clamp(0, i64::from(u32::MAX)) as u32,
            nseconds: UnixMetadataExt::ctime_nsec(metadata).clamp(0, 999_999_999) as u32 }
    }
    #[cfg(not(unix))] { nfs_time(metadata.modified().map(|t| t.into_std()).unwrap_or(_fallback)) }
}

fn client_time(time: nfstime3) -> NfsResult<SystemTime> {
    if time.nseconds >= 1_000_000_000 { return Err(nfsstat3::NFS3ERR_INVAL); }
    Ok(UNIX_EPOCH + Duration::new(u64::from(time.seconds), time.nseconds))
}
fn attr_atime(time: set_atime) -> NfsResult<Option<SystemTime>> {
    match time { set_atime::DONT_CHANGE => Ok(None), set_atime::SET_TO_SERVER_TIME => Ok(Some(SystemTime::now())),
        set_atime::SET_TO_CLIENT_TIME(time) => client_time(time).map(Some) }
}
fn attr_mtime(time: set_mtime) -> NfsResult<Option<SystemTime>> {
    match time { set_mtime::DONT_CHANGE => Ok(None), set_mtime::SET_TO_SERVER_TIME => Ok(Some(SystemTime::now())),
        set_mtime::SET_TO_CLIENT_TIME(time) => client_time(time).map(Some) }
}

fn nfs_error(error: io::Error) -> nfsstat3 {
    nfs_error_kind(error.kind())
}

fn nfs_context_error(error: anyhow::Error) -> nfsstat3 {
    error.downcast_ref::<io::Error>().map(|error| nfs_error_kind(error.kind()))
        .unwrap_or(nfsstat3::NFS3ERR_SERVERFAULT)
}

fn nfs_error_kind(kind: io::ErrorKind) -> nfsstat3 {
    use io::ErrorKind::*;
    match kind {
        NotFound => nfsstat3::NFS3ERR_NOENT, PermissionDenied => nfsstat3::NFS3ERR_ACCES,
        AlreadyExists => nfsstat3::NFS3ERR_EXIST, NotADirectory => nfsstat3::NFS3ERR_NOTDIR,
        IsADirectory => nfsstat3::NFS3ERR_ISDIR, DirectoryNotEmpty => nfsstat3::NFS3ERR_NOTEMPTY,
        InvalidInput | InvalidData => nfsstat3::NFS3ERR_INVAL, StorageFull => nfsstat3::NFS3ERR_NOSPC,
        ReadOnlyFilesystem => nfsstat3::NFS3ERR_ROFS, FileTooLarge => nfsstat3::NFS3ERR_FBIG,
        Unsupported => nfsstat3::NFS3ERR_NOTSUPP, _ => nfsstat3::NFS3ERR_IO,
    }
}

#[cfg(unix)]
fn filesystem_stats(export: &Export, attr: fattr3) -> NfsResult<FSSTAT3resok> {
    use std::os::fd::AsRawFd;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: fstatvfs writes one statvfs to the initialized output allocation.
    if unsafe { libc::fstatvfs(export.dir.as_raw_fd(), stats.as_mut_ptr()) } != 0 { return Err(nfs_error(io::Error::last_os_error())); }
    let stats = unsafe { stats.assume_init() };
    Ok(FSSTAT3resok { obj_attributes: post_op_attr::Some(attr),
        tbytes: stats.f_blocks.saturating_mul(stats.f_frsize), fbytes: stats.f_bfree.saturating_mul(stats.f_frsize),
        abytes: stats.f_bavail.saturating_mul(stats.f_frsize), tfiles: stats.f_files,
        ffiles: stats.f_ffree, afiles: stats.f_favail, invarsec: 0 })
}

#[cfg(windows)]
fn filesystem_stats(export: &Export, attr: fattr3) -> NfsResult<FSSTAT3resok> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let path: Vec<u16> = std::ffi::OsStr::new(&export.config.path).encode_wide().chain(Some(0)).collect();
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: NUL-terminated trusted configuration path and valid u64 out-pointers.
    if unsafe { GetDiskFreeSpaceExW(path.as_ptr(), &mut available, &mut total, &mut free) } == 0 {
        return Err(nfs_error(io::Error::last_os_error()));
    }
    Ok(FSSTAT3resok { obj_attributes: post_op_attr::Some(attr), tbytes: total, fbytes: free, abytes: available,
        tfiles: 0, ffiles: 0, afiles: 0, invarsec: 0 })
}
