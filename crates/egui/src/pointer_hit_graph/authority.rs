use std::sync::Arc;

use epaint::mutex::Mutex;

use super::PointerHitGraphSnapshot;

/// Shared promotion state for one egui viewport incarnation.
#[derive(Clone, Default)]
pub(crate) struct PresentedPointerHitGraphAuthority {
    presented: Arc<Mutex<Option<PointerHitGraphSnapshot>>>,
}

impl PresentedPointerHitGraphAuthority {
    pub(crate) fn presented(&self) -> Option<PointerHitGraphSnapshot> {
        self.presented.lock().clone()
    }

    pub(crate) fn promote(&self, candidate: &PointerHitGraphSnapshot) -> bool {
        let mut presented = self.presented.lock();
        if presented.as_ref().is_some_and(|current| {
            current.viewport_id() != candidate.viewport_id()
                || candidate.widget_pass_nr() < current.widget_pass_nr()
        }) {
            return false;
        }

        *presented = Some(candidate.clone());
        true
    }
}
