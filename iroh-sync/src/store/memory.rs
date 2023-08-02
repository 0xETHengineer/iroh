//! In memory storage for replicas.

use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    sync::Arc,
};

use anyhow::Result;
use parking_lot::{RwLock, RwLockReadGuard};
use rand_core::CryptoRngCore;

use crate::{
    ranger::{AsFingerprint, Fingerprint, Range, RangeKey},
    sync::{Author, AuthorId, Namespace, NamespaceId, RecordIdentifier, Replica, SignedEntry},
};

/// Manages the replicas and authors for an instance.
#[derive(Debug, Clone, Default)]
pub struct Store {
    replicas: Arc<RwLock<HashMap<NamespaceId, Replica<ReplicaStoreInstance>>>>,
    authors: Arc<RwLock<HashMap<AuthorId, Author>>>,
    /// Stores records by namespace -> identifier + timestamp
    replica_records:
        Arc<RwLock<HashMap<NamespaceId, BTreeMap<RecordIdentifier, BTreeMap<u64, SignedEntry>>>>>,
}

impl super::Store for Store {
    type Instance = ReplicaStoreInstance;
    type GetLatestIter<'a> = GetLatestIter<'a>;
    type GetAllIter<'a> = GetAllIter<'a>;

    fn get_replica(&self, namespace: &NamespaceId) -> Result<Option<Replica<Self::Instance>>> {
        let replicas = &*self.replicas.read();
        Ok(replicas.get(namespace).cloned())
    }

    fn get_author(&self, author: &AuthorId) -> Result<Option<Author>> {
        let authors = &*self.authors.read();
        Ok(authors.get(author).cloned())
    }

    fn new_author<R: CryptoRngCore + ?Sized>(&self, rng: &mut R) -> Result<Author> {
        let author = Author::new(rng);
        self.authors.write().insert(author.id(), author.clone());
        Ok(author)
    }

    fn new_replica(&self, namespace: Namespace) -> Result<Replica<ReplicaStoreInstance>> {
        let id = namespace.id();
        let replica = Replica::new(namespace, ReplicaStoreInstance::new(id, self.clone()));
        self.replicas
            .write()
            .insert(replica.namespace(), replica.clone());
        Ok(replica)
    }

    fn get_latest_by_key_and_author(
        &self,
        namespace: NamespaceId,
        key: impl AsRef<[u8]>,
        author: AuthorId,
    ) -> Result<Option<SignedEntry>> {
        let inner = self.replica_records.read();

        let value = inner
            .get(&namespace)
            .and_then(|records| records.get(&RecordIdentifier::new(key, namespace, author)))
            .and_then(|values| values.last_key_value());

        Ok(value.map(|(_, v)| v.clone()))
    }

    fn get_latest_by_key(
        &self,
        namespace: NamespaceId,
        key: impl AsRef<[u8]>,
    ) -> Result<GetLatestIter<'_>> {
        let records = self.replica_records.read();
        let key = key.as_ref().to_vec();
        let filter = GetFilter::Key { namespace, key };

