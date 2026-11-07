//! Small copy-on-write shards keep revision snapshots cheap while bounding lookup
//! depth independently of revision history. Only shards containing edits are copied.
use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    sync::Arc,
};
#[derive(Clone)]
pub struct Map<K, V> {
    shards: Vec<Arc<BTreeMap<K, V>>>,
}
impl<K, V> Default for Map<K, V> {
    fn default() -> Self {
        Self {
            shards: (0..64).map(|_| Arc::new(BTreeMap::new())).collect(),
        }
    }
}
impl<K: Ord + Hash + Clone, V: Clone> Map<K, V> {
    fn shard(key: &K) -> usize {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        (h.finish() as usize) % 64
    }
    pub fn get(&self, key: &K) -> Option<&V> {
        self.shards[Self::shard(key)].get(key)
    }
    pub fn insert(&mut self, key: K, value: V) {
        Arc::make_mut(&mut self.shards[Self::shard(&key)]).insert(key, value);
    }
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        Arc::make_mut(&mut self.shards[Self::shard(key)]).get_mut(key)
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.shards.iter().flat_map(|s| s.iter())
    }
}
impl<K: Ord + Hash + Clone, V: Clone> FromIterator<(K, V)> for Map<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(items: I) -> Self {
        let mut result = Self::default();
        for (k, v) in items {
            result.insert(k, v);
        }
        result
    }
}
impl<K: Ord + Hash + Clone + serde::Serialize, V: Clone + serde::Serialize> serde::Serialize
    for Map<K, V>
{
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.iter().collect::<BTreeMap<_, _>>().serialize(s)
    }
}
impl<'de, K: Ord + Hash + Clone + serde::Deserialize<'de>, V: Clone + serde::Deserialize<'de>>
    serde::Deserialize<'de> for Map<K, V>
{
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(BTreeMap::<K, V>::deserialize(d)?.into_iter().collect())
    }
}
