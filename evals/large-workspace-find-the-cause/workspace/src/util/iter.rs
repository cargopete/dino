use std::collections::BTreeMap;

/// Groups items by a key, keeping each group in the order items arrived.
pub fn group_by<T, K, I, F>(items: I, mut key: F) -> BTreeMap<K, Vec<T>>
where
    I: IntoIterator<Item = T>,
    K: Ord,
    F: FnMut(&T) -> K,
{
    let mut groups: BTreeMap<K, Vec<T>> = BTreeMap::new();
    for item in items {
        groups.entry(key(&item)).or_default().push(item);
    }
    groups
}
