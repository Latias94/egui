use std::sync::Arc;

use epaint::mutex::Mutex;

use crate::{PaintOutcome, ViewportId};

use super::{PointerHitGraphSnapshot, authority::PresentedPointerHitGraphAuthority};

/// A completed egui pass that can become pointer receiver authority after host settlement.
///
/// The candidate is bound to the egui viewport identity that produced it. Native integrations
/// must bind that identity to their own exact window incarnation. Integrations should call
/// [`Self::settle_for`] exactly when the corresponding renderer attempt reaches a terminal
/// outcome. A successful settlement means the host accepted this pass as its latest interactable
/// visual output; it does not claim that a compositor has already displayed it.
#[derive(Clone)]
pub struct PointerHitGraphCandidate {
    authority: PresentedPointerHitGraphAuthority,
    settlement: Arc<Mutex<CandidateSettlement>>,
    snapshot: PointerHitGraphSnapshot,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CandidateSettlement {
    #[default]
    Pending,
    Presented,
    NotPresented,
}

impl PointerHitGraphCandidate {
    pub(crate) fn new(
        authority: PresentedPointerHitGraphAuthority,
        snapshot: PointerHitGraphSnapshot,
    ) -> Self {
        Self {
            authority,
            settlement: Default::default(),
            snapshot,
        }
    }

    /// Return the immutable hit graph produced by the completed egui pass.
    pub fn snapshot(&self) -> &PointerHitGraphSnapshot {
        &self.snapshot
    }

    /// Settle this candidate with its first terminal renderer outcome.
    ///
    /// Returns `true` when this candidate is now the viewport's presented authority. Skipped and
    /// failed outcomes never promote. A late candidate older than the current authority is also
    /// rejected. This convenience method assumes the outcome belongs to the candidate's own egui
    /// viewport; native integrations should use [`Self::settle_for`].
    pub fn settle(&self, outcome: &PaintOutcome) -> bool {
        self.settle_for(self.snapshot.viewport_id(), outcome)
    }

    /// Accept this candidate as the current semantic frame in an explicitly headless host.
    ///
    /// This is intended for deterministic test harnesses that have no physical renderer. Regular
    /// integrations must instead call [`Self::settle`] after their renderer reaches a terminal
    /// outcome. Calling this method before an ordinary production frame is painted would violate
    /// the pointer authority contract.
    pub fn accept_for_headless_host(&self) -> bool {
        self.settle_terminal(self.snapshot.viewport_id(), true)
    }

    /// Settle this candidate for the egui viewport that received the renderer outcome.
    ///
    /// The viewport identity check and terminal settlement are atomic across all candidate clones.
    /// A mismatched viewport terminally rejects the candidate and cannot be retried later.
    pub fn settle_for(&self, expected_viewport: ViewportId, outcome: &PaintOutcome) -> bool {
        let rendered = matches!(
            outcome,
            PaintOutcome::SubmittedToBrowserCanvas
                | PaintOutcome::SubmittedToSwapchain
                | PaintOutcome::Swapped
        );
        self.settle_terminal(expected_viewport, rendered)
    }

    fn settle_terminal(&self, expected_viewport: ViewportId, rendered: bool) -> bool {
        let mut settlement = self.settlement.lock();
        if *settlement != CandidateSettlement::Pending {
            return false;
        }

        let presented = self.snapshot.viewport_id() == expected_viewport
            && rendered
            && self.authority.promote(&self.snapshot);
        *settlement = if presented {
            CandidateSettlement::Presented
        } else {
            CandidateSettlement::NotPresented
        };
        presented
    }
}

impl std::fmt::Debug for PointerHitGraphCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PointerHitGraphCandidate")
            .field("snapshot", &self.snapshot)
            .finish_non_exhaustive()
    }
}
