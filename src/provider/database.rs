use super::DbEntry;
use crate::{
    rpc_protocol::ValidateProgress,
    util::{validate_bao, BaoValidationError},
    Hash,
};
use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use futures::{
    Future, Stream, StreamExt, FutureExt, future::LocalBoxFuture,
};
use std::{
    collections::{BTreeSet, HashMap},
    fmt, io::{self, SeekFrom},
    path::{Path, PathBuf},
    result,
    sync::{Arc, RwLock},
};
use tokio::{sync::mpsc, io::{AsyncSeekExt, AsyncReadExt}};

trait ReadSlice {
    type ReadAtFuture<'a>: Future<Output = io::Result<()>> + 'a
    where
        Self: 'a;
    fn read_at<'a>(&'a mut self, offset: u64, buf: &'a mut [u8]) -> Self::ReadAtFuture<'_>;
    type LenFuture<'a>: Future<Output = io::Result<u64>> + 'a
    where
        Self: 'a;
    fn len(&mut self) -> Self::LenFuture<'_>;
}

fn slice_read_at(slice: impl AsRef<[u8]>, offset: u64, buf: &mut [u8]) -> Option<()> {
    let bytes = slice.as_ref();
    let start: usize = offset.try_into().ok()?;
    let end = start.checked_add(buf.len())?;
    let len = bytes.len();
    if end <= len {
        buf.copy_from_slice(&bytes[start..end]);
        Some(())
    } else {
        None
    }
}

impl ReadSlice for tokio::fs::File {
    type ReadAtFuture<'a> = LocalBoxFuture<'a, io::Result<()>>;
    fn read_at<'a>(&'a mut self, offset: u64, buf: &'a mut [u8]) -> Self::ReadAtFuture<'a> {
        async move {
            self.seek(SeekFrom::Start(offset)).await?;
            self.read_exact(buf).await?;
            Ok(())
        }.boxed_local()
    }
    type LenFuture<'a> = LocalBoxFuture<'a, io::Result<u64>>;
    fn len(&mut self) -> Self::LenFuture<'_> {
        async move {
            let metadata = self.metadata().await?;
            Ok(metadata.len())
        }.boxed_local()
    }
}

impl ReadSlice for Bytes {
    type ReadAtFuture<'a> = futures::future::Ready<io::Result<()>>;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Self::ReadAtFuture<'_> {
        let res = slice_read_at(&self, offset, buf).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "offset {} and len {} is out of bounds for bytes of len {}",
                    offset,
                    buf.len(),
                    Bytes::len(self)
                ),
            )
        });
        futures::future::ready(res)
    }
    type LenFuture<'a> = futures::future::Ready<io::Result<u64>>;
    fn len(&mut self) -> Self::LenFuture<'_> {
        futures::future::ready(Ok(Bytes::len(self) as u64))
    }
}

impl ReadSlice for BytesMut {
    type ReadAtFuture<'a> = futures::future::Ready<io::Result<()>>;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Self::ReadAtFuture<'_> {
        let res = slice_read_at(&self, offset, buf).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "offset {} and len {} is out of bounds for bytes of len {}",
                    offset,
                    buf.len(),
                    BytesMut::len(self)
                ),
            )
        });
        futures::future::ready(res)
    }
    type LenFuture<'a> = futures::future::Ready<io::Result<u64>>;
    fn len(&mut self) -> Self::LenFuture<'_> {
        futures::future::ready(Ok(BytesMut::len(self) as u64))
    }
}

trait WriteSlice {
    type WriteSliceFuture<'a>: Future<Output = io::Result<()>> + 'a
    where
        Self: 'a;
    fn write_at(&mut self, offset: u64, buffer: &[u8]) -> Self::WriteSliceFuture<'_>;

    type TruncateFuture<'a>: Future<Output = io::Result<()>> + 'a
    where
        Self: 'a;
    fn truncate(&mut self, size: u64) -> Self::TruncateFuture<'_>;
}

