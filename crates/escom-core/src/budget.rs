use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Accounts for the complete allocation, including a partially consumed request.
#[derive(Debug)]
pub(crate) struct ByteBudget {
    used: AtomicUsize,
    limit: usize,
}

impl ByteBudget {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            limit,
        })
    }

    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub(crate) fn reserve(self: &Arc<Self>, bytes: usize) -> Option<Reservation> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= self.limit)
            })
            .ok()?;
        Some(Reservation {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

#[derive(Debug)]
pub(crate) struct Reservation {
    budget: Arc<ByteBudget>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reservation_rejects_overflow_and_releases_on_drop() {
        let budget = ByteBudget::new(8);
        let first = budget.reserve(6).unwrap();
        assert!(budget.reserve(3).is_none());
        assert!(budget.reserve(usize::MAX).is_none());
        drop(first);
        assert_eq!(budget.used(), 0);
        assert!(budget.reserve(8).is_some());
    }
}
