use crate::db::{self, Database};
use crate::highlight::highlight;
use crate::id::Id;
use crate::Error;
use axum::extract::{FromRef, Path};
use axum_extra::extract::cookie::Key as SigningKey;
use lru::LruCache;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    id: Id,
    ext: String,
}

pub struct Inner {
    cache: LruCache<Key, String>,
    cached: HashMap<Id, HashSet<String>>,
}

type Cache = Arc<Mutex<Inner>>;

impl Key {
    pub fn new(id: Id, ext: String) -> Key {
        Self { id, ext }
    }

    pub fn id(&self) -> String {
        self.id.to_string()
    }

    pub fn extension(&self) -> String {
        self.ext.clone()
    }
}

impl TryFrom<Path<String>> for Key {
    type Error = Error;

    fn try_from(value: Path<String>) -> Result<Self, Self::Error> {
        let (id, ext) = match value.split_once('.') {
            None => (Id::try_from(value.as_str())?, "txt".to_string()),
            Some((id, ext)) => (Id::try_from(id)?, ext.to_string()),
        };

        Ok(Self { id, ext })
    }
}

impl Inner {
    pub fn new(size: NonZeroUsize) -> Self {
        let cache = lru::LruCache::new(size);

        Self {
            cache,
            cached: HashMap::new(),
        }
    }

    pub fn get<'a>(&'a mut self, k: &Key) -> Option<&'a String> {
        self.cache.get(k)
    }

    pub fn put(&mut self, k: Key, v: String) -> Option<String> {
        if let Some(cached) = self.cached.get_mut(&k.id) {
            if !cached.contains(&k.ext) {
                cached.insert(k.ext.clone());
            }
        } else {
            let mut set = HashSet::new();
            set.insert(k.ext.clone());
            self.cached.insert(k.id, set);
        }

        self.cache.put(k, v)
    }

    pub fn remove(&mut self, id: Id) {
        if let Some(exts) = self.cached.remove(&id) {
            for ext in exts {
                tracing::debug!("evicting {id:?}.{ext}");
                self.cache.pop(&Key::new(id, ext));
            }
        }
    }
}

/// Cache layer combining database and cache access.
#[derive(Clone)]
pub struct Layer {
    db: Database,
    cache: Cache,
    key: SigningKey,
}

/// Entry and syntax highlighted text.
pub struct Entry {
    pub formatted: String,
    pub uid: Option<i64>,
}

impl Layer {
    pub fn new(db: Database, cache_size: NonZeroUsize, key: SigningKey) -> Self {
        let cache = Arc::new(Mutex::new(Inner::new(cache_size)));

        Self { db, cache, key }
    }

    /// Insert `entry` into the database.
    pub async fn insert(
        &self,
        id: Id,
        uid: Option<i64>,
        entry: db::InsertEntry,
    ) -> Result<(), Error> {
        self.db.insert(id, uid, entry).await
    }

    /// Look up or generate HTML formatted data. Return `None` if `key` is not found.
    pub async fn get_formatted(&self, key: &Key) -> Result<Entry, Error> {
        let entry = self.db.get(key.id).await?;
        let uid = entry.uid;

        if let Some(cached) = self.cache.lock().unwrap().get(key) {
            tracing::debug!(?key, "found cached item");

            return Ok(Entry {
                formatted: cached.to_string(),
                uid,
            });
        }

        let burn_after_reading = entry.burn_after_reading.unwrap_or(false);
        let ext = key.ext.clone();
        let formatted = tokio::task::spawn_blocking(move || highlight(&entry, &ext)).await??;

        if !burn_after_reading {
            tracing::debug!(?key, "cache item");
            self.cache
                .lock()
                .unwrap()
                .put(key.clone(), formatted.clone());
        }

        Ok(Entry { formatted, uid })
    }

    /// Get raw content for `id` or `None` if not found.
    pub async fn get(&self, id: Id) -> Result<db::ReadEntry, Error> {
        self.db.get(id).await
    }

    /// Delete `id`.
    pub async fn delete(&self, id: Id) -> Result<(), Error> {
        self.cache.lock().unwrap().remove(id);
        self.db.delete(id).await
    }

    /// Retrieve next monotonically increasing uid.
    pub async fn next_uid(&self) -> Result<i64, Error> {
        self.db.next_uid().await
    }
}

impl FromRef<Layer> for SigningKey {
    fn from_ref(layer: &Layer) -> Self {
        layer.key.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn expired_does_not_exist() -> Result<(), Box<dyn std::error::Error>> {
        let db = Database::new(db::Open::Memory)?;
        let key = SigningKey::generate();
        let layer = Layer::new(db, NonZeroUsize::new(128).unwrap(), key);

        let entry = db::InsertEntry {
            text: "hello world".to_string(),
            expires: Some(1),
            ..Default::default()
        };

        let id = Id::from(1234);
        let key = Key::new(id, "rs".to_string());
        layer.insert(id, None, entry).await?;

        let result = layer.get_formatted(&key).await;
        assert!(result.is_err());
        assert!(matches!(result.err().unwrap(), Error::NotFound));

        Ok(())
    }
}