fn bytes_mut_write_at(this: &mut BytesMut, offset: u64, buf: &[u8]) -> Option<()> {
    let start: usize = offset.try_into().ok()?;
    let end = start.checked_add(buf.len())?;
    let len = BytesMut::len(this);
    if end > len {
        // add to the end
        this.resize(start, 0);
        this.extend_from_slice(buf);
    } else {
        // modify existing buffer
        this[start..end].copy_from_slice(buf);
    }
    Some(())
}

impl WriteSlice for BytesMut {
    type WriteSliceFuture<'a> = futures::future::Ready<io::Result<()>>;

    fn write_at(&mut self, offset: u64, buffer: &[u8]) -> Self::WriteSliceFuture<'_> {
        let res = bytes_mut_write_at(self, offset, buffer).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "offset {} and len {} is out of bounds for bytes of len {}",
                    offset,
                    buffer.len(),
                    BytesMut::len(self)
                ),
            )
        });
        futures::future::ready(res)
    }

    type TruncateFuture<'a> = futures::future::Ready<io::Result<()>>;

    fn truncate(&mut self, size: u64) -> Self::TruncateFuture<'_> {
        // if size is > usize::MAX there is nothing to do
        if let Ok(size) = size.try_into() {
            self.truncate(size);
        }
        futures::future::ready(Ok(()))
    }
}

trait VFS {
    type Id: Send + Sync + 'static;
    type ReadRaw: ReadSlice + Unpin + 'static;
    type WriteRaw: ReadSlice + WriteSlice + Unpin + 'static;
    type ResultIterator: Iterator<Item = io::Result<Self::Id>> + Send + Sync + 'static;
    /// create a handle for internal data
    ///
    /// `name_hint` is a hint for the internal name (base).
    /// `purpose` can also be used as a hint for the internal name (extension).
    fn create(&self, name_hint: &[u8], purpose: Purpose) -> io::Result<Self::Id>;
    /// open an internal handle for reading
    fn open_read(&self, handle: Self::Id) -> io::Result<Self::ReadRaw>;
    /// open an internal handle for writing
    fn open_write(&self, handle: Self::Id) -> io::Result<Self::WriteRaw>;
    /// delete an internal handle
    fn delete(&self, handle: Self::Id) -> io::Result<()>;
    /// create a snapshot of the vfs and return an iterator over it
    fn enumerate(&self) -> Self::ResultIterator;
}

trait ResourceLoader {
    /// A stable resource identifier, like a path or an url
    type Id: Send + Sync + 'static;
    /// type of an open resouce
    type ReadRaw: ReadSlice + Unpin + 'static;
    /// open a resource
    fn open(&self, handle: Self::Id) -> Result<Self::ReadRaw>;
}

enum Purpose {
    /// File is going to be used to store data
    Data,
    /// File is going to be used to store a bao outboard
    Outboard,
    /// File is going to be used to store metadata
    Meta,
}

type VfsId<X> = <X as VFS>::Id;
type ExId<D> = <<D as AbstractDatabase>::External as ResourceLoader>::Id;
type InId<D> = <<D as AbstractDatabase>::Internal as VFS>::Id;

enum AdbId<D: AbstractDatabase> {
    Internal(InId<D>),
    External(ExId<D>),
}

struct AdbEntry<D: AbstractDatabase> {
    outboard: AdbId<D>,
    data: AdbId<D>,
}

