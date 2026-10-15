//! Orders explicit chat clipboard requests across cards without retaining bodies.
use std::sync::{Arc,Mutex,Weak,atomic::{AtomicU64,Ordering}};

pub(super) trait Pending:Send+Sync {fn withdraw(&self);}
#[derive(Default)]
struct Hub {generation:AtomicU64,queued:Mutex<Vec<Weak<dyn Pending>>>}
#[derive(Clone,Default)]
pub(super) struct Intents(Arc<Hub>);
#[derive(Clone)]
pub(super) struct Intent {hub:Arc<Hub>,generation:u64}
impl Intents {
    pub(super) fn next(&self)->Intent {
        let generation=self.0.generation.fetch_add(1,Ordering::AcqRel).wrapping_add(1);
        let previous=std::mem::take(&mut *self.0.queued.lock().unwrap_or_else(|error|error.into_inner()));
        // Withdraw after unlocking; a consumer can retire the actual last
        // body owner on its dedicated reclaimer without nested hub locks.
        for slot in previous {if let Some(slot)=slot.upgrade() {slot.withdraw();}}
        Intent {hub:self.0.clone(),generation}
    }
}
impl Intent {
    pub(super) fn current(&self)->bool {self.hub.generation.load(Ordering::Acquire)==self.generation}
    pub(super) fn register<T:Pending+'static>(&self,input:&Arc<T>) {
        let erased:Arc<dyn Pending>=input.clone();
        self.hub.queued.lock().unwrap_or_else(|error|error.into_inner()).push(Arc::downgrade(&erased));
    }
}
