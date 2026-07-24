use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub(crate) struct AcpSlot<T> {
    pub(crate) lifecycle: Mutex<()>,
    pub(crate) value: T,
}

pub(crate) struct AcpRegistry<T> {
    sessions: Mutex<HashMap<String, Arc<AcpSlot<T>>>>,
    #[allow(dead_code)]
    spawn_gate: Arc<RwLock<()>>,
}

impl<T> AcpRegistry<T> {
    pub(crate) fn new(spawn_gate: Arc<RwLock<()>>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            spawn_gate,
        }
    }

    pub(crate) fn reserve_with(
        &self,
        id: &str,
        create: impl FnOnce() -> T,
    ) -> (Arc<AcpSlot<T>>, bool) {
        use std::collections::hash_map::Entry;

        match self.sessions.lock().unwrap().entry(id.to_string()) {
            Entry::Occupied(entry) => (Arc::clone(entry.get()), false),
            Entry::Vacant(entry) => {
                let slot = Arc::new(AcpSlot {
                    lifecycle: Mutex::new(()),
                    value: create(),
                });
                entry.insert(Arc::clone(&slot));
                (slot, true)
            }
        }
    }

    pub(crate) fn get(&self, id: &str) -> Option<Arc<AcpSlot<T>>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub(crate) fn snapshot(&self) -> Vec<(String, Arc<AcpSlot<T>>)> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, slot)| (id.clone(), Arc::clone(slot)))
            .collect()
    }

    pub(crate) fn remove_if_same(
        &self,
        id: &str,
        expected: &Arc<AcpSlot<T>>,
    ) -> Option<Arc<AcpSlot<T>>> {
        let mut sessions = self.sessions.lock().unwrap();
        let current = sessions.get(id)?;
        if !Arc::ptr_eq(current, expected) {
            return None;
        }
        sessions.remove(id)
    }

    #[allow(dead_code)]
    pub(crate) fn spawn_gate(&self) -> Arc<RwLock<()>> {
        Arc::clone(&self.spawn_gate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, RwLock};

    #[test]
    fn concurrent_reserve_returns_one_entry() {
        let registry = Arc::new(AcpRegistry::<usize>::new(Arc::new(RwLock::new(()))));
        let created = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();

        for _ in 0..8 {
            let registry = Arc::clone(&registry);
            let created = Arc::clone(&created);
            threads.push(std::thread::spawn(move || {
                registry.reserve_with("same", || {
                    created.fetch_add(1, Ordering::SeqCst);
                    7
                })
            }));
        }

        let entries: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(created.load(Ordering::SeqCst), 1);
        assert!(
            entries
                .windows(2)
                .all(|pair| Arc::ptr_eq(&pair[0].0, &pair[1].0))
        );
    }

    #[test]
    fn remove_if_same_does_not_delete_a_replacement() {
        let registry = AcpRegistry::new(Arc::new(RwLock::new(())));
        let (old, _) = registry.reserve_with("id", || 1usize);
        assert!(registry.remove_if_same("id", &old).is_some());
        let (replacement, _) = registry.reserve_with("id", || 2usize);

        assert!(registry.remove_if_same("id", &old).is_none());
        assert!(Arc::ptr_eq(&registry.get("id").unwrap(), &replacement));
    }
}