trait AbstractDatabase: Sized {
    /// The type of the internal VFS
    type Internal: VFS;
    /// The type of the external resource loader
    type External: ResourceLoader;
    /// The type of the temporary pin
    type TempPin;
    /// The type of the future returned by `get`
    type GetFuture<'a>: Future<Output = io::Result<Option<AdbEntry<Self>>>> + 'a
    where
        Self: 'a;
    /// The type of the future returned by `insert`
    type InsertFuture<'a>: Future<Output = io::Result<Hash>> + 'a
    where
        Self: 'a;
    /// The type of the future returned by `pin`
    type PinFuture<'a>: Future<Output = io::Result<()>> + 'a
    where
        Self: 'a;
    /// The type of the stream returned by `blobs`
    type BlobStream<'a>: Stream<Item = io::Result<Hash>> + 'a
    where
        Self: 'a;
    /// The type of the stream returned by `pins`
    type PinStream<'a>: Stream<Item = io::Result<Vec<u8>>> + 'a
    where
        Self: 'a;
    /// Create a new database or open an existing one.
    ///
    /// `name` is the name of the database. If `None`, a new database will be
    /// created. Otherwise, the database will be opened.
    /// `internal` is the internal VFS.
    /// `external` is the external resource loader.
    fn new(
        name: Option<InId<Self>>,
        internal: Self::Internal,
        external: Self::External,
    ) -> (Self, InId<Self>);
    /// get the data and outboard for a given hash, if it exists
    fn get(&self, key: &Hash) -> Self::GetFuture<'_>;
    /// insert a new data and outboard pair
    ///
    /// if `outboard` is `None`, it will be generated from `data`. Otherwise it
    /// will be assumed to match data.
    fn insert(&self, data: AdbId<Self>, outboard: Option<AdbId<Self>>) -> Self::InsertFuture<'_>;
    /// Enumerate all blobs in the database.
    fn blobs(&self) -> Self::BlobStream<'_>;
    /// Pin a blob in the database.
    fn pin(&self, name: &[u8], hash: Option<&Hash>) -> Self::PinFuture<'_>;
    /// Enumerate all pinned hashes in the database.
    fn pins(&self) -> Self::PinStream<'_>;
}

/// File name of directory inside `IROH_DATA_DIR` where outboards are stored.
const FNAME_OUTBOARDS: &str = "outboards";

/// File name of directory inside `IROH_DATA_DIR` where collections are stored.
///
/// This is now used not just for collections but also for internally generated blobs.
const FNAME_COLLECTIONS: &str = "collections";

/// File name inside `IROH_DATA_DIR` where paths to data are stored.
pub const FNAME_PATHS: &str = "paths.bin";

/// Database containing content-addressed data (blobs or collections).
#[derive(Debug, Clone, Default)]
pub struct Database(Arc<RwLock<HashMap<Hash, DbEntry>>>);

impl From<HashMap<Hash, DbEntry>> for Database {
    fn from(map: HashMap<Hash, DbEntry>) -> Self {
        Self(Arc::new(RwLock::new(map)))
    }
}

/// A snapshot of the database.
///
/// `E` can be `Infallible` if we take a snapshot from an in memory database,
/// or `io::Error` if we read a database from disk.
pub(crate) struct Snapshot<E> {
    /// list of paths we have, hash is the hash of the blob or collection
    paths: Box<dyn Iterator<Item = (Hash, u64, Option<PathBuf>)>>,
    /// map of hash to outboard, hash is the hash of the outboard and is unique
    outboards: Box<dyn Iterator<Item = result::Result<(Hash, Bytes), E>>>,
    /// map of hash to collection, hash is the hash of the collection and is unique
    collections: Box<dyn Iterator<Item = result::Result<(Hash, Bytes), E>>>,
}

impl<E> fmt::Debug for Snapshot<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot").finish()
    }
}

/// An error that can never happen
#[derive(Debug)]
pub enum NoError {}

impl From<NoError> for io::Error {
    fn from(_: NoError) -> Self {
        unreachable!()
    }
}

struct DataPaths {
    #[allow(dead_code)]
    data_dir: PathBuf,
    outboards_dir: PathBuf,
    collections_dir: PathBuf,
    paths_file: PathBuf,
}

impl DataPaths {
    fn new(data_dir: PathBuf) -> Self {
        Self {
            outboards_dir: data_dir.join(FNAME_OUTBOARDS),
            collections_dir: data_dir.join(FNAME_COLLECTIONS),
            paths_file: data_dir.join(FNAME_PATHS),
            data_dir,
        }
    }
}

/// Using base64 you have all those weird characters like + and /.
/// So we use hex for file names.
fn format_hash(hash: &Hash) -> String {
    hex::encode(hash.as_ref())
}

