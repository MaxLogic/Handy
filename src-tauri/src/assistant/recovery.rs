use super::{RecoveryOutcome, TargetKind};
use serde::Serialize;
use specta::Type;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Serialize, Type)]
pub struct AssistantRecoveryItem {
    pub id: u64,
    pub created_at: i64,
    pub target: TargetKind,
    pub outcome: RecoveryOutcome,
    pub reason: String,
    pub detail: Option<String>,
    pub clipboard: String,
    pub instruction: String,
    pub generated: String,
}

impl std::fmt::Debug for AssistantRecoveryItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistantRecoveryItem")
            .field("id", &self.id)
            .field("target", &self.target)
            .field("outcome", &self.outcome)
            .field("reason", &self.reason)
            .field("detail", &self.detail)
            .field("clipboard", &self.clipboard)
            .finish()
    }
}

struct Inner {
    capacity: usize,
    reserved: usize,
    items: Vec<AssistantRecoveryItem>,
    next_id: u64,
}

pub(crate) struct RecoveryStore {
    inner: Mutex<Inner>,
}

impl RecoveryStore {
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                capacity,
                reserved: 0,
                items: Vec::new(),
                next_id: 1,
            }),
        })
    }
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub(crate) fn reserve(self: &Arc<Self>) -> Option<Reservation> {
        let mut inner = self.inner();
        if inner.items.len() + inner.reserved >= inner.capacity {
            return None;
        }
        inner.reserved += 1;
        Some(Reservation {
            store: Arc::clone(self),
            live: true,
        })
    }
    pub(crate) fn items(&self) -> Vec<AssistantRecoveryItem> {
        self.inner().items.clone()
    }
    pub(crate) fn get(&self, id: u64) -> Option<AssistantRecoveryItem> {
        self.inner()
            .items
            .iter()
            .find(|item| item.id == id)
            .cloned()
    }
    pub(crate) fn dismiss(&self, id: u64) -> bool {
        let mut inner = self.inner();
        let Some(index) = inner.items.iter().position(|item| item.id == id) else {
            return false;
        };
        inner.items.remove(index);
        true
    }
    pub(crate) fn capacity(&self) -> usize {
        self.inner().capacity
    }
    pub(crate) fn free_slots(&self) -> usize {
        let inner = self.inner();
        inner.capacity - inner.reserved - inner.items.len()
    }
}

pub(crate) struct Reservation {
    store: Arc<RecoveryStore>,
    live: bool,
}
impl Reservation {
    pub(crate) fn fill(
        mut self,
        build: impl FnOnce(u64) -> AssistantRecoveryItem,
    ) -> AssistantRecoveryItem {
        let mut inner = self.store.inner();
        let item = build(inner.next_id);
        inner.next_id += 1;
        inner.items.push(item.clone());
        inner.reserved -= 1;
        self.live = false;
        item
    }
    pub(crate) fn release(self) {
        drop(self);
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.live {
            self.store.inner().reserved -= 1;
        }
    }
}