        Ok(GetLatestIter {
            records,
            filter,
            index: 0,
        })
    }

    fn get_latest_by_prefix(
        &self,
        namespace: NamespaceId,
        prefix: impl AsRef<[u8]>,
    ) -> Result<GetLatestIter<'_>> {
        let records = self.replica_records.read();
        let prefix = prefix.as_ref().to_vec();
        let filter = GetFilter::Prefix { namespace, prefix };

        Ok(GetLatestIter {
            records,
            filter,
            index: 0,
        })
    }

    fn get_latest(&self, namespace: NamespaceId) -> Result<GetLatestIter<'_>> {
        let records = self.replica_records.read();
        let filter = GetFilter::All { namespace };

        Ok(GetLatestIter {
            records,
            filter,
            index: 0,
        })
    }

    fn get_all_by_key_and_author<'a, 'b: 'a>(
        &'a self,
        namespace: NamespaceId,
        key: impl AsRef<[u8]> + 'b,
        author: AuthorId,
    ) -> Result<GetAllIter<'a>> {
        let records = self.replica_records.read();
        let record_id = RecordIdentifier::new(key, namespace, author);
        let filter = GetFilter::KeyAuthor(record_id);

        Ok(GetAllIter {
            records,
            filter,
            index: 0,
        })
    }

    fn get_all_by_key(
        &self,
        namespace: NamespaceId,
        key: impl AsRef<[u8]>,
    ) -> Result<GetAllIter<'_>> {
        let records = self.replica_records.read();
        let key = key.as_ref().to_vec();
        let filter = GetFilter::Key { namespace, key };

        Ok(GetAllIter {
            records,
            filter,
            index: 0,
        })
    }

    fn get_all_by_prefix(
        &self,
        namespace: NamespaceId,
        prefix: impl AsRef<[u8]>,
    ) -> Result<GetAllIter<'_>> {
        let records = self.replica_records.read();
        let prefix = prefix.as_ref().to_vec();
        let filter = GetFilter::Prefix { namespace, prefix };

        Ok(GetAllIter {
            records,
            filter,
            index: 0,
        })
    }

    fn get_all(&self, namespace: NamespaceId) -> Result<GetAllIter<'_>> {
        let records = self.replica_records.read();
        let filter = GetFilter::All { namespace };

        Ok(GetAllIter {
            records,
            filter,
            index: 0,
        })
    }
}

#[derive(Debug)]
enum GetFilter {
    /// All entries.
    All { namespace: NamespaceId },
    /// Filter by key and author.
    KeyAuthor(RecordIdentifier),
    /// Filter by key only.
    Key {
        namespace: NamespaceId,
        key: Vec<u8>,
    },
    /// Filter by prefix only.
    Prefix {
        namespace: NamespaceId,
        prefix: Vec<u8>,
    },
}

impl GetFilter {
    fn namespace(&self) -> &NamespaceId {
        match self {
            GetFilter::All { ref namespace } => namespace,
            GetFilter::KeyAuthor(ref r) => r.namespace(),
            GetFilter::Key { ref namespace, .. } => namespace,
            GetFilter::Prefix { ref namespace, .. } => namespace,
        }
    }
}

#[derive(Debug)]
pub struct GetLatestIter<'a> {
    records: RwLockReadGuard<
        'a,
        HashMap<NamespaceId, BTreeMap<RecordIdentifier, BTreeMap<u64, SignedEntry>>>,
    >,
    filter: GetFilter,
    /// Current iteration index.
    index: usize,
}

impl<'a> Iterator for GetLatestIter<'a> {
    type Item = Result<SignedEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let records = self.records.get(self.filter.namespace())?;
        let res = match self.filter {
            GetFilter::All { namespace } => {
                let (_, res) = records
                    .iter()
                    .filter(|(k, _)| k.namespace() == &namespace)
                    .filter_map(|(_key, value)| value.last_key_value())
                    .nth(self.index)?;
                res.clone()
            }
            GetFilter::KeyAuthor(ref record_id) => {
                let values = records.get(record_id)?;
                let (_, res) = values.iter().nth(self.index)?;
                res.clone()
            }
            GetFilter::Key { namespace, ref key } => {
                let (_, res) = records
                    .iter()
                    .filter(|(k, _)| k.key() == key && k.namespace() == &namespace)
                    .filter_map(|(_key, value)| value.last_key_value())
                    .nth(self.index)?;
                res.clone()
            }
            GetFilter::Prefix {
                namespace,
                ref prefix,
            } => {
                let (_, res) = records
                    .iter()
                    .filter(|(k, _)| k.key().starts_with(prefix) && k.namespace() == &namespace)
                    .filter_map(|(_key, value)| value.last_key_value())
                    .nth(self.index)?;
                res.clone()
            }
        };
        self.index += 1;
        Some(Ok(res))
    }
}

