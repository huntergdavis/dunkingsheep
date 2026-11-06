//! A string-keyed map that iterates in insertion order, the way a Python dict does.
//!
//! The state file lists the stuck candidates, the stuck records and the spent ledger in the order
//! their keys were first assigned. A sorted map would write the same entries in a different order, so
//! a Rust save of a Python-written file would not be byte for byte the same.

use std::collections::BTreeMap;
use std::ops::Index;

/// Keys in insertion order. Assigning an existing key keeps its place. Removing a key and assigning it
/// again puts it at the end, as a Python dict does after `del` or `pop`.
#[derive(Debug, Clone)]
pub struct OrderedMap<V> {
    /// Each value with the sequence number of its latest insertion.
    entries: BTreeMap<String, (u64, V)>,
    next_seq: u64,
}

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            next_seq: 0,
        }
    }
}

// The methods mirror the BTreeMap surface the callers use. A caller may not use every one of them.
#[allow(dead_code)]
impl<V> OrderedMap<V> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        self.entries.get(key).map(|(_, value)| value)
    }

    pub(crate) fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.entries.get_mut(key).map(|(_, value)| value)
    }

    /// Assigns `key`. An existing key keeps its place; the value it had is returned.
    pub(crate) fn insert(&mut self, key: String, value: V) -> Option<V> {
        if let Some((_, slot)) = self.entries.get_mut(&key) {
            return Some(std::mem::replace(slot, value));
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.insert(key, (seq, value));
        None
    }

    pub(crate) fn remove(&mut self, key: &str) -> Option<V> {
        self.entries.remove(key).map(|(_, value)| value)
    }

    /// Every value, in insertion order.
    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, value)| value)
    }

    /// The entry for `key`, so a caller can insert a default when the key is absent.
    pub(crate) fn entry(&mut self, key: String) -> Entry<'_, V> {
        Entry { map: self, key }
    }

    /// Every key and value, in insertion order.
    pub(crate) fn iter(&self) -> std::vec::IntoIter<(&String, &V)> {
        let mut rows: Vec<(u64, &String, &V)> = self
            .entries
            .iter()
            .map(|(key, (seq, value))| (*seq, key, value))
            .collect();
        rows.sort_unstable_by_key(|row| row.0);
        rows.into_iter()
            .map(|(_, key, value)| (key, value))
            .collect::<Vec<_>>()
            .into_iter()
    }
}

/// A pending assignment to one key of an [`OrderedMap`].
pub struct Entry<'a, V> {
    map: &'a mut OrderedMap<V>,
    key: String,
}

impl<'a, V> Entry<'a, V> {
    /// The value for the key. A missing key is inserted with `V::default()` at the end.
    pub(crate) fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        let Self { map, key } = self;
        if !map.contains_key(&key) {
            map.insert(key.clone(), V::default());
        }
        map.get_mut(&key).expect("the key was just inserted")
    }
}

impl<V> Index<&str> for OrderedMap<V> {
    type Output = V;

    fn index(&self, key: &str) -> &V {
        self.get(key).expect("no entry for key")
    }
}

impl<V: PartialEq> PartialEq for OrderedMap<V> {
    /// Two maps are equal when they hold the same keys with equal values, in any order, as dicts are.
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .entries
                .iter()
                .all(|(key, (_, value))| other.get(key) == Some(value))
    }
}

impl<V> FromIterator<(String, V)> for OrderedMap<V> {
    fn from_iter<I: IntoIterator<Item = (String, V)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (key, value) in iter {
            map.insert(key, value);
        }
        map
    }
}

impl<'a, V> IntoIterator for &'a OrderedMap<V> {
    type Item = (&'a String, &'a V);
    type IntoIter = std::vec::IntoIter<(&'a String, &'a V)>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::OrderedMap;

    fn keys(map: &OrderedMap<u32>) -> Vec<&str> {
        map.iter().map(|(key, _)| key.as_str()).collect()
    }

    #[test]
    fn keeps_first_insertion_order_and_updates_in_place() {
        let mut map = OrderedMap::new();
        map.insert("w9:p2".to_owned(), 1);
        map.insert("w9:p1".to_owned(), 2);
        assert_eq!(Some(2), map.insert("w9:p2".to_owned(), 3));
        assert_eq!(keys(&map), ["w9:p2", "w9:p1"]);
        assert_eq!(3, map["w9:p2"]);
    }

    #[test]
    fn remove_then_reinsert_moves_the_key_to_the_end() {
        let mut map = OrderedMap::new();
        map.insert("a".to_owned(), 1);
        map.insert("b".to_owned(), 2);
        assert_eq!(Some(1), map.remove("a"));
        map.insert("a".to_owned(), 3);
        assert_eq!(keys(&map), ["b", "a"]);
    }

    #[test]
    fn entry_or_default_inserts_once_and_returns_the_slot() {
        let mut map: OrderedMap<u32> = OrderedMap::new();
        *map.entry("k".to_owned()).or_default() += 1;
        *map.entry("k".to_owned()).or_default() += 1;
        assert_eq!(2, map["k"]);
        assert_eq!(1, map.len());
    }

    #[test]
    fn equality_ignores_order_as_dict_equality_does() {
        let mut left = OrderedMap::new();
        left.insert("a".to_owned(), 1);
        left.insert("b".to_owned(), 2);
        let mut right = OrderedMap::new();
        right.insert("b".to_owned(), 2);
        right.insert("a".to_owned(), 1);
        assert_eq!(left, right);
        right.insert("a".to_owned(), 9);
        assert_ne!(left, right);
    }
}
