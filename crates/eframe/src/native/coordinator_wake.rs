use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Coalesces native coordinator records into one root-viewport wake edge.
///
/// The coordinator remains the owner of causal records. This sidecar only makes
/// newly available records observable to the event loop; it carries no platform
/// facts and cannot mutate coordinator state.
#[derive(Clone, Default)]
pub(super) struct NativeCoordinatorWake {
    state: Arc<WakeState>,
}

#[derive(Default)]
struct WakeState {
    pending: AtomicBool,
    context: Option<egui::Context>,
}

impl fmt::Debug for NativeCoordinatorWake {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeCoordinatorWake")
            .field("pending", &self.state.pending.load(Ordering::Acquire))
            .field("enabled", &self.state.context.is_some())
            .finish()
    }
}

impl NativeCoordinatorWake {
    pub(super) fn new(context: egui::Context) -> Self {
        Self {
            state: Arc::new(WakeState {
                pending: AtomicBool::new(false),
                context: Some(context),
            }),
        }
    }

    pub(super) fn notify_record_available(&self) {
        if self.state.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(context) = &self.state.context {
            context.request_repaint_of(egui::ViewportId::ROOT);
        }
    }

    /// Marks all records preceding the next frozen ingress as consumed.
    pub(super) fn begin_consume(&self) {
        self.state.pending.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub(super) fn is_pending(&self) -> bool {
        self.state.pending.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[test]
    fn repeated_records_coalesce_until_the_next_consume_boundary() {
        let context = egui::Context::default();
        let wake_count = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&wake_count);
        context.set_request_repaint_callback(move |request| {
            assert_eq!(request.viewport_id, egui::ViewportId::ROOT);
            observed.fetch_add(1, Ordering::Relaxed);
        });
        let wake = NativeCoordinatorWake::new(context);

        wake.notify_record_available();
        wake.notify_record_available();
        assert!(wake.is_pending());
        assert_eq!(wake_count.load(Ordering::Relaxed), 1);

        wake.begin_consume();
        assert!(!wake.is_pending());

        wake.notify_record_available();
        assert!(wake.is_pending());
        assert_eq!(wake_count.load(Ordering::Relaxed), 1);
    }
}
