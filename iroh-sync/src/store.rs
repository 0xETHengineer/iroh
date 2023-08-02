pub mod memory {
    //! In memory storage for replicas.

    use std::{
        collections::{BTreeMap, HashMap},
        sync::Arc,
    };

    use parking_lot::{RwLock, RwLockReadGuard};
    use rand_core::CryptoRngCore;

    use crate::{
        ranger::{AsFingerprint, Fingerprint, Range, RangeKey},
        sync::{
            Author, AuthorId, Namespace, NamespaceId, RecordIdentifier, Replica as SyncReplica,
            SignedEntry,
        },
    };

    pub type Replica = SyncReplica<ReplicaStoreInstance>;

    /// Manages the replicas and authors for an instance.
    #[derive(Debug, Clone, Default)]
    pub struct ReplicaStore {
        replicas: Arc<RwLock<HashMap<NamespaceId, Replica>>>,
        authors: Arc<RwLock<HashMap<AuthorId, Author>>>,
        /// Stores records by namespace -> identifier + timestamp
        replica_records: Arc<
            RwLock<HashMap<NamespaceId, BTreeMap<RecordIdentifier, BTreeMap<u64, SignedEntry>>>>,
        >,
    }

    impl ReplicaStore {
        pub fn get_replica(&self, namespace: &NamespaceId) -> Option<Replica> {
            let replicas = &*self.replicas.read();
            replicas.get(namespace).cloned()
        }

        pub fn get_author(&self, author: &AuthorId) -> Option<Author> {
            let authors = &*self.authors.read();
            authors.get(author).cloned()
        }

        pub fn new_author<R: CryptoRngCore + ?Sized>(&self, rng: &mut R) -> Author {
            let author = Author::new(rng);
            self.authors.write().insert(*author.id(), author.clone());
            author
        }

        pub fn new_replica(&self, namespace: Namespace) -> Replica {
            let id = *namespace.id();
            let replica = Replica::new(namespace, ReplicaStoreInstance::new(id, self.clone()));
            self.replicas
                .write()
                .insert(replica.namespace(), replica.clone());
            replica
        }
    }

    #[derive(Debug, Clone)]
    pub struct ReplicaStoreInstance {
        namespace: NamespaceId,
        store: ReplicaStore,
    }

    impl ReplicaStoreInstance {
        fn new(namespace: NamespaceId, store: ReplicaStore) -> Self {
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

        fn records_iter(&self) -> RecordsIter<'_> {
            RecordsIter {
                namespace: self.namespace,
                replica_records: self.store.replica_records.read(),
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
    }

    impl Iterator for RecordsIter<'_> {
        type Item = (RecordIdentifier, BTreeMap<u64, SignedEntry>);

        fn next(&mut self) -> Option<Self::Item> {
            todo!()
        }
    }

    impl crate::ranger::Store<RecordIdentifier, SignedEntry> for ReplicaStoreInstance {
        /// Get a the first key (or the default if none is available).
        fn get_first(&self) -> RecordIdentifier {
            self.with_records(|records| {
                records
                    .and_then(|r| r.first_key_value().map(|(k, _)| k.clone()))
                    .unwrap_or_default()
            })
        }

        fn get(&self, key: &RecordIdentifier) -> Option<SignedEntry> {
            self.with_records(|records| {
                records
                    .and_then(|r| r.get(key))
                    .and_then(|values| values.last_key_value())
                    .map(|(_, v)| v.clone())
            })
        }

        fn len(&self) -> usize {
            self.with_records(|records| records.map(|v| v.len()).unwrap_or_default())
        }

        fn is_empty(&self) -> bool {
            self.len() == 0
        }

        fn get_fingerprint(
            &self,
            range: &Range<RecordIdentifier>,
            limit: Option<&Range<RecordIdentifier>>,
        ) -> Fingerprint {
            let elements = self.get_range(range.clone(), limit.cloned());
            let mut fp = Fingerprint::empty();
            for el in elements {
                fp ^= el.0.as_fingerprint();
            }

            fp
        }

        fn put(&mut self, k: RecordIdentifier, v: SignedEntry) {
            // TODO: propagate error/not insertion?
            if v.verify().is_ok() {
                let timestamp = v.entry().record().timestamp();
                // TODO: verify timestamp is "reasonable"

                self.with_records_mut(|records| {
                    match records {
                        Some(records) => {
                            records.entry(k).or_default().insert(timestamp, v);
                        }
                        None => {
                            // ?
                        }
                    }
                });
            }
        }

        type RangeIterator<'a> = RangeIterator<'a>;
        fn get_range(
            &self,
            range: Range<RecordIdentifier>,
            limit: Option<Range<RecordIdentifier>>,
        ) -> Self::RangeIterator<'_> {
            RangeIterator {
                iter: self.records_iter(),
                range: Some(range),
                limit,
            }
        }

        fn remove(&mut self, key: &RecordIdentifier) -> Option<SignedEntry> {
            self.with_records_mut(|records| {
                records
                    .and_then(|records| records.remove(key))
                    .and_then(|mut v| v.last_entry().map(|e| e.remove_entry().1))
            })
        }

        type AllIterator<'a> = RangeIterator<'a>;

        fn all(&self) -> Self::AllIterator<'_> {
            RangeIterator {
                iter: self.records_iter(),
                range: None,
                limit: None,
            }
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
}