/// Parse a hash from a string, e.g. a file name.
fn parse_hash(hash: &str) -> Result<Hash> {
    let hash = hex::decode(hash)?;
    let hash: [u8; 32] = hash.try_into().ok().context("wrong size for hash")?;
    Ok(Hash::from(hash))
}

impl Snapshot<io::Error> {
    /// Load a snapshot from disk.
    pub fn load(data_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        use std::fs;
        let DataPaths {
            outboards_dir,
            collections_dir,
            paths_file,
            ..
        } = DataPaths::new(data_dir.as_ref().to_path_buf());
        let paths = fs::read(&paths_file)
            .with_context(|| format!("Failed reading {}", paths_file.display()))?;
        let paths = postcard::from_bytes::<Vec<(Hash, u64, Option<PathBuf>)>>(&paths)?;
        let hashes = paths
            .iter()
            .map(|(hash, _, _)| *hash)
            .collect::<BTreeSet<_>>();
        let outboards = hashes.clone().into_iter().map(move |hash| {
            let path = outboards_dir.join(format_hash(&hash));
            fs::read(path).map(|x| (hash, Bytes::from(x)))
        });
        let collections = fs::read_dir(&collections_dir)
            .with_context(|| {
                format!(
                    "Failed reading collections directory {}",
                    collections_dir.display()
                )
            })?
            .map(move |entry| {
                let entry = entry?;
                let path = entry.path();
                // skip directories
                if entry.file_type()?.is_dir() {
                    tracing::debug!("skipping directory: {:?}", path);
                    return Ok(None);
                }
                // try to get the file name as an OsStr
                let name = if let Some(name) = path.file_name() {
                    name
                } else {
                    tracing::debug!("skipping unexpected path: {:?}", path);
                    return Ok(None);
                };
                // try to convert into a std str
                let name = if let Some(name) = name.to_str() {
                    name
                } else {
                    tracing::debug!("skipping unexpected path: {:?}", path);
                    return Ok(None);
                };
                // try to parse the file name as a hash
                let hash = match parse_hash(name) {
                    Ok(hash) => hash,
                    Err(err) => {
                        tracing::debug!("skipping unexpected path: {:?}: {}", path, err);
                        return Ok(None);
                    }
                };
                // skip files that are not in the paths file
                if !hashes.contains(&hash) {
                    tracing::debug!("skipping unexpected hash: {:?}", hash);
                    return Ok(None);
                }
                // read the collection data and turn it into a Bytes
                let collection = Bytes::from(fs::read(path)?);
                io::Result::Ok(Some((hash, collection)))
            })
            .filter_map(|x| x.transpose());
        Ok(Self {
            paths: Box::new(paths.into_iter()),
            outboards: Box::new(outboards),
            collections: Box::new(collections),
        })
    }
}

impl<E> Snapshot<E>
where
    io::Error: From<E>,
{
    /// Persist the snapshot to disk.
    pub fn persist(self, data_dir: impl AsRef<Path>) -> io::Result<()> {
        use std::fs;
        let DataPaths {
            outboards_dir,
            collections_dir,
            paths_file,
            ..
        } = DataPaths::new(data_dir.as_ref().to_path_buf());
        fs::create_dir_all(&data_dir)?;
        fs::create_dir_all(&outboards_dir)?;
        fs::create_dir_all(&collections_dir)?;
        for item in self.outboards {
            let (hash, outboard) = item.map_err(Into::into)?;
            let path = outboards_dir.join(format_hash(&hash));
            fs::write(path, &outboard)?;
        }
        for item in self.collections {
            let (hash, collection) = item.map_err(Into::into)?;
            let path = collections_dir.join(format_hash(&hash));
            fs::write(path, &collection)?;
        }
        let mut paths = self.paths.collect::<Vec<_>>();
        paths.sort_by_key(|(path, _, _)| *path);
        let paths_content = postcard::to_stdvec(&paths).expect("failed to serialize paths file");
        fs::write(paths_file, paths_content)?;
        Ok(())
    }
}

