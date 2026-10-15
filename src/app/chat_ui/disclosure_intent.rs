//! Bounded body-free history-page disclosure intent. Values never authorize a
//! read: consumers must first receive the exact typed source epoch again.
use std::{collections::{BTreeMap,HashMap},hash::Hash,sync::Arc};
const RECORDS:usize=4096;
const BYTES:usize=1024*1024;
/// Logical metadata budget, not allocator/RSS accounting. One shared key
/// allocation is indexed twice; old order records are removed on replacement.
pub(super) struct Cache<K,V> {values:HashMap<Arc<K>,(u64,V,usize)>,order:BTreeMap<u64,Arc<K>>,next:u64,bytes:usize}
impl<K,V> Default for Cache<K,V> {fn default()->Self {Self {values:HashMap::new(),order:BTreeMap::new(),next:0,bytes:0}}}
impl<K:Eq+Hash,V> Cache<K,V> {
    pub(super) fn clear(&mut self) {self.values.clear();self.order.clear();self.bytes=0;}
    pub(super) fn take(&mut self,key:&K)->Option<V> {
        let (order,value,bytes)=self.values.remove(key)?;self.order.remove(&order);self.bytes=self.bytes.saturating_sub(bytes);Some(value)
    }
    pub(super) fn remember(&mut self,key:K,value:V,bytes:usize) {
        self.take(&key);
        if bytes>BYTES {return;}
        if self.next==u64::MAX {self.clear();self.next=0;}
        self.next+=1;let key=Arc::new(key);self.bytes+=bytes;
        self.order.insert(self.next,key.clone());self.values.insert(key,(self.next,value,bytes));
        while self.values.len()>RECORDS || self.bytes>BYTES {
            let Some((_,key))=self.order.first_key_value() else {break;};let key=key.clone();self.take(key.as_ref());
        }
    }
    #[cfg(test)]
    pub(super) fn usage(&self)->(usize,usize) {(self.values.len(),self.bytes)}
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_and_weighted_eviction_keep_both_indexes_bounded() {
        let mut cache=Cache::default();
        for i in 0..RECORDS+4 {cache.remember(i,i,1);}
        assert_eq!(cache.usage(),(RECORDS,RECORDS));assert!(cache.take(&0).is_none());
        for _ in 0..10000 {cache.remember(7,9,2);}
        assert_eq!(cache.order.len(),cache.values.len());assert!(cache.usage().1<=BYTES);
        cache.remember(8,10,BYTES);assert_eq!(cache.usage(),(1,BYTES));
        cache.remember(9,11,BYTES+1);assert!(cache.take(&9).is_none());assert_eq!(cache.take(&8),Some(10));
        assert_eq!(cache.usage(),(0,0));
    }
}