#[derive(Debug)]
pub struct GetAllIter<'a> {
    records: RwLockReadGuard<
        'a,
        HashMap<NamespaceId, BTreeMap<RecordIdentifier, BTreeMap<u64, SignedEntry>>>,
    >,
    filter: GetFilter,
    /// Current iteration index.
    index: usize,
}

impl<'a> Iterator for GetAllIter<'a> {
    type Item = Result<(u64, SignedEntry)>;

    fn next(&mut self) -> Option<Self::Item> {
        let records = self.records.get(self.filter.namespace())?;
        let res = match self.filter {
            GetFilter::All { namespace } => records
                .iter()
                .filter(|(k, _)| k.namespace() == &namespace)
                .flat_map(|(_, value)| value.iter().map(|(t, value)| (*t, value.clone())))
                .nth(self.index)?,
            GetFilter::KeyAuthor(ref record_id) => {
                let values = records.get(record_id)?;
                let (t, value) = values.iter().nth(self.index)?;
                (*t, value.clone())
            }
            GetFilter::Key { namespace, ref key } => records
                .iter()
                .filter(|(k, _)| k.key() == key && k.namespace() == &namespace)
                .flat_map(|(_, value)| value.iter().map(|(t, value)| (*t, value.clone())))
                .nth(self.index)?,
            GetFilter::Prefix {
                namespace,
                ref prefix,
            } => records
                .iter()
                .filter(|(k, _)| k.key().starts_with(prefix) && k.namespace() == &namespace)
                .flat_map(|(_, value)| value.iter().map(|(t, value)| (*t, value.clone())))
                .nth(self.index)?,
        };
        self.index += 1;
        Some(Ok(res))
    }
}

#[derive(Debug, Clone)]
pub struct ReplicaStoreInstance {
    namespace: NamespaceId,
    store: Store,
}

impl ReplicaStoreInstance {
    fn new(namespace: NamespaceId, store: Store) -> Self {
        ReplicaStoreInstance { namespace, store }
    }

    fn with_records<F, T>(&self, f: F) -> T
    where
        F: FnOnce(Option<&BTreeMap<RecordIdentifier, BTreeMap<u64, SignedEntry>>>) -> T,
    {
        let guard = self.store.replica_records.read();
        let value = guard.get(&self.namespace);
        f(value)
    }

    fn with_records_mut<F, T>(&self, f: F) -> T
    where
        F: FnOnce(Option<&mut BTreeMap<RecordIdentifier, BTreeMap<u64, SignedEntry>>>) -> T,
    {
        let mut guard = self.store.replica_records.write();
        let value = guard.get_mut(&self.namespace);
        f(value)
    }

    fn with_records_mut_with_default<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut BTreeMap<RecordIdentifier, BTreeMap<u64, SignedEntry>>) -> T,
    {
        let mut guard = self.store.replica_records.write();
        let value = guard.entry(self.namespace).or_default();
        f(value)
    }

    fn records_iter(&self) -> RecordsIter<'_> {
        RecordsIter {
            namespace: self.namespace,
            replica_records: self.store.replica_records.read(),
            i: 0,
        }
    }
}

#[derive(Debug)]
struct RecordsIter<'a> {
    namespace: NamespaceId,
    replica_records: RwLockReadGuard<
        'a,
        HashMap<NamespaceId, BTreeMap<RecordIdentifier, BTreeMap<u64, SignedEntry>>>,
    >,
    i: usize,
}

impl Iterator for RecordsIter<'_> {
    type Item = (RecordIdentifier, BTreeMap<u64, SignedEntry>);

    fn next(&mut self) -> Option<Self::Item> {
        let records = self.replica_records.get(&self.namespace)?;
        let (key, value) = records.iter().nth(self.i)?;
        self.i += 1;
        Some((key.clone(), value.clone()))
    }
}