impl Database {
    /// Load a database from disk for testing. Synchronous.
    #[cfg(feature = "cli")]
    pub fn load_test(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        Self::load_internal(dir)
    }

    /// Save a database to disk for testing. Synchronous.
    #[cfg(feature = "cli")]
    pub fn save_test(&self, dir: impl AsRef<Path>) -> io::Result<()> {
        let dir = dir.as_ref().to_path_buf();
        self.save_internal(dir)
    }

    fn load_internal(dir: PathBuf) -> anyhow::Result<Self> {
        tracing::info!("Loading snapshot from {}...", dir.display());
        let snapshot = Snapshot::load(dir)?;
        let db = Self::from_snapshot(snapshot)?;
        tracing::info!("Database loaded");
        anyhow::Ok(db)
    }

    fn save_internal(&self, dir: PathBuf) -> io::Result<()> {
        tracing::info!("Persisting database to {}...", dir.display());
        let snapshot = self.snapshot();
        snapshot.persist(dir)?;
        tracing::info!("Database stored");
        io::Result::Ok(())
    }

    /// Load a database from disk.
    pub async fn load(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let db = tokio::task::spawn_blocking(|| Self::load_internal(dir)).await??;
        Ok(db)
    }

    /// Save a database to disk.
    pub async fn save(&self, dir: impl AsRef<Path>) -> io::Result<()> {
        let dir = dir.as_ref().to_path_buf();
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.save_internal(dir)).await??;
        Ok(())
    }

    /// Load a database from disk.
    pub(crate) fn from_snapshot<E: Into<io::Error>>(snapshot: Snapshot<E>) -> Result<Self> {
        let Snapshot {
            outboards,
            collections,
            paths,
        } = snapshot;
        let outboards = outboards
            .collect::<result::Result<HashMap<_, _>, E>>()
            .map_err(Into::into)
            .context("Failed reading outboards")?;
        let collections = collections
            .collect::<result::Result<HashMap<_, _>, E>>()
            .map_err(Into::into)
            .context("Failed reading collections")?;
        let mut db = HashMap::new();
        for (hash, size, path) in paths {
            if let (Some(path), Some(outboard)) = (path, outboards.get(&hash)) {
                db.insert(
                    hash,
                    DbEntry::External {
                        outboard: outboard.clone(),
                        path,
                        size,
                    },
                );
            }
        }
        for (hash, data) in collections {
            if let Some(outboard) = outboards.get(&hash) {
                db.insert(
                    hash,
                    DbEntry::Internal {
                        outboard: outboard.clone(),
                        data,
                    },
                );
            }
        }

        Ok(Self(Arc::new(RwLock::new(db))))
    }

    /// Validate the entire database, including collections.
    ///
    /// This works by taking a snapshot of the database, and then validating. So anything you add after this call will not be validated.
    pub(crate) async fn validate(&self, tx: mpsc::Sender<ValidateProgress>) -> anyhow::Result<()> {
        // This makes a copy of the db, but since the outboards are Bytes, it's not expensive.
        let mut data = self
            .0
            .read()
            .unwrap()
            .clone()
            .into_iter()
            .collect::<Vec<_>>();
        data.sort_by_key(|(k, e)| (e.is_external(), e.blob_path().map(ToOwned::to_owned), *k));
        tx.send(ValidateProgress::Starting {
            total: data.len() as u64,
        })
        .await?;
        futures::stream::iter(data)
            .enumerate()
            .map(|(id, (hash, boc))| {
                let id = id as u64;
                let path = if let DbEntry::External { path, .. } = &boc {
                    Some(path.clone())
                } else {
                    None
                };
                let size = boc.size();
                let entry_tx = tx.clone();
                let done_tx = tx.clone();
                async move {
                    entry_tx
                        .send(ValidateProgress::Entry {
                            id,
                            hash,
                            path: path.clone(),
                            size,
                        })
                        .await?;
                    let error = tokio::task::spawn_blocking(move || {
                        let progress_tx = entry_tx.clone();
                        let progress = |offset| {
                            progress_tx
                                .try_send(ValidateProgress::Progress { id, offset })
                                .ok();
                        };
                        let res = match boc {
                            DbEntry::External { outboard, path, .. } => {
                                match std::fs::File::open(&path) {
                                    Ok(data) => {
                                        tracing::info!("validating {}", path.display());
                                        let res = validate_bao(hash, data, outboard, progress);
                                        tracing::info!("done validating {}", path.display());
                                        res
                                    }
                                    Err(cause) => Err(BaoValidationError::from(cause)),
                                }
                            }
                            DbEntry::Internal { outboard, data } => {
                                let data = std::io::Cursor::new(data);
                                validate_bao(hash, data, outboard, progress)
                            }
                        };
                        res.err()
                    })
                    .await?;
                    let error = error.map(|x| x.to_string());
                    done_tx.send(ValidateProgress::Done { id, error }).await?;
                    anyhow::Ok(())
                }
            })
            .buffer_unordered(num_cpus::get())
            .map(|item| {
                // unwrapping is fine here, because it will only happen if the task panicked
                // basically we are just moving the panic on this task.
                item.expect("task panicked");
                Ok(())
            })
            .forward(futures::sink::drain())
            .await?;
        Ok(())
    }

    /// take a snapshot of the database
    pub(crate) fn snapshot(&self) -> Snapshot<NoError> {
        let this = self.0.read().unwrap();
        let outboards = this
            .iter()
            .map(|(k, v)| match v {
                DbEntry::External { outboard, .. } => (*k, outboard.clone()),
                DbEntry::Internal { outboard, .. } => (*k, outboard.clone()),
            })
            .collect::<Vec<_>>();

        let collections = this
            .iter()
            .filter_map(|(k, v)| match v {
                DbEntry::External { .. } => None,
                DbEntry::Internal { data, .. } => Some((*k, data.clone())),
            })
            .collect::<Vec<_>>();

        let paths = this
            .iter()
            .map(|(k, v)| match v {
                DbEntry::External { path, size, .. } => (*k, *size, Some(path.clone())),
                DbEntry::Internal { data, .. } => (*k, data.len() as u64, None),
            })
            .collect::<Vec<_>>();

        Snapshot {
            outboards: Box::new(outboards.into_iter().map(Ok)),
            collections: Box::new(collections.into_iter().map(Ok)),
            paths: Box::new(paths.into_iter()),
        }
    }

    pub(crate) fn get(&self, key: &Hash) -> Option<DbEntry> {
        self.0.read().unwrap().get(key).cloned()
    }

    pub(crate) fn union_with(&self, db: HashMap<Hash, DbEntry>) {
        let mut inner = self.0.write().unwrap();
        for (k, v) in db {
            inner.entry(k).or_insert(v);
        }
    }

    /// Iterate over all blobs that are stored externally.
    pub fn external(&self) -> impl Iterator<Item = (Hash, PathBuf, u64)> + 'static {
        let items = self
            .0
            .read()
            .unwrap()
            .iter()
            .filter_map(|(k, v)| match v {
                DbEntry::External { path, size, .. } => Some((*k, path.clone(), *size)),
                DbEntry::Internal { .. } => None,
            })
            .collect::<Vec<_>>();
        // todo: make this a proper lazy iterator at some point
        // e.g. by using an immutable map or a real database that supports snapshots.
        items.into_iter()
    }

    /// Iterate over all collections in the database.
    pub fn internal(&self) -> impl Iterator<Item = (Hash, Bytes)> + 'static {
        let items = self
            .0
            .read()
            .unwrap()
            .iter()
            .filter_map(|(hash, v)| match v {
                DbEntry::External { .. } => None,
                DbEntry::Internal { data, .. } => Some((*hash, data.clone())),
            })
            .collect::<Vec<_>>();
        // todo: make this a proper lazy iterator at some point
        // e.g. by using an immutable map or a real database that supports snapshots.
        items.into_iter()
    }

    /// Unwrap into the inner HashMap
    pub fn to_inner(&self) -> HashMap<Hash, DbEntry> {
        self.0.read().unwrap().clone()
    }
}