impl crate::ranger::Store<RecordIdentifier, SignedEntry> for ReplicaStoreInstance {
    type Error = Infallible;

    /// Get a the first key (or the default if none is available).
    fn get_first(&self) -> Result<RecordIdentifier, Self::Error> {
        Ok(self.with_records(|records| {
            records
                .and_then(|r| r.first_key_value().map(|(k, _)| k.clone()))
                .unwrap_or_default()
        }))
    }

    fn get(&self, key: &RecordIdentifier) -> Result<Option<SignedEntry>, Self::Error> {
        Ok(self.with_records(|records| {
            records
                .and_then(|r| r.get(key))
                .and_then(|values| values.last_key_value())
                .map(|(_, v)| v.clone())
        }))
    }

    fn len(&self) -> Result<usize, Self::Error> {
        Ok(self.with_records(|records| records.map(|v| v.len()).unwrap_or_default()))
    }

    fn is_empty(&self) -> Result<bool, Self::Error> {
        Ok(self.len()? == 0)
    }

    fn get_fingerprint(
        &self,
        range: &Range<RecordIdentifier>,
        limit: Option<&Range<RecordIdentifier>>,
    ) -> Result<Fingerprint, Self::Error> {
        let elements = self.get_range(range.clone(), limit.cloned())?;
        let mut fp = Fingerprint::empty();
        for el in elements {
            fp ^= el.0.as_fingerprint();
        }

        Ok(fp)
    }

    fn put(&mut self, k: RecordIdentifier, v: SignedEntry) -> Result<(), Self::Error> {
        // TODO: propagate error/not insertion?
        if v.verify().is_ok() {
            let timestamp = v.entry().record().timestamp();
            // TODO: verify timestamp is "reasonable"

            self.with_records_mut_with_default(|records| {
                records.entry(k).or_default().insert(timestamp, v);
            });
        }
        Ok(())
    }

    type RangeIterator<'a> = RangeIterator<'a>;
    fn get_range(
        &self,
        range: Range<RecordIdentifier>,
        limit: Option<Range<RecordIdentifier>>,
    ) -> Result<Self::RangeIterator<'_>, Self::Error> {
        Ok(RangeIterator {
            iter: self.records_iter(),
            range: Some(range),
            limit,
        })
    }

    fn remove(&mut self, key: &RecordIdentifier) -> Result<Vec<(u64, SignedEntry)>, Self::Error> {
        let res = self.with_records_mut(|records| {
            records
                .and_then(|records| records.remove(key).map(|v| v.into_iter().collect()))
                .unwrap_or_default()
        });
        Ok(res)
    }

    type AllIterator<'a> = RangeIterator<'a>;

    fn all(&self) -> Result<Self::AllIterator<'_>, Self::Error> {
        Ok(RangeIterator {
            iter: self.records_iter(),
            range: None,
            limit: None,
        })
    }
}

#[derive(Debug)]
pub struct RangeIterator<'a> {
    iter: RecordsIter<'a>,
    range: Option<Range<RecordIdentifier>>,
    limit: Option<Range<RecordIdentifier>>,
}

impl RangeIterator<'_> {
    fn matches(&self, x: &RecordIdentifier) -> bool {
        let range = self.range.as_ref().map(|r| x.contains(r)).unwrap_or(true);
        let limit = self.limit.as_ref().map(|r| x.contains(r)).unwrap_or(true);
        range && limit
    }
}

impl Iterator for RangeIterator<'_> {
    type Item = (RecordIdentifier, SignedEntry);

    fn next(&mut self) -> Option<Self::Item> {
        let mut next = self.iter.next()?;
        loop {
            if self.matches(&next.0) {
                let (k, mut values) = next;
                let (_, v) = values.pop_last()?;
                return Some((k, v));
            }

            next = self.iter.next()?;
        }
    }
}
