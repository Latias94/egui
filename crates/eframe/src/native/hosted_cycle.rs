//! Complete-roster native viewport cycle orchestration.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::{Rc, Weak},
    sync::Arc,
};

use egui::{OrderedViewportIdMap, RawInput, ViewportId, ViewportOutput};

use super::{PendingPresentation, PresentationResults};

type ImmediateViolationLog = Rc<RefCell<Vec<HostedImmediateViewportViolation>>>;
type WeakImmediateViolationLog = Weak<RefCell<Vec<HostedImmediateViewportViolation>>>;

thread_local! {
    static TRANSACTIONAL_IMMEDIATE_GUARDS: RefCell<Vec<WeakImmediateViolationLog>> =
        const { RefCell::new(Vec::new()) };
}

/// One immediate viewport attempt rejected by a transactional hosted cycle.
///
/// The immediate renderer records this fact before it creates or mutates a
/// window, consumes input, invokes UI, or paints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostedImmediateViewportViolation {
    viewport_id: ViewportId,
}

impl HostedImmediateViewportViolation {
    /// Creates a violation for the immediate viewport that attempted to render.
    pub const fn new(viewport_id: ViewportId) -> Self {
        Self { viewport_id }
    }

    /// Returns the immediate viewport that attempted to render.
    pub const fn viewport_id(self) -> ViewportId {
        self.viewport_id
    }
}

impl std::fmt::Display for HostedImmediateViewportViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "immediate viewport {:?} is forbidden during a transactional hosted cycle",
            self.viewport_id
        )
    }
}

impl std::error::Error for HostedImmediateViewportViolation {}

/// RAII scope that protects a transactional hosted cycle from immediate
/// viewport side effects.
///
/// A backend enters this guard before driving the cycle. egui then embeds
/// immediate viewport callbacks without changing deferred viewport behavior.
/// The native immediate renderer must still call
/// [`check_hosted_immediate_viewport`] as its first operation; reaching it is a
/// backend contract violation. Guards are thread-local because immediate
/// viewport rendering is synchronous and thread-bound.
#[must_use = "the guard must remain alive for the complete hosted viewport cycle"]
#[derive(Debug)]
pub struct HostedViewportTransactionGuard {
    violations: Option<ImmediateViolationLog>,
    _immediate_embedding: Option<egui::ImmediateViewportEmbeddingGuard>,
}

impl HostedViewportTransactionGuard {
    /// Enters the guard required by `mode`.
    ///
    /// Compatibility mode creates an inert guard. Transactional mode registers
    /// a thread-local violation log until [`Self::finish`] or drop.
    pub fn enter(mode: crate::HostedViewportMode) -> Self {
        if mode == crate::HostedViewportMode::Compatibility {
            return Self {
                violations: None,
                _immediate_embedding: None,
            };
        }

        let violations = Rc::new(RefCell::new(Vec::new()));
        TRANSACTIONAL_IMMEDIATE_GUARDS.with(|guards| {
            guards.borrow_mut().push(Rc::downgrade(&violations));
        });
        Self {
            violations: Some(violations),
            _immediate_embedding: Some(egui::ImmediateViewportEmbeddingGuard::enter()),
        }
    }

    /// Returns a snapshot of violations observed by this scope.
    pub fn violations(&self) -> Vec<HostedImmediateViewportViolation> {
        self.violations
            .as_ref()
            .map_or_else(Vec::new, |violations| violations.borrow().clone())
    }

    /// Seals violation observation before the application publication hook.
    ///
    /// The immediate-viewport embedding scope remains active until this guard
    /// is dropped. Only the violation log is detached, so the publication hook
    /// cannot create a recursive native viewport and no new recoverable failure
    /// can appear after application state has been published.
    ///
    /// # Errors
    ///
    /// Returns the first violation observed before the seal. The caller must
    /// abort the hosted cycle without invoking its publication hook.
    pub fn seal_before_application_commit(
        &mut self,
    ) -> Result<(), HostedImmediateViewportViolation> {
        let violations = self.take_violations();
        violations.first().copied().map_or(Ok(()), Err)
    }

    /// Unregisters the scope and returns all violations it observed.
    pub fn finish(mut self) -> Vec<HostedImmediateViewportViolation> {
        self.take_violations()
    }

    fn first_violation(&self) -> Option<HostedImmediateViewportViolation> {
        let violations = self.violations.as_ref()?;
        violations.borrow().first().copied()
    }

    fn take_violations(&mut self) -> Vec<HostedImmediateViewportViolation> {
        let Some(violations) = self.violations.take() else {
            return Vec::new();
        };
        unregister_immediate_violation_log(&violations);
        std::mem::take(&mut *violations.borrow_mut())
    }
}

impl Drop for HostedViewportTransactionGuard {
    fn drop(&mut self) {
        let _ = self.take_violations();
    }
}

fn unregister_immediate_violation_log(log: &ImmediateViolationLog) {
    TRANSACTIONAL_IMMEDIATE_GUARDS.with(|guards| {
        guards.borrow_mut().retain(|candidate| {
            candidate
                .upgrade()
                .is_some_and(|candidate| !Rc::ptr_eq(&candidate, log))
        });
    });
}

/// Checks whether an immediate viewport may proceed on this thread.
///
/// An immediate viewport renderer must call this before **any** window, input,
/// UI, or paint work. When at least one transactional guard is active, this
/// records the violation in every nested transactional scope and returns an
/// error. Compatibility mode and calls outside a hosted cycle return `Ok(())`.
///
/// # Errors
///
/// Returns [`HostedImmediateViewportViolation`] while a transactional hosted
/// cycle is active on this thread.
pub fn check_hosted_immediate_viewport(
    viewport_id: ViewportId,
) -> Result<(), HostedImmediateViewportViolation> {
    let violation = HostedImmediateViewportViolation::new(viewport_id);
    let rejected = TRANSACTIONAL_IMMEDIATE_GUARDS.with(|guards| {
        let mut guards = guards.borrow_mut();
        let mut rejected = false;
        guards.retain(|candidate| {
            let Some(candidate) = candidate.upgrade() else {
                return false;
            };
            candidate.borrow_mut().push(violation);
            rejected = true;
            true
        });
        rejected
    });
    if rejected { Err(violation) } else { Ok(()) }
}

/// An immutable complete physical root/deferred input roster for one native
/// host cycle.
///
/// The roster is frozen before any viewport UI callback runs. A native runtime
/// may inspect every input in [`Self::run`] via the begin hook, then execute the
/// callbacks in any exact permutation without changing which input facts belong
/// to the cycle. Construction validates [`ViewportId::ROOT`], so every value of
/// this type carries a physical root rather than representing a rootless state.
///
/// In [`crate::HostedViewportMode::Compatibility`], recursively rendered
/// immediate viewports are outside this roster and may complete UI and
/// presentation before the physical cycle ends. A compliant
/// [`crate::HostedViewportMode::Transactional`] backend embeds their callbacks,
/// making this roster the complete native viewport transaction.
#[derive(Clone)]
pub struct HostedViewportCycle {
    inputs: BTreeMap<ViewportId, RawInput>,
    native_ingress: Option<Arc<crate::NativeHostIngress>>,
    claimed_native_event_envelopes: Arc<egui::mutex::Mutex<BTreeSet<(ViewportId, usize)>>>,
    settled_native_scroll_edges: Arc<egui::mutex::Mutex<BTreeSet<crate::NativePointerSequence>>>,
    native_effect_sink: Option<crate::NativeEffectSink>,
    native_viewport_create_sink: Option<crate::NativeViewportCreateSink>,
    native_staging_presentations: Arc<BTreeSet<crate::NativeViewportBinding>>,
    issued_native_staging_presentations:
        Arc<egui::mutex::Mutex<BTreeSet<HostedNativeOutputAuthority>>>,
}

impl std::fmt::Debug for HostedViewportCycle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedViewportCycle")
            .field("inputs", &self.inputs)
            .field("native_ingress", &self.native_ingress)
            .field("native_effect_sink", &self.native_effect_sink)
            .field(
                "native_viewport_create_sink",
                &self.native_viewport_create_sink,
            )
            .finish_non_exhaustive()
    }
}

/// One viewport input consumed by a hosted-cycle UI callback.
#[derive(Clone, Debug)]
pub struct HostedViewportInput {
    viewport_id: ViewportId,
    raw_input: RawInput,
}

impl HostedViewportInput {
    /// Returns the viewport that owns this input.
    pub const fn viewport_id(&self) -> ViewportId {
        self.viewport_id
    }

    /// Returns the frozen raw input without consuming it.
    pub const fn raw_input(&self) -> &RawInput {
        &self.raw_input
    }

    /// Separates the viewport identity from its frozen raw input.
    pub fn into_parts(self) -> (ViewportId, RawInput) {
        (self.viewport_id, self.raw_input)
    }
}

/// One viewport callback result retained until the complete cycle ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostedViewportOutput<T> {
    viewport_id: ViewportId,
    output: T,
    native_authority: Option<HostedNativeOutputAuthority>,
    native_staging_presentation: Option<HostedNativeOutputAuthority>,
    presentation_result_follow_up: Option<HostedPresentationFollowUp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct HostedNativeOutputAuthority {
    binding: crate::NativeViewportBinding,
    snapshot_generation: crate::native::platform_provider::NativePlatformSnapshotGeneration,
}

/// An affine authorization for one native staging presentation.
///
/// The native host mints eligibility only for an exact viewport binding that
/// has a known non-minimized presentation state and a non-zero render surface.
/// The core still decides whether that capability represents hidden pre-show
/// or visible post-show staging. This value can be taken only once from its
/// hosted cycle and attached only to the matching output from that same native
/// snapshot.
#[derive(Debug)]
pub struct HostedNativeStagingPresentation {
    authority: HostedNativeOutputAuthority,
}

/// Why a native staging presentation could not be authorized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedNativeStagingPresentationError {
    /// The hosted cycle has no native provider snapshot.
    NativeIngressUnavailable,
    /// The requested viewport is not part of the frozen native output roster.
    ViewportUnavailable {
        /// Requested logical viewport.
        viewport_id: ViewportId,
    },
    /// The native host did not prove a stageable, non-minimized, non-zero surface.
    SurfaceUnavailable {
        /// Exact native viewport lifetime which lacked staging authority.
        binding: crate::NativeViewportBinding,
    },
    /// This exact cycle and viewport already issued its one authorization.
    AlreadyAuthorized {
        /// Exact native viewport lifetime whose authorization was consumed.
        binding: crate::NativeViewportBinding,
    },
    /// The authorization belongs to another viewport lifetime or host snapshot.
    OutputAuthorityMismatch {
        /// Output that rejected the authorization.
        viewport_id: ViewportId,
    },
    /// The output already carries a hidden staging authorization.
    OutputAlreadyAuthorized {
        /// Output which already carries staging authority.
        viewport_id: ViewportId,
    },
    /// The staging output has no opaque token for terminal renderer settlement.
    PresentationTokenMissing {
        /// Output which omitted its presentation token.
        viewport_id: ViewportId,
    },
}

impl std::fmt::Display for HostedNativeStagingPresentationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NativeIngressUnavailable => {
                formatter.write_str("hosted cycle has no native ingress authority")
            }
            Self::ViewportUnavailable { viewport_id } => {
                write!(formatter, "hosted viewport {viewport_id:?} is unavailable")
            }
            Self::SurfaceUnavailable { binding } => write!(
                formatter,
                "native viewport {:?} has no staging surface authority",
                binding.viewport_id()
            ),
            Self::AlreadyAuthorized { binding } => write!(
                formatter,
                "native viewport {:?} already issued its staging authorization",
                binding.viewport_id()
            ),
            Self::OutputAuthorityMismatch { viewport_id } => write!(
                formatter,
                "staging authorization does not match output {viewport_id:?}"
            ),
            Self::OutputAlreadyAuthorized { viewport_id } => write!(
                formatter,
                "hosted output {viewport_id:?} already has staging authority"
            ),
            Self::PresentationTokenMissing { viewport_id } => write!(
                formatter,
                "staging output {viewport_id:?} has no presentation token"
            ),
        }
    }
}

impl std::error::Error for HostedNativeStagingPresentationError {}

/// Why a hosted output could not request a follow-up cycle after presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedPresentationFollowUpError {
    /// The output has no opaque token from which a renderer result can be produced.
    PresentationTokenMissing {
        /// Output which omitted its presentation token.
        viewport_id: ViewportId,
    },
}

impl std::fmt::Display for HostedPresentationFollowUpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PresentationTokenMissing { viewport_id } => write!(
                formatter,
                "hosted output {viewport_id:?} cannot request a presentation follow-up without a token"
            ),
        }
    }
}

impl std::error::Error for HostedPresentationFollowUpError {}

/// Which terminal renderer result must schedule another root hosted cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostedPresentationFollowUp {
    /// Continue only after a successful native renderer submission.
    SuccessfulSubmission,
    /// Continue after any terminal renderer result, including skip or failure.
    AnyResult,
}

impl HostedPresentationFollowUp {
    pub(crate) const fn matches(self, outcome: &egui::PaintOutcome) -> bool {
        match self {
            Self::SuccessfulSubmission => matches!(
                outcome,
                egui::PaintOutcome::SubmittedToSwapchain | egui::PaintOutcome::Swapped
            ),
            Self::AnyResult => true,
        }
    }
}

/// Drives one hosted viewport cycle through its ordered begin, viewport, and
/// end phases.
///
/// A single driver owns the mutable host state for all three phases. This
/// avoids splitting one application or protocol session across independently
/// captured callbacks while preserving the complete-roster barrier.
pub trait HostedViewportCycleDriver {
    /// Output produced by one viewport callback.
    type Output;

    /// Observes the complete frozen physical root/deferred input roster before
    /// any callback in that roster.
    ///
    /// # Errors
    ///
    /// Returning an error aborts the cycle before viewport UI begins.
    fn begin(&mut self, cycle: &HostedViewportCycle) -> crate::HostedViewportAppResult<()>;

    /// Runs exactly one viewport callback with its frozen input.
    ///
    /// # Errors
    ///
    /// Returning an error aborts the cycle before any later viewport or end phase.
    fn run_viewport(
        &mut self,
        input: HostedViewportInput,
    ) -> crate::HostedViewportAppResult<Self::Output>;

    /// Observes every callback output after the exact roster has completed.
    ///
    /// # Errors
    ///
    /// Returning an error aborts ordinary renderer work. The resulting
    /// [`HostedViewportCycleAbort`] retains every staged output for terminal
    /// token and resource settlement.
    fn end(
        &mut self,
        outputs: &mut [HostedViewportOutput<Self::Output>],
    ) -> crate::HostedViewportAppResult<()>;

    /// Rolls back driver-owned state after any invoked cycle phase aborts.
    ///
    /// The default is suitable for stateless drivers. Stateful drivers must
    /// make this operation idempotent because begin and end hooks may already
    /// have cleaned up a subset of their state before returning an error. In
    /// transactional mode this hook runs while the immediate-viewport guard is
    /// still active; attempts made by cleanup code are rejected and retained in
    /// the resulting [`HostedViewportCycleAbort`].
    fn abort(&mut self) {}
}

impl<T> HostedViewportOutput<T> {
    pub(super) const fn new(viewport_id: ViewportId, output: T) -> Self {
        Self {
            viewport_id,
            output,
            native_authority: None,
            native_staging_presentation: None,
            presentation_result_follow_up: None,
        }
    }

    const fn with_native_authority(
        viewport_id: ViewportId,
        output: T,
        native_authority: Option<HostedNativeOutputAuthority>,
    ) -> Self {
        Self {
            viewport_id,
            output,
            native_authority,
            native_staging_presentation: None,
            presentation_result_follow_up: None,
        }
    }

    /// Returns the viewport that produced this output.
    pub const fn viewport_id(&self) -> ViewportId {
        self.viewport_id
    }

    /// Returns the callback output without consuming it.
    pub const fn output(&self) -> &T {
        &self.output
    }

    /// Mutably borrows the callback output at the cycle-final sealing boundary.
    pub const fn output_mut(&mut self) -> &mut T {
        &mut self.output
    }

    pub(super) const fn is_native_staging_presentation(&self) -> bool {
        self.native_staging_presentation.is_some()
    }

    pub(super) const fn presentation_result_follow_up(&self) -> Option<HostedPresentationFollowUp> {
        self.presentation_result_follow_up
    }

    /// Returns the exact native lifetime that authorized this hosted output.
    ///
    /// Transactional adapters use this fact before committing their semantic
    /// frame so an output from a recycled viewport slot cannot be rebound to a
    /// different native incarnation.
    pub fn native_binding(&self) -> Option<crate::NativeViewportBinding> {
        match self.native_authority {
            Some(authority) => Some(authority.binding),
            None => None,
        }
    }

    /// Separates the viewport identity from its callback output.
    pub fn into_parts(self) -> (ViewportId, T) {
        (self.viewport_id, self.output)
    }
}

impl HostedViewportOutput<egui::FullOutput> {
    /// Requests one root hosted cycle after this output reaches a terminal renderer result.
    ///
    /// This is intended for affine application protocols whose next state depends on the exact
    /// presentation result. Ordinary continuously rendered outputs should leave it disabled.
    pub fn require_presentation_result_follow_up(
        &mut self,
        follow_up: HostedPresentationFollowUp,
    ) -> Result<(), HostedPresentationFollowUpError> {
        if self.output.platform_output.presentation_token.is_none() {
            return Err(HostedPresentationFollowUpError::PresentationTokenMissing {
                viewport_id: self.viewport_id,
            });
        }
        self.presentation_result_follow_up = Some(match self.presentation_result_follow_up {
            Some(HostedPresentationFollowUp::AnyResult) => HostedPresentationFollowUp::AnyResult,
            Some(HostedPresentationFollowUp::SuccessfulSubmission)
                if follow_up == HostedPresentationFollowUp::AnyResult =>
            {
                HostedPresentationFollowUp::AnyResult
            }
            Some(existing) => existing,
            None => follow_up,
        });
        Ok(())
    }

    /// Validates a native staging authorization without consuming it.
    ///
    /// This is the prepare half of the hosted output transaction. The caller
    /// may validate every output before committing its semantic frame, then
    /// attach the authorization after it installs the renderer token.
    pub fn validate_native_staging_presentation(
        &self,
        authorization: &HostedNativeStagingPresentation,
    ) -> Result<(), HostedNativeStagingPresentationError> {
        if self.native_authority != Some(authorization.authority) {
            return Err(
                HostedNativeStagingPresentationError::OutputAuthorityMismatch {
                    viewport_id: self.viewport_id,
                },
            );
        }
        if self.native_staging_presentation.is_some() {
            return Err(
                HostedNativeStagingPresentationError::OutputAlreadyAuthorized {
                    viewport_id: self.viewport_id,
                },
            );
        }
        Ok(())
    }

    /// Marks this output as the single native staging presentation authorized
    /// by its exact native host cycle.
    ///
    /// The output must already carry an opaque presentation token so every
    /// renderer success, skip, or failure can be reported terminally.
    ///
    /// # Errors
    ///
    /// Returns [`HostedNativeStagingPresentationError::OutputAuthorityMismatch`]
    /// when the authorization came from another viewport lifetime or native
    /// snapshot, `OutputAlreadyAuthorized` when this output was already marked,
    /// or `PresentationTokenMissing` when no terminal result can be correlated.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the affine staging authorization must be consumed exactly once"
    )]
    pub fn authorize_native_staging_presentation(
        &mut self,
        authorization: HostedNativeStagingPresentation,
    ) -> Result<(), HostedNativeStagingPresentationError> {
        self.validate_native_staging_presentation(&authorization)?;
        if self.output.platform_output.presentation_token.is_none() {
            return Err(
                HostedNativeStagingPresentationError::PresentationTokenMissing {
                    viewport_id: self.viewport_id,
                },
            );
        }
        self.native_staging_presentation = Some(authorization.authority);
        Ok(())
    }
}

/// A hosted cycle that aborted before normal renderer processing.
///
/// Outputs which completed before the failure remain available so the native
/// backend can terminally settle presentation tokens and account for texture
/// resources. Inputs not yet passed to a viewport callback are retained for
/// diagnostics and fatal-abort accounting only. They must not be replayed after
/// any viewport UI callback has begun.
pub struct HostedViewportCycleAbort<T> {
    error: HostedViewportCycleError,
    staged_outputs: Vec<HostedViewportOutput<T>>,
    unconsumed_inputs: Vec<HostedViewportInput>,
    immediate_viewport_violations: Vec<HostedImmediateViewportViolation>,
}

/// A successfully staged hosted cycle whose transactional guard remains active.
///
/// Native backends must retain this value through every post-UI application
/// callback and renderer commit. In particular, autosave may call application
/// code after the output roster seals, so releasing the guard at UI completion
/// would let an immediate viewport escape the complete-cycle barrier.
#[must_use = "the transaction guard must remain alive until the backend commit boundary"]
#[derive(Debug)]
pub(crate) struct HostedViewportCycleCompletion<T> {
    outputs: Vec<HostedViewportOutput<T>>,
    guard: HostedViewportTransactionGuard,
}

impl<T> HostedViewportCycleCompletion<T> {
    /// Separates staged outputs from the guard that protects their commit.
    pub(crate) fn into_parts(
        self,
    ) -> (Vec<HostedViewportOutput<T>>, HostedViewportTransactionGuard) {
        (self.outputs, self.guard)
    }

    fn into_outputs(self) -> Vec<HostedViewportOutput<T>> {
        let (outputs, guard) = self.into_parts();
        debug_assert!(
            guard.finish().is_empty(),
            "a transactional violation must abort before the cycle succeeds"
        );
        outputs
    }
}

/// Owned resources recovered from one aborted hosted viewport cycle.
pub type HostedViewportCycleAbortParts<T> = (
    HostedViewportCycleError,
    Vec<HostedViewportOutput<T>>,
    Vec<HostedViewportInput>,
    Vec<HostedImmediateViewportViolation>,
);

impl<T> HostedViewportCycleAbort<T> {
    pub(crate) fn from_error(error: HostedViewportCycleError) -> Self {
        Self {
            error,
            staged_outputs: Vec::new(),
            unconsumed_inputs: Vec::new(),
            immediate_viewport_violations: Vec::new(),
        }
    }

    /// Returns the failure which aborted the hosted cycle.
    pub const fn error(&self) -> &HostedViewportCycleError {
        &self.error
    }

    /// Returns outputs produced before the cycle aborted.
    ///
    /// These outputs are not eligible for ordinary painting. The backend still
    /// owns their presentation-token and texture-resource settlement.
    pub fn staged_outputs(&self) -> &[HostedViewportOutput<T>] {
        &self.staged_outputs
    }

    /// Mutably borrows outputs produced before the cycle aborted.
    pub fn staged_outputs_mut(&mut self) -> &mut [HostedViewportOutput<T>] {
        &mut self.staged_outputs
    }

    /// Retains outputs produced by a driver callback that failed after creating
    /// its output value.
    ///
    /// Generic cycle orchestration cannot recover a value hidden behind the
    /// driver's error type. A renderer-specific driver may buffer that value and
    /// append it here before converting this abort into its fatal host error.
    pub fn append_staged_outputs(
        &mut self,
        outputs: impl IntoIterator<Item = HostedViewportOutput<T>>,
    ) {
        self.staged_outputs.extend(outputs);
    }

    /// Returns inputs that were never passed to a viewport callback.
    ///
    /// This is retained evidence, not replay authorization.
    pub fn unconsumed_inputs(&self) -> &[HostedViewportInput] {
        &self.unconsumed_inputs
    }

    /// Returns immediate viewport attempts observed by the transactional guard.
    pub fn immediate_viewport_violations(&self) -> &[HostedImmediateViewportViolation] {
        &self.immediate_viewport_violations
    }

    /// Separates the abort cause from every retained resource.
    pub fn into_parts(self) -> HostedViewportCycleAbortParts<T> {
        (
            self.error,
            self.staged_outputs,
            self.unconsumed_inputs,
            self.immediate_viewport_violations,
        )
    }
}

impl<T> std::fmt::Debug for HostedViewportCycleAbort<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostedViewportCycleAbort")
            .field("error", &self.error)
            .field("staged_output_count", &self.staged_outputs.len())
            .field("unconsumed_input_count", &self.unconsumed_inputs.len())
            .field(
                "immediate_viewport_violations",
                &self.immediate_viewport_violations,
            )
            .finish()
    }
}

impl<T> std::fmt::Display for HostedViewportCycleAbort<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl<T> std::error::Error for HostedViewportCycleAbort<T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Structural or application failure which aborts one hosted viewport cycle.
#[derive(Debug)]
pub enum HostedViewportCycleError {
    /// No physical viewport input was supplied.
    EmptyRoster,
    /// The physical root input was absent from the roster.
    MissingRootInput,
    /// Two inputs named the same viewport.
    DuplicateInput {
        /// Duplicated viewport identity.
        viewport_id: ViewportId,
    },
    /// The frozen native platform inventory omitted one hosted viewport input.
    NativeIngressMissingViewport {
        /// Hosted viewport absent from the native inventory.
        viewport_id: ViewportId,
    },
    /// A staged output no longer belongs to this cycle's exact native snapshot.
    NativeOutputAuthorityMismatch {
        /// Output whose native authority was replaced or replayed.
        viewport_id: ViewportId,
    },
    /// A hidden staging authorization did not match the frozen native roster.
    NativeStagingAuthorityMismatch {
        /// Viewport named by the invalid authorization.
        viewport_id: ViewportId,
    },
    /// Callback order named a viewport outside the frozen roster.
    UnexpectedCallback {
        /// Unexpected viewport identity.
        viewport_id: ViewportId,
    },
    /// Callback order named one viewport more than once.
    DuplicateCallback {
        /// Duplicated viewport identity.
        viewport_id: ViewportId,
    },
    /// Callback order omitted one frozen viewport.
    MissingCallback {
        /// First omitted viewport in canonical order.
        viewport_id: ViewportId,
    },
    /// The application rejected the complete input roster before UI began.
    BeginHook {
        /// Application error which aborted the cycle.
        source: crate::HostedViewportAppError,
    },
    /// The application rejected one viewport UI callback.
    ViewportUi {
        /// Viewport whose UI callback failed.
        viewport_id: ViewportId,
        /// Application error which aborted the cycle.
        source: crate::HostedViewportAppError,
    },
    /// The application rejected the complete staged output roster.
    EndHook {
        /// Application error which aborted the cycle.
        source: crate::HostedViewportAppError,
    },
    /// The application could not publish its prepared transaction after host seal.
    CommitHook {
        /// Application error which rejected the sealed host transaction.
        source: crate::HostedViewportAppError,
    },
    /// Transactional mode observed an immediate viewport attempt.
    ImmediateViewportAttempt {
        /// First immediate viewport rejected by the transaction guard.
        viewport_id: ViewportId,
    },
    /// The native host could not preserve a required cycle invariant.
    Runtime {
        /// Native runtime error which made the cycle unsafe to continue.
        source: crate::HostedViewportAppError,
    },
}

impl std::fmt::Display for HostedViewportCycleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyRoster => formatter.write_str("hosted viewport cycle roster is empty"),
            Self::MissingRootInput => {
                formatter.write_str("hosted viewport cycle roster has no physical root input")
            }
            Self::DuplicateInput { viewport_id } => {
                write!(
                    formatter,
                    "duplicate hosted input for viewport {viewport_id:?}"
                )
            }
            Self::NativeIngressMissingViewport { viewport_id } => write!(
                formatter,
                "native host ingress omits hosted viewport {viewport_id:?}"
            ),
            Self::NativeOutputAuthorityMismatch { viewport_id } => write!(
                formatter,
                "hosted output {viewport_id:?} does not match its frozen native authority"
            ),
            Self::NativeStagingAuthorityMismatch { viewport_id } => write!(
                formatter,
                "hidden staging authority does not match viewport {viewport_id:?}"
            ),
            Self::UnexpectedCallback { viewport_id } => write!(
                formatter,
                "hosted callback order contains unexpected viewport {viewport_id:?}"
            ),
            Self::DuplicateCallback { viewport_id } => write!(
                formatter,
                "hosted callback order contains duplicate viewport {viewport_id:?}"
            ),
            Self::MissingCallback { viewport_id } => write!(
                formatter,
                "hosted callback order omits viewport {viewport_id:?}"
            ),
            Self::BeginHook { source } => {
                write!(formatter, "hosted viewport begin hook failed: {source}")
            }
            Self::ViewportUi {
                viewport_id,
                source,
            } => write!(
                formatter,
                "hosted viewport {viewport_id:?} UI callback failed: {source}"
            ),
            Self::EndHook { source } => {
                write!(formatter, "hosted viewport end hook failed: {source}")
            }
            Self::CommitHook { source } => {
                write!(formatter, "hosted viewport commit hook failed: {source}")
            }
            Self::ImmediateViewportAttempt { viewport_id } => write!(
                formatter,
                "transactional hosted cycle rejected immediate viewport {viewport_id:?}"
            ),
            Self::Runtime { source } => {
                write!(formatter, "hosted viewport runtime failed: {source}")
            }
        }
    }
}

impl std::error::Error for HostedViewportCycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BeginHook { source }
            | Self::ViewportUi { source, .. }
            | Self::EndHook { source }
            | Self::CommitHook { source }
            | Self::Runtime { source } => Some(source.as_ref()),
            Self::EmptyRoster
            | Self::MissingRootInput
            | Self::DuplicateInput { .. }
            | Self::NativeIngressMissingViewport { .. }
            | Self::NativeOutputAuthorityMismatch { .. }
            | Self::NativeStagingAuthorityMismatch { .. }
            | Self::UnexpectedCallback { .. }
            | Self::DuplicateCallback { .. }
            | Self::MissingCallback { .. }
            | Self::ImmediateViewportAttempt { .. } => None,
        }
    }
}

impl From<std::io::Error> for HostedViewportCycleError {
    fn from(source: std::io::Error) -> Self {
        Self::Runtime {
            source: Box::new(source),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostedViewportOutputConsolidationError {
    MissingRootOutput,
    DuplicateOwnerOutput { viewport_id: ViewportId },
    MissingRootRecord,
    MissingOwnerRecord { viewport_id: ViewportId },
    MissingActiveRecord { viewport_id: ViewportId },
}

impl std::fmt::Display for HostedViewportOutputConsolidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRootOutput => {
                formatter.write_str("complete hosted cycle has no root output")
            }
            Self::DuplicateOwnerOutput { viewport_id } => write!(
                formatter,
                "complete hosted cycle has duplicate output owner {viewport_id:?}"
            ),
            Self::MissingRootRecord => {
                formatter.write_str("root hosted output has no root viewport record")
            }
            Self::MissingOwnerRecord { viewport_id } => write!(
                formatter,
                "physical hosted output owner {viewport_id:?} has no self record"
            ),
            Self::MissingActiveRecord { viewport_id } => write!(
                formatter,
                "active hosted viewport {viewport_id:?} lacks a complete output record"
            ),
        }
    }
}

impl std::error::Error for HostedViewportOutputConsolidationError {}

impl From<HostedViewportOutputConsolidationError> for HostedViewportCycleError {
    fn from(source: HostedViewportOutputConsolidationError) -> Self {
        Self::Runtime {
            source: Box::new(source),
        }
    }
}

/// One sealed viewport output whose presentation obligation is already armed.
///
/// Native backends must arm the complete output roster before invoking any
/// renderer or platform side effect. Dropping this value before an explicit
/// terminal outcome reports [`egui::PaintFailure::CoordinatorAborted`].
pub(super) struct ArmedHostedOutput {
    viewport_id: ViewportId,
    full_output: egui::FullOutput,
    native_staging_presentation: bool,
    pending_presentation: PendingPresentation,
}

impl ArmedHostedOutput {
    pub(super) fn into_parts(self) -> (ViewportId, egui::FullOutput, bool, PendingPresentation) {
        (
            self.viewport_id,
            self.full_output,
            self.native_staging_presentation,
            self.pending_presentation,
        )
    }
}

/// Arms every presentation token before a backend may perform side effects.
///
/// This is deliberately a roster-wide phase rather than work performed inside
/// a renderer loop. If rendering unwinds on an early viewport, the remaining
/// [`ArmedHostedOutput`] values still settle their tokens through RAII.
pub(super) fn arm_hosted_presentations(
    outputs: Vec<HostedViewportOutput<egui::FullOutput>>,
    presentation_results: &PresentationResults,
) -> Vec<ArmedHostedOutput> {
    outputs
        .into_iter()
        .map(|output| {
            let native_staging_presentation = output.is_native_staging_presentation();
            let native_binding = output.native_binding();
            let follow_up = output.presentation_result_follow_up();
            let (viewport_id, mut full_output) = output.into_parts();
            let pointer_hit_graph_candidate = pointer_hit_graph_candidate_for_hosted_output(
                viewport_id,
                full_output.pointer_hit_graph_candidate.take(),
                native_staging_presentation,
            );
            let pending_presentation = PendingPresentation::new(
                presentation_results.clone(),
                viewport_id,
                native_binding,
                full_output.platform_output.presentation_token.take(),
            )
            .with_follow_up_requirement(follow_up)
            .with_pointer_hit_graph_candidate(pointer_hit_graph_candidate);

            ArmedHostedOutput {
                viewport_id,
                full_output,
                native_staging_presentation,
                pending_presentation,
            }
        })
        .collect()
}

/// Applies texture sets in staged-output order and defers every free until all
/// outputs in the hosted cycle have reached a terminal paint outcome.
///
/// Glow and WGPU share one texture namespace across viewports. Keeping this
/// ordering in the coordinator prevents a later viewport's update from becoming
/// visible to an earlier viewport, while still allowing later outputs to reuse a
/// texture that an earlier output released.
#[derive(Default)]
pub(super) struct HostedTextureSynchronizer {
    deferred_frees: Vec<egui::TextureId>,
}

impl HostedTextureSynchronizer {
    pub(super) fn synchronize_output(
        &mut self,
        delta: &mut egui::TexturesDelta,
        mut set_texture: impl FnMut(egui::TextureId, &egui::epaint::ImageDelta),
    ) {
        for (texture_id, image_delta) in std::mem::take(&mut delta.set) {
            self.deferred_frees.retain(|free_id| *free_id != texture_id);
            set_texture(texture_id, &image_delta);
        }
        for texture_id in std::mem::take(&mut delta.free) {
            if !self.deferred_frees.contains(&texture_id) {
                self.deferred_frees.push(texture_id);
            }
        }
    }

    pub(super) fn into_deferred_frees(self) -> Vec<egui::TextureId> {
        self.deferred_frees
    }
}

pub(super) fn is_active_output_owner(
    active_viewports: &OrderedViewportIdMap<ViewportOutput>,
    viewport_id: ViewportId,
) -> bool {
    active_viewports.contains_key(&viewport_id)
}

pub(super) fn is_native_staging_surface(
    visible: Option<bool>,
    minimized: Option<bool>,
    surface_size: [u32; 2],
) -> bool {
    visible.is_some() && minimized == Some(false) && surface_size[0] > 0 && surface_size[1] > 0
}

pub(super) fn should_present_hosted_output(
    is_visible: bool,
    is_native_staging_presentation: bool,
) -> bool {
    is_visible || is_native_staging_presentation
}

pub(super) fn pointer_hit_graph_candidate_for_hosted_output(
    viewport_id: ViewportId,
    candidate: Option<egui::PointerHitGraphCandidate>,
    is_native_staging_presentation: bool,
) -> Option<egui::PointerHitGraphCandidate> {
    if !is_native_staging_presentation {
        return candidate;
    }
    if let Some(candidate) = candidate {
        candidate.settle_for(
            viewport_id,
            &egui::PaintOutcome::Skipped(egui::PaintSkipReason::NotVisible),
        );
    }
    None
}

pub(super) fn consolidate_hosted_viewport_outputs(
    outputs: &[(ViewportId, OrderedViewportIdMap<ViewportOutput>)],
) -> Result<OrderedViewportIdMap<ViewportOutput>, HostedViewportOutputConsolidationError> {
    let mut output_by_owner = BTreeMap::new();
    for (index, (viewport_id, _)) in outputs.iter().enumerate() {
        if output_by_owner.insert(*viewport_id, index).is_some() {
            return Err(
                HostedViewportOutputConsolidationError::DuplicateOwnerOutput {
                    viewport_id: *viewport_id,
                },
            );
        }
    }
    let root_output_index = output_by_owner
        .get(&ViewportId::ROOT)
        .copied()
        .ok_or(HostedViewportOutputConsolidationError::MissingRootOutput)?;
    if !outputs[root_output_index].1.contains_key(&ViewportId::ROOT) {
        return Err(HostedViewportOutputConsolidationError::MissingRootRecord);
    }
    for (viewport_id, viewport_outputs) in outputs {
        if *viewport_id == ViewportId::ROOT {
            continue;
        }
        if !viewport_outputs.contains_key(viewport_id) {
            return Err(HostedViewportOutputConsolidationError::MissingOwnerRecord {
                viewport_id: *viewport_id,
            });
        }
    }

    // A physical callback is authoritative for its direct children. Immediate
    // descendants inherit the nearest physical callback output because they
    // execute recursively inside that callback.
    let mut active_sources = BTreeMap::from([(ViewportId::ROOT, root_output_index)]);
    let mut pending = VecDeque::from([(ViewportId::ROOT, root_output_index)]);
    while let Some((parent_id, inherited_output_index)) = pending.pop_front() {
        let output_index = output_by_owner
            .get(&parent_id)
            .copied()
            .unwrap_or(inherited_output_index);
        for (viewport_id, viewport_output) in &outputs[output_index].1 {
            if *viewport_id == parent_id || viewport_output.parent != parent_id {
                continue;
            }
            if let std::collections::btree_map::Entry::Vacant(entry) =
                active_sources.entry(*viewport_id)
            {
                entry.insert(output_index);
                pending.push_back((*viewport_id, output_index));
            }
        }
    }

    let mut consolidated = OrderedViewportIdMap::new();
    for (_, viewport_outputs) in outputs {
        for viewport_id in active_sources.keys() {
            let Some(newer) = viewport_outputs.get(viewport_id).cloned() else {
                continue;
            };
            match consolidated.entry(*viewport_id) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(newer);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    entry.get_mut().append(newer);
                }
            }
        }
    }
    if let Some(viewport_id) = active_sources
        .keys()
        .find(|viewport_id| !consolidated.contains_key(viewport_id))
    {
        return Err(
            HostedViewportOutputConsolidationError::MissingActiveRecord {
                viewport_id: *viewport_id,
            },
        );
    }
    Ok(consolidated)
}

#[derive(Debug)]
pub(super) struct PreparedNativeViewportSchedule {
    request: crate::native::platform_provider::NativeViewportCreateRequest,
    accepted: bool,
}

impl PreparedNativeViewportSchedule {
    pub(super) fn commit_schedule(
        self,
        active_viewports: &mut OrderedViewportIdMap<ViewportOutput>,
        sink: &crate::NativeViewportCreateSink,
    ) {
        let parent_remains_active =
            active_viewports.contains_key(&self.request.parent.viewport_id());
        let target_remains_active = active_viewports.contains_key(&self.request.viewport_id);
        if self.accepted
            && parent_remains_active
            && target_remains_active
            && sink.accept_schedule(&self.request)
        {
            return;
        }
        if self.accepted {
            active_viewports.remove(&self.request.viewport_id);
        }
        if !parent_remains_active || !target_remains_active || !self.accepted {
            sink.report_terminal(
                &self.request,
                crate::NativeViewportCreateDispatchOutcome::Rejected,
            );
        }
    }

    pub(super) fn cancel(self, sink: &crate::NativeViewportCreateSink) {
        sink.cancel_request(&self.request);
    }
}

pub(super) fn schedule_native_viewport_creates(
    active_viewports: &mut OrderedViewportIdMap<ViewportOutput>,
    sink: &crate::NativeViewportCreateSink,
) -> Vec<PreparedNativeViewportSchedule> {
    sink.close_and_drain()
        .into_iter()
        .map(|request| {
            let parent_is_active = active_viewports.contains_key(&request.parent.viewport_id());
            let target_is_free = !active_viewports.contains_key(&request.viewport_id);
            let accepted = if parent_is_active && target_is_free {
                active_viewports.insert(
                    request.viewport_id,
                    ViewportOutput {
                        parent: request.parent.viewport_id(),
                        class: egui::ViewportClass::Deferred,
                        builder: request.builder.clone(),
                        viewport_ui_cb: Some(Arc::clone(&request.viewport_ui_cb)),
                        commands: Vec::new(),
                        repaint_delay: std::time::Duration::ZERO,
                    },
                );
                true
            } else {
                false
            };
            PreparedNativeViewportSchedule { request, accepted }
        })
        .collect()
}

fn native_ingress_record_matches_binding(
    record: &crate::NativeIngressRecord,
    expected: crate::NativeViewportBinding,
) -> bool {
    match record.event() {
        crate::NativeIngressEvent::PointerEdge(edge) => matches!(
            edge.source(),
            crate::NativePointerSource::Viewport(binding)
                if binding == expected
        ),
        crate::NativeIngressEvent::KeyEdge(edge) => edge.binding() == expected,
        crate::NativeIngressEvent::AccessibilityEdge(edge) => edge.binding() == expected,
        crate::NativeIngressEvent::CloseObservation(observation) => {
            observation.binding() == expected
        }
        crate::NativeIngressEvent::EffectResult(_)
        | crate::NativeIngressEvent::PresentationResult(_)
        | crate::NativeIngressEvent::Retirement(_)
        | crate::NativeIngressEvent::BindingIngressQuiesced(_)
        | crate::NativeIngressEvent::ViewportCreateResult(_)
        | crate::NativeIngressEvent::PlatformSnapshot(_) => false,
    }
}

impl HostedViewportCycle {
    /// Freezes one exact native input roster.
    ///
    /// # Errors
    ///
    /// Returns [`HostedViewportCycleError::EmptyRoster`] when no input is
    /// supplied, [`HostedViewportCycleError::MissingRootInput`] when the exact
    /// roster omits [`ViewportId::ROOT`], or
    /// [`HostedViewportCycleError::DuplicateInput`] when two inputs name the
    /// same viewport.
    pub fn new(
        inputs: impl IntoIterator<Item = RawInput>,
    ) -> Result<Self, HostedViewportCycleError> {
        Self::from_parts(inputs, None, BTreeSet::new(), None, None)
    }

    #[cfg(test)]
    pub(crate) fn with_native_ingress(
        inputs: impl IntoIterator<Item = RawInput>,
        native_ingress: crate::NativeHostIngress,
    ) -> Result<Self, HostedViewportCycleError> {
        Self::with_native_ingress_and_native_staging(inputs, native_ingress, [])
    }

    #[cfg(test)]
    pub(crate) fn with_native_ingress_and_native_staging(
        inputs: impl IntoIterator<Item = RawInput>,
        native_ingress: crate::NativeHostIngress,
        native_staging_presentations: impl IntoIterator<Item = crate::NativeViewportBinding>,
    ) -> Result<Self, HostedViewportCycleError> {
        Self::from_parts(
            inputs,
            Some(Arc::new(native_ingress)),
            native_staging_presentations.into_iter().collect(),
            None,
            None,
        )
    }

    pub(crate) fn with_native_runtime_ingress(
        inputs: impl IntoIterator<Item = RawInput>,
        native_ingress: crate::NativeHostIngress,
        native_staging_presentations: impl IntoIterator<Item = crate::NativeViewportBinding>,
        native_effect_sink: crate::NativeEffectSink,
        native_viewport_create_sink: crate::NativeViewportCreateSink,
    ) -> Result<Self, HostedViewportCycleError> {
        Self::from_parts(
            inputs,
            Some(Arc::new(native_ingress)),
            native_staging_presentations.into_iter().collect(),
            Some(native_effect_sink),
            Some(native_viewport_create_sink),
        )
    }

    fn from_parts(
        inputs: impl IntoIterator<Item = RawInput>,
        native_ingress: Option<Arc<crate::NativeHostIngress>>,
        native_staging_presentations: BTreeSet<crate::NativeViewportBinding>,
        native_effect_sink: Option<crate::NativeEffectSink>,
        native_viewport_create_sink: Option<crate::NativeViewportCreateSink>,
    ) -> Result<Self, HostedViewportCycleError> {
        let mut roster = BTreeMap::new();
        for input in inputs {
            let viewport_id = input.viewport_id;
            if roster.insert(viewport_id, input).is_some() {
                return Err(HostedViewportCycleError::DuplicateInput { viewport_id });
            }
        }
        if roster.is_empty() {
            return Err(HostedViewportCycleError::EmptyRoster);
        }
        if !roster.contains_key(&ViewportId::ROOT) {
            return Err(HostedViewportCycleError::MissingRootInput);
        }
        if let Some(native_ingress) = native_ingress.as_deref() {
            let native_viewports = native_ingress
                .platform()
                .inventory()
                .iter()
                .map(|binding| binding.viewport_id())
                .collect::<BTreeSet<_>>();
            if let Some(viewport_id) = roster
                .keys()
                .find(|viewport_id| !native_viewports.contains(viewport_id))
            {
                return Err(HostedViewportCycleError::NativeIngressMissingViewport {
                    viewport_id: *viewport_id,
                });
            }
            for binding in &native_staging_presentations {
                let snapshot = native_ingress
                    .platform()
                    .windows()
                    .iter()
                    .find(|snapshot| snapshot.binding() == *binding);
                let matches_staging_roster = roster.contains_key(&binding.viewport_id())
                    && snapshot.is_some_and(|snapshot| {
                        snapshot
                            .presentation()
                            .value()
                            .value()
                            .is_some_and(|state| {
                                matches!(
                                    state,
                                    crate::NativePresentationState::Hidden
                                        | crate::NativePresentationState::Visible
                                )
                            })
                    });
                if !matches_staging_roster {
                    return Err(HostedViewportCycleError::NativeStagingAuthorityMismatch {
                        viewport_id: binding.viewport_id(),
                    });
                }
            }
        } else if let Some(binding) = native_staging_presentations.iter().next() {
            return Err(HostedViewportCycleError::NativeStagingAuthorityMismatch {
                viewport_id: binding.viewport_id(),
            });
        }
        Ok(Self {
            inputs: roster,
            native_ingress,
            claimed_native_event_envelopes: Arc::default(),
            settled_native_scroll_edges: Arc::default(),
            native_effect_sink,
            native_viewport_create_sink,
            native_staging_presentations: Arc::new(native_staging_presentations),
            issued_native_staging_presentations: Arc::default(),
        })
    }

    /// Returns the number of frozen physical viewport inputs.
    pub fn len(&self) -> usize {
        self.inputs.len()
    }

    /// Returns whether the frozen roster is empty.
    ///
    /// A successfully constructed cycle is never empty; this accessor exists
    /// for ordinary collection-style inspection.
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }

    /// Iterates over every frozen input in canonical viewport order.
    pub fn inputs(&self) -> impl ExactSizeIterator<Item = (ViewportId, &RawInput)> {
        self.inputs
            .iter()
            .map(|(viewport_id, input)| (*viewport_id, input))
    }

    /// Returns the atomic native platform ingress frozen before this cycle.
    ///
    /// Manually constructed cycles and non-native harnesses do not carry native
    /// ingress. A native Glow or WGPU runtime supplies one before invoking raw
    /// input hooks or any viewport UI callback. Its native inventory must cover
    /// every input in this hosted roster. Compatibility-mode immediate windows
    /// may appear as additional inventory entries because their UI remains
    /// outside the hosted-cycle barrier.
    pub fn native_host_ingress(&self) -> Option<&crate::NativeHostIngress> {
        self.native_ingress.as_deref()
    }

    /// Validate one affine envelope claim against this exact native host cycle.
    ///
    /// Validation requires the process-local identity retained by this viewport's frozen
    /// [`RawInput`], the exact viewport incarnation, and one unique correlation in the
    /// coordinator-owned native ingress journal. The returned record remains typed so the
    /// consumer can additionally require the event lane it expects.
    pub fn validate_native_event_envelope_claim(
        &self,
        binding: crate::NativeViewportBinding,
        claim: egui::EventEnvelopeClaim,
    ) -> Option<crate::NativeEventEnvelopeReceipt<'_>> {
        let viewport_id = binding.viewport_id();
        let input = self.inputs.get(&viewport_id)?;
        if !input.contains_event_envelope_claim(&claim) {
            return None;
        }
        let raw_event_index = claim.raw_event_index();
        let receipt = self
            .native_ingress
            .as_deref()?
            .validate_event_envelope_claim(claim)?;
        if !native_ingress_record_matches_binding(receipt.record(), binding) {
            return None;
        }
        if !self
            .claimed_native_event_envelopes
            .lock()
            .insert((viewport_id, raw_event_index))
        {
            return None;
        }
        Some(receipt)
    }

    /// Affinely settles the egui wheel derivative of one exact native scroll edge.
    ///
    /// A successful settlement either removes the one required correlated derivative before the
    /// callback viewport begins its egui pass or consumes a provider-minted explicit-absence
    /// proof. The provider-owned pointer sequence identifies the ingress record even after its
    /// semantic owner retires or a terminal edge becomes bindingless. Required derivatives remain
    /// bound by backend event correlation across the complete callback roster. Merely failing to
    /// find a wheel event is not evidence of absence. Both outcomes prevent a native protocol
    /// consumer and egui's `WheelState` from consuming the same physical sample.
    /// Unknown, ambiguous, foreign, mismatched, and repeated settlements fail closed.
    pub fn claim_native_scroll_derivative(
        &self,
        pointer_sequence: crate::NativePointerSequence,
    ) -> bool {
        let Some(ingress) = self.native_ingress.as_deref() else {
            return false;
        };
        let mut records = ingress.ordered().records().iter().filter(|record| {
            matches!(
                record.event(),
                crate::NativeIngressEvent::PointerEdge(edge)
                    if edge.sequence() == pointer_sequence
                        && matches!(edge.kind(), crate::NativePointerEdgeKind::Scrolled(_))
            )
        });
        let Some(record) = records.next() else {
            return false;
        };
        if records.next().is_some() {
            return false;
        }
        let Some(disposition) = record.scroll_derivative_disposition() else {
            return false;
        };
        let mut derivatives =
            record
                .backend_event_sequence()
                .into_iter()
                .flat_map(|backend_sequence| {
                    self.inputs.iter().flat_map(move |(viewport_id, input)| {
                        input
                            .events
                            .iter()
                            .enumerate()
                            .filter_map(move |(index, envelope)| {
                                (envelope.correlation().sequence() == Some(backend_sequence)
                                    && matches!(envelope.event(), egui::Event::MouseWheel { .. }))
                                .then_some((*viewport_id, index))
                            })
                    })
                });
        let derivative = derivatives.next();
        if derivatives.next().is_some() {
            return false;
        }
        match disposition {
            crate::native::platform_provider::NativeScrollDerivativeDisposition::RequiredDerivative
                if derivative.is_none() || record.backend_event_sequence().is_none() =>
            {
                return false;
            }
            crate::native::platform_provider::NativeScrollDerivativeDisposition::ExplicitNoDerivative(
                _,
            ) if derivative.is_some() => {
                return false;
            }
            _ => {}
        }
        let mut settled_scroll_edges = self.settled_native_scroll_edges.lock();
        if !settled_scroll_edges.insert(pointer_sequence) {
            return false;
        }
        let Some((derivative_viewport, raw_event_index)) = derivative else {
            return matches!(
                disposition,
                crate::native::platform_provider::NativeScrollDerivativeDisposition::ExplicitNoDerivative(
                    _,
                )
            );
        };
        if self
            .claimed_native_event_envelopes
            .lock()
            .insert((derivative_viewport, raw_event_index))
        {
            true
        } else {
            settled_scroll_edges.remove(&pointer_sequence);
            false
        }
    }

    fn take_viewport_input(&mut self, viewport_id: ViewportId) -> Option<RawInput> {
        let mut input = self.inputs.remove(&viewport_id)?;
        let claimed = self.claimed_native_event_envelopes.lock();
        if claimed.iter().any(|(viewport, _)| *viewport == viewport_id) {
            input.events = input
                .events
                .into_iter()
                .enumerate()
                .filter_map(|(index, envelope)| {
                    (!claimed.contains(&(viewport_id, index))).then_some(envelope)
                })
                .collect();
        }
        drop(claimed);
        Some(input)
    }

    /// Returns the cycle-scoped sink for exact native window operations.
    ///
    /// A native backend closes this sink after the output transaction seals.
    /// Submitting an operation reserves its coordinator lane but neither the
    /// submission nor its later dispatch result acknowledges platform state.
    pub const fn native_effect_sink(&self) -> Option<&crate::NativeEffectSink> {
        self.native_effect_sink.as_ref()
    }

    /// Return the cycle-scoped lane for scheduling an unbound native viewport.
    pub const fn native_viewport_create_sink(&self) -> Option<&crate::NativeViewportCreateSink> {
        self.native_viewport_create_sink.as_ref()
    }

    /// Takes the one native staging presentation authorized for `viewport_id`
    /// in this exact native host cycle.
    ///
    /// The authorization is affine: clones of this cycle share the same issue
    /// ledger, and an authorization cannot be reused by a later snapshot or a
    /// recreated native viewport.
    ///
    /// # Errors
    ///
    /// Returns [`HostedNativeStagingPresentationError`] when native ingress is
    /// absent, the viewport is outside the frozen roster, the backend did not
    /// prove a stageable surface, or the authorization was already
    /// issued.
    pub fn take_native_staging_presentation(
        &self,
        viewport_id: ViewportId,
    ) -> Result<HostedNativeStagingPresentation, HostedNativeStagingPresentationError> {
        let authority = self.native_output_authority(viewport_id).ok_or_else(|| {
            if self.native_ingress.is_some() {
                HostedNativeStagingPresentationError::ViewportUnavailable { viewport_id }
            } else {
                HostedNativeStagingPresentationError::NativeIngressUnavailable
            }
        })?;
        if !self
            .native_staging_presentations
            .contains(&authority.binding)
        {
            return Err(HostedNativeStagingPresentationError::SurfaceUnavailable {
                binding: authority.binding,
            });
        }
        if !self
            .issued_native_staging_presentations
            .lock()
            .insert(authority)
        {
            return Err(HostedNativeStagingPresentationError::AlreadyAuthorized {
                binding: authority.binding,
            });
        }
        Ok(HostedNativeStagingPresentation { authority })
    }

    /// Runs one complete hosted cycle.
    ///
    /// `on_begin` observes the complete immutable input roster before the first
    /// viewport callback. `on_end` observes every callback output only after the
    /// exact roster has run. Invalid callback orders are rejected before either
    /// hook or any callback executes.
    ///
    /// # Errors
    ///
    /// Returns a structural callback-order error when the supplied order is not
    /// an exact permutation of the frozen roster. No hook or callback runs in
    /// that case. Application failures identify whether begin, one viewport UI,
    /// or end rejected the cycle; no later phase runs after such a failure.
    pub fn run<T>(
        self,
        callback_order: impl IntoIterator<Item = ViewportId>,
        on_begin: impl FnOnce(&Self) -> crate::HostedViewportAppResult<()>,
        run_viewport: impl FnMut(HostedViewportInput) -> crate::HostedViewportAppResult<T>,
        on_end: impl FnOnce(&mut [HostedViewportOutput<T>]) -> crate::HostedViewportAppResult<()>,
    ) -> Result<Vec<HostedViewportOutput<T>>, HostedViewportCycleAbort<T>> {
        self.run_with_mode(
            crate::HostedViewportMode::Compatibility,
            callback_order,
            on_begin,
            run_viewport,
            on_end,
        )
    }

    /// Runs one complete hosted cycle under the selected immediate-viewport
    /// contract.
    ///
    /// Transactional mode installs a thread-local guard for all three phases.
    /// An immediate renderer must call [`check_hosted_immediate_viewport`] before
    /// any side effect. The first recorded attempt aborts the cycle before its
    /// next phase, while retaining outputs and unconsumed inputs.
    ///
    /// # Errors
    ///
    /// Returns [`HostedViewportCycleAbort`] for structural, application, or
    /// transactional failures. Once viewport UI begins, retained inputs are
    /// diagnostic evidence and must not be replayed.
    pub fn run_with_mode<T>(
        mut self,
        mode: crate::HostedViewportMode,
        callback_order: impl IntoIterator<Item = ViewportId>,
        on_begin: impl FnOnce(&Self) -> crate::HostedViewportAppResult<()>,
        mut run_viewport: impl FnMut(HostedViewportInput) -> crate::HostedViewportAppResult<T>,
        on_end: impl FnOnce(&mut [HostedViewportOutput<T>]) -> crate::HostedViewportAppResult<()>,
    ) -> Result<Vec<HostedViewportOutput<T>>, HostedViewportCycleAbort<T>> {
        let callback_order = callback_order.into_iter().collect::<Vec<_>>();
        if let Err(error) = Self::validate_callback_order(&self.inputs, &callback_order) {
            return Err(self.into_abort(error, Vec::new(), Vec::new()));
        }

        let guard = HostedViewportTransactionGuard::enter(mode);
        if let Err(source) = on_begin(&self) {
            let violations = guard.finish();
            return Err(self.into_abort(
                HostedViewportCycleError::BeginHook { source },
                Vec::new(),
                violations,
            ));
        }
        if let Some(violation) = guard.first_violation() {
            let violations = guard.finish();
            return Err(self.into_abort(
                HostedViewportCycleError::ImmediateViewportAttempt {
                    viewport_id: violation.viewport_id(),
                },
                Vec::new(),
                violations,
            ));
        }

        let mut outputs = Vec::with_capacity(callback_order.len());
        for viewport_id in callback_order {
            let native_authority = self.native_output_authority(viewport_id);
            let Some(raw_input) = self.take_viewport_input(viewport_id) else {
                let violations = guard.finish();
                return Err(self.into_abort(
                    HostedViewportCycleError::MissingCallback { viewport_id },
                    outputs,
                    violations,
                ));
            };
            let input = HostedViewportInput {
                viewport_id,
                raw_input,
            };
            match run_viewport(input) {
                Ok(output) => outputs.push(HostedViewportOutput::with_native_authority(
                    viewport_id,
                    output,
                    native_authority,
                )),
                Err(source) => {
                    let violations = guard.finish();
                    return Err(self.into_abort(
                        HostedViewportCycleError::ViewportUi {
                            viewport_id,
                            source,
                        },
                        outputs,
                        violations,
                    ));
                }
            }
            if let Some(violation) = guard.first_violation() {
                let violations = guard.finish();
                return Err(self.into_abort(
                    HostedViewportCycleError::ImmediateViewportAttempt {
                        viewport_id: violation.viewport_id(),
                    },
                    outputs,
                    violations,
                ));
            }
        }
        if let Err(source) = on_end(&mut outputs) {
            let violations = guard.finish();
            return Err(self.into_abort(
                HostedViewportCycleError::EndHook { source },
                outputs,
                violations,
            ));
        }
        if let Some(violation) = guard.first_violation() {
            let violations = guard.finish();
            return Err(self.into_abort(
                HostedViewportCycleError::ImmediateViewportAttempt {
                    viewport_id: violation.viewport_id(),
                },
                outputs,
                violations,
            ));
        }
        if let Err(error) = self.validate_output_authorities(&outputs) {
            let violations = guard.finish();
            return Err(self.into_abort(error, outputs, violations));
        }
        let violations = guard.finish();
        debug_assert!(
            violations.is_empty(),
            "a transactional violation must abort before the cycle succeeds"
        );
        Ok(outputs)
    }

    /// Drives one complete hosted cycle through a single mutable owner.
    ///
    /// The callback order must be an exact permutation of the frozen roster.
    /// Structural errors are returned before the driver observes any phase.
    ///
    /// # Errors
    ///
    /// Returns a structural callback-order error when the supplied order is not
    /// an exact permutation of the frozen roster. The driver remains untouched
    /// in that case. Driver failures identify the phase which aborted the cycle,
    /// and suppress every later phase.
    pub fn drive<D>(
        self,
        callback_order: impl IntoIterator<Item = ViewportId>,
        driver: &mut D,
    ) -> Result<Vec<HostedViewportOutput<D::Output>>, HostedViewportCycleAbort<D::Output>>
    where
        D: HostedViewportCycleDriver,
    {
        self.drive_with_mode(
            crate::HostedViewportMode::Compatibility,
            callback_order,
            driver,
        )
    }

    /// Drives one complete hosted cycle under the selected immediate-viewport
    /// contract.
    ///
    /// This is the single-owner counterpart to [`Self::run_with_mode`].
    /// Compatibility mode is equivalent to [`Self::drive`].
    ///
    /// # Errors
    ///
    /// Returns [`HostedViewportCycleAbort`] without discarding earlier outputs
    /// or inputs that were never passed to the driver.
    pub fn drive_with_mode<D>(
        self,
        mode: crate::HostedViewportMode,
        callback_order: impl IntoIterator<Item = ViewportId>,
        driver: &mut D,
    ) -> Result<Vec<HostedViewportOutput<D::Output>>, HostedViewportCycleAbort<D::Output>>
    where
        D: HostedViewportCycleDriver,
    {
        self.drive_with_guard(
            HostedViewportTransactionGuard::enter(mode),
            callback_order,
            driver,
        )
        .map(HostedViewportCycleCompletion::into_outputs)
    }

    pub(crate) fn drive_with_guard<D>(
        mut self,
        guard: HostedViewportTransactionGuard,
        callback_order: impl IntoIterator<Item = ViewportId>,
        driver: &mut D,
    ) -> Result<HostedViewportCycleCompletion<D::Output>, HostedViewportCycleAbort<D::Output>>
    where
        D: HostedViewportCycleDriver,
    {
        let callback_order = callback_order.into_iter().collect::<Vec<_>>();
        if let Err(error) = Self::validate_callback_order(&self.inputs, &callback_order) {
            return Err(self.into_abort(error, Vec::new(), Vec::new()));
        }

        if let Some(violation) = guard.first_violation() {
            let violations = guard.finish();
            return Err(self.into_abort(
                HostedViewportCycleError::ImmediateViewportAttempt {
                    viewport_id: violation.viewport_id(),
                },
                Vec::new(),
                violations,
            ));
        }
        if let Err(source) = driver.begin(&self) {
            return Err(self.abort_driven_cycle(
                driver,
                guard,
                HostedViewportCycleError::BeginHook { source },
                Vec::new(),
            ));
        }
        if let Some(violation) = guard.first_violation() {
            return Err(self.abort_driven_cycle(
                driver,
                guard,
                HostedViewportCycleError::ImmediateViewportAttempt {
                    viewport_id: violation.viewport_id(),
                },
                Vec::new(),
            ));
        }

        let mut outputs = Vec::with_capacity(callback_order.len());
        for viewport_id in callback_order {
            let native_authority = self.native_output_authority(viewport_id);
            let Some(raw_input) = self.take_viewport_input(viewport_id) else {
                return Err(self.abort_driven_cycle(
                    driver,
                    guard,
                    HostedViewportCycleError::MissingCallback { viewport_id },
                    outputs,
                ));
            };
            let input = HostedViewportInput {
                viewport_id,
                raw_input,
            };
            match driver.run_viewport(input) {
                Ok(output) => outputs.push(HostedViewportOutput::with_native_authority(
                    viewport_id,
                    output,
                    native_authority,
                )),
                Err(source) => {
                    return Err(self.abort_driven_cycle(
                        driver,
                        guard,
                        HostedViewportCycleError::ViewportUi {
                            viewport_id,
                            source,
                        },
                        outputs,
                    ));
                }
            }
            if let Some(violation) = guard.first_violation() {
                return Err(self.abort_driven_cycle(
                    driver,
                    guard,
                    HostedViewportCycleError::ImmediateViewportAttempt {
                        viewport_id: violation.viewport_id(),
                    },
                    outputs,
                ));
            }
        }
        if let Err(source) = driver.end(&mut outputs) {
            return Err(self.abort_driven_cycle(
                driver,
                guard,
                HostedViewportCycleError::EndHook { source },
                outputs,
            ));
        }
        if let Some(violation) = guard.first_violation() {
            return Err(self.abort_driven_cycle(
                driver,
                guard,
                HostedViewportCycleError::ImmediateViewportAttempt {
                    viewport_id: violation.viewport_id(),
                },
                outputs,
            ));
        }
        if let Err(error) = self.validate_output_authorities(&outputs) {
            return Err(self.abort_driven_cycle(driver, guard, error, outputs));
        }
        debug_assert!(
            guard.violations().is_empty(),
            "a transactional violation must abort before the cycle succeeds"
        );
        Ok(HostedViewportCycleCompletion { outputs, guard })
    }

    fn abort_driven_cycle<D>(
        self,
        driver: &mut D,
        guard: HostedViewportTransactionGuard,
        error: HostedViewportCycleError,
        staged_outputs: Vec<HostedViewportOutput<D::Output>>,
    ) -> HostedViewportCycleAbort<D::Output>
    where
        D: HostedViewportCycleDriver,
    {
        // Application cleanup is part of the transaction. Releasing the guard
        // first would let an abort hook create an immediate native viewport.
        driver.abort();
        let violations = guard.finish();
        self.into_abort(error, staged_outputs, violations)
    }

    fn native_output_authority(
        &self,
        viewport_id: ViewportId,
    ) -> Option<HostedNativeOutputAuthority> {
        let platform = self.native_ingress.as_deref()?.platform();
        let binding = platform
            .inventory()
            .iter()
            .copied()
            .find(|binding| binding.viewport_id() == viewport_id)?;
        platform
            .windows()
            .iter()
            .any(|snapshot| snapshot.binding() == binding)
            .then_some(HostedNativeOutputAuthority {
                binding,
                snapshot_generation: platform.snapshot_generation(),
            })
    }

    fn validate_output_authorities<T>(
        &self,
        outputs: &[HostedViewportOutput<T>],
    ) -> Result<(), HostedViewportCycleError> {
        for output in outputs {
            let expected = self.native_output_authority(output.viewport_id);
            if output.native_authority != expected
                || output
                    .native_staging_presentation
                    .is_some_and(|authority| Some(authority) != expected)
            {
                return Err(HostedViewportCycleError::NativeOutputAuthorityMismatch {
                    viewport_id: output.viewport_id,
                });
            }
        }
        Ok(())
    }

    fn into_abort<T>(
        self,
        error: HostedViewportCycleError,
        staged_outputs: Vec<HostedViewportOutput<T>>,
        immediate_viewport_violations: Vec<HostedImmediateViewportViolation>,
    ) -> HostedViewportCycleAbort<T> {
        if let Some(effect_sink) = self.native_effect_sink.as_ref() {
            effect_sink.close_and_cancel();
        }
        if let Some(create_sink) = self.native_viewport_create_sink.as_ref() {
            create_sink.close_and_cancel();
        }
        let unconsumed_inputs = self
            .inputs
            .into_iter()
            .map(|(viewport_id, raw_input)| HostedViewportInput {
                viewport_id,
                raw_input,
            })
            .collect();
        HostedViewportCycleAbort {
            error,
            staged_outputs,
            unconsumed_inputs,
            immediate_viewport_violations,
        }
    }

    fn validate_callback_order(
        inputs: &BTreeMap<ViewportId, RawInput>,
        callback_order: &[ViewportId],
    ) -> Result<(), HostedViewportCycleError> {
        let mut seen = BTreeSet::new();
        for viewport_id in callback_order {
            if !inputs.contains_key(viewport_id) {
                return Err(HostedViewportCycleError::UnexpectedCallback {
                    viewport_id: *viewport_id,
                });
            }
            if !seen.insert(*viewport_id) {
                return Err(HostedViewportCycleError::DuplicateCallback {
                    viewport_id: *viewport_id,
                });
            }
        }
        if let Some(viewport_id) = inputs
            .keys()
            .find(|viewport_id| !seen.contains(viewport_id))
        {
            return Err(HostedViewportCycleError::MissingCallback {
                viewport_id: *viewport_id,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, sync::Arc};

    use egui::{
        BackendEventDerivation, BackendEventSequence, Event, Modifiers, OrderedViewportIdMap,
        PointerButton, Pos2, ViewportBuilder, ViewportClass, ViewportOutput,
    };

    use super::*;

    struct RecordingDriver {
        phases: Vec<&'static str>,
        child: ViewportId,
    }

    impl HostedViewportCycleDriver for RecordingDriver {
        type Output = ViewportId;

        fn begin(&mut self, cycle: &HostedViewportCycle) -> crate::HostedViewportAppResult<()> {
            assert_eq!(cycle.len(), 2);
            self.phases.push("begin");
            Ok(())
        }

        fn run_viewport(
            &mut self,
            input: HostedViewportInput,
        ) -> crate::HostedViewportAppResult<Self::Output> {
            assert_eq!(self.phases.first(), Some(&"begin"));
            assert!(!self.phases.contains(&"end"));
            self.phases.push(if input.viewport_id() == self.child {
                "child"
            } else {
                "root"
            });
            Ok(input.viewport_id())
        }

        fn end(
            &mut self,
            outputs: &mut [HostedViewportOutput<Self::Output>],
        ) -> crate::HostedViewportAppResult<()> {
            assert_eq!(outputs.len(), 2);
            self.phases.push("end");
            Ok(())
        }
    }

    fn input(viewport_id: ViewportId, sequence: u128, event: Event) -> RawInput {
        let mut derivation = BackendEventDerivation::known(BackendEventSequence::new(sequence));
        RawInput {
            viewport_id,
            events: vec![derivation.envelope(event)],
            ..Default::default()
        }
    }

    #[test]
    fn presentation_follow_up_requires_and_preserves_an_exact_renderer_token() {
        let mut output = HostedViewportOutput::new(ViewportId::ROOT, egui::FullOutput::default());
        assert_eq!(
            output.require_presentation_result_follow_up(
                HostedPresentationFollowUp::SuccessfulSubmission,
            ),
            Err(HostedPresentationFollowUpError::PresentationTokenMissing {
                viewport_id: ViewportId::ROOT,
            })
        );

        output.output_mut().platform_output.presentation_token =
            Some(egui::UserData::new("follow-up"));
        output
            .require_presentation_result_follow_up(HostedPresentationFollowUp::SuccessfulSubmission)
            .expect("an exact renderer token can request one follow-up cycle");
        assert_eq!(
            output.presentation_result_follow_up(),
            Some(HostedPresentationFollowUp::SuccessfulSubmission)
        );
        output
            .require_presentation_result_follow_up(HostedPresentationFollowUp::AnyResult)
            .expect("a terminal requirement can strengthen the same renderer token");
        assert_eq!(
            output.presentation_result_follow_up(),
            Some(HostedPresentationFollowUp::AnyResult)
        );
    }

    #[test]
    fn provisional_viewport_create_reaches_host_schedule_and_aborts_silently() {
        let coordinator = Arc::new(egui::mutex::Mutex::new(
            crate::native::platform_provider::NativePlatformCoordinator::default(),
        ));
        let parent = coordinator
            .lock()
            .register_viewport(ViewportId::ROOT)
            .expect("the root native binding is available");
        let target = ViewportId::from_hash_of("provisional-native-create");
        let sink = crate::NativeViewportCreateSink::new(Arc::clone(&coordinator), [parent]);
        sink.submit(
            parent,
            target,
            ViewportBuilder::default().with_visible(false),
            Arc::new(|_| {}),
            crate::NativeViewportCreateCorrelation::new(egui::UserData::new("candidate")),
        )
        .expect("the application prepare phase reserves the create lane");
        let mut active = OrderedViewportIdMap::from([(
            ViewportId::ROOT,
            ViewportOutput {
                parent: ViewportId::ROOT,
                class: ViewportClass::Root,
                builder: ViewportBuilder::default(),
                viewport_ui_cb: None,
                commands: Vec::new(),
                repaint_delay: std::time::Duration::MAX,
            },
        )]);

        let schedules = schedule_native_viewport_creates(&mut active, &sink);

        assert_eq!(schedules.len(), 1);
        assert!(active.contains_key(&target));
        for schedule in schedules {
            schedule.cancel(&sink);
        }
        coordinator
            .lock()
            .issue_viewport_create(
                parent,
                target,
                crate::NativeViewportCreateCorrelation::new(egui::UserData::new("retry")),
            )
            .expect("host abort must release the create lane without a terminal result");
    }

    fn viewport_output(parent: ViewportId, class: ViewportClass) -> ViewportOutput {
        ViewportOutput {
            parent,
            class,
            builder: ViewportBuilder::default(),
            viewport_ui_cb: None,
            commands: Vec::new(),
            repaint_delay: std::time::Duration::MAX,
        }
    }

    fn image_delta(color: egui::Color32) -> egui::epaint::ImageDelta {
        egui::epaint::ImageDelta::full(
            egui::ColorImage::filled([1, 1], color),
            egui::TextureOptions::LINEAR,
        )
    }

    #[test]
    fn native_staging_surface_requires_exact_non_minimized_non_zero_facts() {
        assert!(is_native_staging_surface(Some(false), Some(false), [1, 1]));
        assert!(is_native_staging_surface(Some(true), Some(false), [1, 1]));
        assert!(!is_native_staging_surface(Some(false), Some(true), [1, 1]));
        assert!(!is_native_staging_surface(None, Some(false), [1, 1]));
        assert!(!is_native_staging_surface(Some(false), None, [1, 1]));
        assert!(!is_native_staging_surface(Some(false), Some(false), [0, 1]));
        assert!(!is_native_staging_surface(Some(false), Some(false), [1, 0]));
    }

    #[test]
    fn armed_output_roster_settles_every_token_when_rendering_unwinds() {
        let child = ViewportId::from_hash_of("armed-presentation-child");
        let coordinator = Arc::new(egui::mutex::Mutex::new(
            crate::native::platform_provider::NativePlatformCoordinator::default(),
        ));
        let root_binding = coordinator
            .lock()
            .register_viewport(ViewportId::ROOT)
            .unwrap();
        let child_binding = coordinator.lock().register_viewport(child).unwrap();
        let settled = Arc::new(egui::mutex::Mutex::new(Vec::new()));
        let hook_settled = Arc::clone(&settled);
        let presentation_results = PresentationResults::new(
            Some(Arc::new(move |result| {
                hook_settled.lock().push(result);
            })),
            coordinator,
        );
        let generation = crate::native::platform_provider::NativePlatformSnapshotGeneration::new(1);
        let output = |viewport_id, binding, token| {
            let mut full_output = egui::FullOutput::default();
            full_output.platform_output.presentation_token = Some(egui::UserData::new(token));
            HostedViewportOutput::with_native_authority(
                viewport_id,
                full_output,
                Some(HostedNativeOutputAuthority {
                    binding,
                    snapshot_generation: generation,
                }),
            )
        };
        let armed = arm_hosted_presentations(
            vec![
                output(ViewportId::ROOT, root_binding, "root"),
                output(child, child_binding, "child"),
            ],
            &presentation_results,
        );

        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _armed = armed;
            panic!("injected renderer failure");
        }));
        assert!(unwind.is_err());
        presentation_results.drain();

        let settled = settled.lock();
        assert_eq!(settled.len(), 2);
        assert!(settled.iter().all(|result| matches!(
            result.outcome(),
            egui::PaintOutcome::Failed(egui::PaintFailure::CoordinatorAborted)
        )));
        assert!(
            settled
                .iter()
                .any(|result| result.viewport_id() == ViewportId::ROOT)
        );
        assert!(settled.iter().any(|result| result.viewport_id() == child));
    }

    #[test]
    fn frozen_roster_requires_the_physical_root() {
        let child = ViewportId::from_hash_of("rootless-hosted-cycle");

        let error = HostedViewportCycle::new([input(child, 1, Event::Copy)])
            .expect_err("a hosted cycle cannot exist without its physical root");

        assert!(matches!(error, HostedViewportCycleError::MissingRootInput));
    }

    #[test]
    fn complete_input_roster_precedes_every_permuted_viewport_callback() {
        let child = ViewportId::from_hash_of("hosted child");
        let cycle = HostedViewportCycle::new([
            input(ViewportId::ROOT, 1, Event::Copy),
            input(child, 2, Event::Cut),
        ])
        .expect("the exact roster is valid");
        let phases = RefCell::new(Vec::new());

        let outputs = cycle
            .run(
                [child, ViewportId::ROOT],
                |inputs| {
                    phases.borrow_mut().push("begin");
                    assert_eq!(inputs.len(), 2);
                    Ok(())
                },
                |input| {
                    let prior_phases = phases.borrow();
                    assert_eq!(prior_phases.first(), Some(&"begin"));
                    assert!(!prior_phases.contains(&"end"));
                    drop(prior_phases);
                    phases.borrow_mut().push(if input.viewport_id() == child {
                        "child"
                    } else {
                        "root"
                    });
                    Ok(input.viewport_id())
                },
                |outputs| {
                    assert_eq!(outputs.len(), 2);
                    phases.borrow_mut().push("end");
                    Ok(())
                },
            )
            .expect("a complete permutation runs");

        assert_eq!(phases.into_inner(), ["begin", "child", "root", "end"]);
        assert_eq!(outputs[0].output(), &child);
        assert_eq!(outputs[1].output(), &ViewportId::ROOT);
    }

    #[test]
    fn release_then_press_keeps_global_sequence_position_and_source_viewport() {
        let child = ViewportId::from_hash_of("hosted target");
        let release_position = Pos2::new(17.0, 19.0);
        let press_position = Pos2::new(71.0, 73.0);
        let cycle = HostedViewportCycle::new([
            input(
                ViewportId::ROOT,
                41,
                Event::PointerButton {
                    pos: release_position,
                    button: PointerButton::Primary,
                    pressed: false,
                    modifiers: Modifiers::NONE,
                },
            ),
            input(
                child,
                42,
                Event::PointerButton {
                    pos: press_position,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ),
        ])
        .expect("the exact roster is valid");

        cycle
            .run(
                [child, ViewportId::ROOT],
                |inputs| {
                    let mut edges = inputs
                        .inputs()
                        .flat_map(|(viewport_id, input)| {
                            input.events.iter().map(move |event| {
                                (
                                    event
                                        .correlation()
                                        .sequence()
                                        .expect("backend sequence is known"),
                                    viewport_id,
                                    event.event().clone(),
                                )
                            })
                        })
                        .collect::<Vec<_>>();
                    edges.sort_by_key(|(sequence, _, _)| *sequence);

                    assert!(matches!(
                        &edges[0],
                        (sequence, viewport_id, Event::PointerButton { pos, pressed: false, .. })
                            if *sequence == BackendEventSequence::new(41)
                                && *viewport_id == ViewportId::ROOT
                                && *pos == release_position
                    ));
                    assert!(matches!(
                        &edges[1],
                        (sequence, viewport_id, Event::PointerButton { pos, pressed: true, .. })
                            if *sequence == BackendEventSequence::new(42)
                                && *viewport_id == child
                                && *pos == press_position
                    ));
                    Ok(())
                },
                |_| Ok(()),
                |_| Ok(()),
            )
            .expect("callback order cannot alter the frozen event journal");
    }

    #[test]
    fn invalid_callback_order_is_rejected_before_any_hook_runs() {
        let child = ViewportId::from_hash_of("missing child");
        let cycle = HostedViewportCycle::new([
            input(ViewportId::ROOT, 1, Event::Copy),
            input(child, 2, Event::Cut),
        ])
        .expect("the exact roster is valid");
        let invoked = RefCell::new(false);

        let error = cycle
            .run(
                [ViewportId::ROOT],
                |_| {
                    *invoked.borrow_mut() = true;
                    Ok(())
                },
                |_| {
                    *invoked.borrow_mut() = true;
                    Ok(())
                },
                |_| {
                    *invoked.borrow_mut() = true;
                    Ok(())
                },
            )
            .expect_err("a partial callback roster must fail closed");

        assert!(matches!(
            error.error(),
            HostedViewportCycleError::MissingCallback { viewport_id } if *viewport_id == child
        ));
        assert!(!invoked.into_inner());
    }

    #[test]
    fn one_driver_owns_every_hosted_cycle_phase() {
        let child = ViewportId::from_hash_of("driver child");
        let cycle = HostedViewportCycle::new([
            input(ViewportId::ROOT, 1, Event::Copy),
            input(child, 2, Event::Cut),
        ])
        .expect("the exact roster is valid");
        let mut driver = RecordingDriver {
            phases: Vec::new(),
            child,
        };

        let outputs = cycle
            .drive([child, ViewportId::ROOT], &mut driver)
            .expect("one driver can own the complete cycle");

        assert_eq!(driver.phases, ["begin", "child", "root", "end"]);
        assert_eq!(outputs[0].output(), &child);
        assert_eq!(outputs[1].output(), &ViewportId::ROOT);
    }

    #[test]
    fn begin_failure_runs_no_viewport_or_end_phase() {
        let child = ViewportId::from_hash_of("begin-failure-child");
        let cycle = HostedViewportCycle::new([
            input(ViewportId::ROOT, 1, Event::Copy),
            input(child, 2, Event::Cut),
        ])
        .expect("the exact roster is valid");
        let phases = RefCell::new(Vec::new());

        let error = cycle
            .run(
                [ViewportId::ROOT, child],
                |_| {
                    phases.borrow_mut().push("begin");
                    Err(std::io::Error::other("begin rejected").into())
                },
                |_| {
                    phases.borrow_mut().push("viewport");
                    Ok(())
                },
                |_| {
                    phases.borrow_mut().push("end");
                    Ok(())
                },
            )
            .expect_err("a begin failure must abort the complete cycle");

        assert!(matches!(
            error.error(),
            HostedViewportCycleError::BeginHook { .. }
        ));
        assert_eq!(
            error
                .unconsumed_inputs()
                .iter()
                .map(HostedViewportInput::viewport_id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([ViewportId::ROOT, child])
        );
        assert!(error.staged_outputs().is_empty());
        assert_eq!(phases.into_inner(), ["begin"]);
    }

    #[test]
    fn viewport_failure_stops_later_ui_and_skips_end() {
        let child = ViewportId::from_hash_of("viewport-failure-child");
        let cycle = HostedViewportCycle::new([
            input(ViewportId::ROOT, 1, Event::Copy),
            input(child, 2, Event::Cut),
        ])
        .expect("the exact roster is valid");
        let phases = RefCell::new(Vec::new());

        let error = cycle
            .run(
                [child, ViewportId::ROOT],
                |_| {
                    phases.borrow_mut().push("begin");
                    Ok(())
                },
                |input| {
                    phases.borrow_mut().push(if input.viewport_id() == child {
                        "child"
                    } else {
                        "root"
                    });
                    if input.viewport_id() == child {
                        Err(std::io::Error::other("child rejected").into())
                    } else {
                        Ok(())
                    }
                },
                |_| {
                    phases.borrow_mut().push("end");
                    Ok(())
                },
            )
            .expect_err("one viewport failure must abort the complete cycle");

        assert!(matches!(
            error.error(),
            HostedViewportCycleError::ViewportUi { viewport_id, .. } if *viewport_id == child
        ));
        assert!(error.staged_outputs().is_empty());
        assert_eq!(
            error
                .unconsumed_inputs()
                .iter()
                .map(HostedViewportInput::viewport_id)
                .collect::<Vec<_>>(),
            [ViewportId::ROOT]
        );
        assert_eq!(phases.into_inner(), ["begin", "child"]);
    }

    #[test]
    fn viewport_failure_retains_prior_output_and_only_later_inputs() {
        let failing = ViewportId::from_hash_of("failing-hosted-child");
        let later = ViewportId::from_hash_of("unconsumed-hosted-child");
        let cycle = HostedViewportCycle::new([
            input(ViewportId::ROOT, 1, Event::Copy),
            input(failing, 2, Event::Cut),
            input(later, 3, Event::Paste("later".to_owned())),
        ])
        .expect("the exact roster is valid");

        let error = cycle
            .run(
                [ViewportId::ROOT, failing, later],
                |_| Ok(()),
                |input| {
                    if input.viewport_id() == failing {
                        Err(std::io::Error::other("child rejected").into())
                    } else {
                        Ok(input.viewport_id())
                    }
                },
                |_| Ok(()),
            )
            .expect_err("one viewport failure must abort normal rendering");

        assert_eq!(error.staged_outputs().len(), 1);
        assert_eq!(error.staged_outputs()[0].viewport_id(), ViewportId::ROOT);
        assert_eq!(error.unconsumed_inputs().len(), 1);
        assert_eq!(error.unconsumed_inputs()[0].viewport_id(), later);
    }

    #[test]
    fn transactional_abort_hook_stays_guarded_for_every_driven_failure() {
        #[derive(Clone, Copy, Debug)]
        enum Failure {
            Begin,
            ViewportUi,
            End,
            ImmediateViewport,
            OutputAuthority,
        }

        struct FailingDriver {
            failure: Failure,
            aborts: usize,
            phase_immediate_viewport: ViewportId,
            abort_immediate_viewport: ViewportId,
            abort_observation: Option<Result<(), HostedImmediateViewportViolation>>,
            abort_viewport_class: Option<egui::ViewportClass>,
            forged_binding: crate::NativeViewportBinding,
        }

        impl HostedViewportCycleDriver for FailingDriver {
            type Output = ();

            fn begin(
                &mut self,
                _cycle: &HostedViewportCycle,
            ) -> crate::HostedViewportAppResult<()> {
                match self.failure {
                    Failure::Begin => Err(std::io::Error::other("begin rejected").into()),
                    _ => Ok(()),
                }
            }

            fn run_viewport(
                &mut self,
                _input: HostedViewportInput,
            ) -> crate::HostedViewportAppResult<Self::Output> {
                match self.failure {
                    Failure::ViewportUi => Err(std::io::Error::other("viewport rejected").into()),
                    Failure::ImmediateViewport => {
                        let _ = check_hosted_immediate_viewport(self.phase_immediate_viewport);
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }

            fn end(
                &mut self,
                outputs: &mut [HostedViewportOutput<Self::Output>],
            ) -> crate::HostedViewportAppResult<()> {
                match self.failure {
                    Failure::End => Err(std::io::Error::other("end rejected").into()),
                    Failure::OutputAuthority => {
                        outputs[0].native_authority = Some(HostedNativeOutputAuthority {
                            binding: self.forged_binding,
                            snapshot_generation: crate::NativePlatformSnapshotGeneration::new(1),
                        });
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }

            fn abort(&mut self) {
                self.aborts += 1;
                self.abort_observation = Some(check_hosted_immediate_viewport(
                    self.abort_immediate_viewport,
                ));
                let context = egui::Context::default();
                context.set_embed_viewports(false);
                egui::Context::set_immediate_viewport_renderer(|_, _| {
                    panic!("the abort hook must not reach the native immediate renderer")
                });
                let mut observed_class = None;
                let _ = context.run_ui(egui::RawInput::default(), |ui| {
                    observed_class = Some(ui.ctx().show_viewport_immediate(
                        self.abort_immediate_viewport,
                        egui::ViewportBuilder::default(),
                        |_, class| class,
                    ));
                });
                self.abort_viewport_class = observed_class;
            }
        }

        for failure in [
            Failure::Begin,
            Failure::ViewportUi,
            Failure::End,
            Failure::ImmediateViewport,
            Failure::OutputAuthority,
        ] {
            let phase_attempt =
                ViewportId::from_hash_of(("phase-immediate", format!("{failure:?}")));
            let abort_attempt =
                ViewportId::from_hash_of(("abort-immediate", format!("{failure:?}")));
            let forged_binding =
                crate::native::platform_provider::NativePlatformCoordinator::default()
                    .register_viewport(ViewportId::ROOT)
                    .expect("the test coordinator can mint a root binding");
            let cycle = HostedViewportCycle::new([input(ViewportId::ROOT, 1, Event::Copy)])
                .expect("the root roster is valid");
            let mut driver = FailingDriver {
                failure,
                aborts: 0,
                phase_immediate_viewport: phase_attempt,
                abort_immediate_viewport: abort_attempt,
                abort_observation: None,
                abort_viewport_class: None,
                forged_binding,
            };

            let error = cycle
                .drive_with_mode(
                    crate::HostedViewportMode::Transactional,
                    [ViewportId::ROOT],
                    &mut driver,
                )
                .expect_err("the injected failure must abort the driver-owned transaction");

            match failure {
                Failure::Begin => {
                    assert!(matches!(
                        error.error(),
                        HostedViewportCycleError::BeginHook { .. }
                    ));
                }
                Failure::ViewportUi => assert!(matches!(
                    error.error(),
                    HostedViewportCycleError::ViewportUi {
                        viewport_id: ViewportId::ROOT,
                        ..
                    }
                )),
                Failure::End => {
                    assert!(matches!(
                        error.error(),
                        HostedViewportCycleError::EndHook { .. }
                    ));
                }
                Failure::ImmediateViewport => assert!(matches!(
                    error.error(),
                    HostedViewportCycleError::ImmediateViewportAttempt { viewport_id }
                        if *viewport_id == phase_attempt
                )),
                Failure::OutputAuthority => assert!(matches!(
                    error.error(),
                    HostedViewportCycleError::NativeOutputAuthorityMismatch {
                        viewport_id: ViewportId::ROOT
                    }
                )),
            }

            assert_eq!(driver.aborts, 1, "failure: {failure:?}");
            assert_eq!(
                driver.abort_observation,
                Some(Err(HostedImmediateViewportViolation::new(abort_attempt))),
                "the abort hook must remain inside the guard for {failure:?}",
            );
            assert!(matches!(
                driver.abort_viewport_class,
                Some(egui::ViewportClass::EmbeddedWindow)
            ));
            let expected_violations = match failure {
                Failure::ImmediateViewport => vec![
                    HostedImmediateViewportViolation::new(phase_attempt),
                    HostedImmediateViewportViolation::new(abort_attempt),
                ],
                _ => vec![HostedImmediateViewportViolation::new(abort_attempt)],
            };
            assert_eq!(
                error.immediate_viewport_violations(),
                expected_violations,
                "failure: {failure:?}",
            );
            assert_eq!(check_hosted_immediate_viewport(abort_attempt), Ok(()));
        }
    }

    #[test]
    fn end_failure_retains_staged_outputs_for_terminal_settlement() {
        let cycle = HostedViewportCycle::new([input(ViewportId::ROOT, 1, Event::Copy)])
            .expect("the root roster is valid");
        let phases = RefCell::new(Vec::new());

        let error = cycle
            .run(
                [ViewportId::ROOT],
                |_| {
                    phases.borrow_mut().push("begin");
                    Ok(())
                },
                |_| {
                    phases.borrow_mut().push("root");
                    Ok(())
                },
                |_| {
                    phases.borrow_mut().push("end");
                    Err(std::io::Error::other("end rejected").into())
                },
            )
            .expect_err("an end failure must abort normal rendering");

        assert!(matches!(
            error.error(),
            HostedViewportCycleError::EndHook { .. }
        ));
        assert_eq!(error.staged_outputs().len(), 1);
        assert_eq!(error.staged_outputs()[0].viewport_id(), ViewportId::ROOT);
        assert!(error.unconsumed_inputs().is_empty());
        assert_eq!(phases.into_inner(), ["begin", "root", "end"]);
    }

    #[test]
    fn transactional_drive_rejects_immediate_viewports_and_retains_the_current_output() {
        let child = ViewportId::from_hash_of("transactional-child");
        let cycle = HostedViewportCycle::new([
            input(ViewportId::ROOT, 1, Event::Copy),
            input(child, 2, Event::Cut),
        ])
        .expect("the exact roster is valid");

        let error = cycle
            .run_with_mode(
                crate::HostedViewportMode::Transactional,
                [ViewportId::ROOT, child],
                |_| Ok(()),
                |input| {
                    if input.viewport_id() == ViewportId::ROOT {
                        let _ = check_hosted_immediate_viewport(ViewportId::from_hash_of(
                            "forbidden-immediate",
                        ));
                    }
                    Ok(input.viewport_id())
                },
                |_| Ok(()),
            )
            .expect_err("transactional mode must abort on an immediate viewport attempt");

        assert!(matches!(
            error.error(),
            HostedViewportCycleError::ImmediateViewportAttempt { .. }
        ));
        assert_eq!(error.staged_outputs().len(), 1);
        assert_eq!(error.staged_outputs()[0].viewport_id(), ViewportId::ROOT);
        assert_eq!(error.unconsumed_inputs()[0].viewport_id(), child);
        assert_eq!(error.immediate_viewport_violations().len(), 1);
    }

    #[test]
    fn preexisting_transaction_violation_aborts_before_the_begin_hook() {
        let child = ViewportId::from_hash_of("pre-hook-child");
        let attempted = ViewportId::from_hash_of("pre-hook-immediate");
        let cycle = HostedViewportCycle::new([
            input(ViewportId::ROOT, 1, Event::Copy),
            input(child, 2, Event::Cut),
        ])
        .expect("the exact roster is valid");
        let guard = HostedViewportTransactionGuard::enter(crate::HostedViewportMode::Transactional);
        assert_eq!(
            check_hosted_immediate_viewport(attempted),
            Err(HostedImmediateViewportViolation::new(attempted))
        );
        let mut driver = RecordingDriver {
            phases: Vec::new(),
            child,
        };

        let error = cycle
            .drive_with_guard(guard, [ViewportId::ROOT, child], &mut driver)
            .expect_err("a violation captured before begin must abort the cycle");

        assert!(matches!(
            error.error(),
            HostedViewportCycleError::ImmediateViewportAttempt { viewport_id }
                if *viewport_id == attempted
        ));
        assert!(driver.phases.is_empty());
        assert!(error.staged_outputs().is_empty());
        assert_eq!(error.unconsumed_inputs().len(), 2);
    }

    #[test]
    fn transactional_driver_retains_its_guard_until_the_commit_boundary() {
        let child = ViewportId::from_hash_of("retained-transaction-child");
        let attempted = ViewportId::from_hash_of("retained-transaction-immediate");
        let cycle = HostedViewportCycle::new([
            input(ViewportId::ROOT, 1, Event::Copy),
            input(child, 2, Event::Cut),
        ])
        .expect("the exact roster is valid");
        let guard = HostedViewportTransactionGuard::enter(crate::HostedViewportMode::Transactional);
        let mut driver = RecordingDriver {
            phases: Vec::new(),
            child,
        };

        let completed = cycle
            .drive_with_guard(guard, [ViewportId::ROOT, child], &mut driver)
            .expect("the staged transaction must complete before commit");

        assert_eq!(
            check_hosted_immediate_viewport(attempted),
            Err(HostedImmediateViewportViolation::new(attempted)),
            "the transaction guard must remain active after UI staging",
        );
        let (outputs, guard) = completed.into_parts();
        assert_eq!(outputs.len(), 2);
        assert_eq!(
            guard.finish(),
            [HostedImmediateViewportViolation::new(attempted)],
        );
        assert_eq!(check_hosted_immediate_viewport(attempted), Ok(()));
    }

    #[test]
    fn application_commit_seal_rejects_preexisting_violation() {
        let attempted = ViewportId::from_hash_of("pre-commit-immediate");
        let mut guard =
            HostedViewportTransactionGuard::enter(crate::HostedViewportMode::Transactional);
        assert_eq!(
            check_hosted_immediate_viewport(attempted),
            Err(HostedImmediateViewportViolation::new(attempted))
        );

        assert_eq!(
            guard.seal_before_application_commit(),
            Err(HostedImmediateViewportViolation::new(attempted))
        );
        assert_eq!(check_hosted_immediate_viewport(attempted), Ok(()));
        assert!(guard.finish().is_empty());
    }

    #[test]
    fn clean_application_commit_seal_closes_the_failure_log() {
        let attempted = ViewportId::from_hash_of("sealed-commit-immediate");
        let mut guard =
            HostedViewportTransactionGuard::enter(crate::HostedViewportMode::Transactional);

        guard
            .seal_before_application_commit()
            .expect("a clean staged cycle can enter publication");
        assert_eq!(check_hosted_immediate_viewport(attempted), Ok(()));
        assert!(guard.finish().is_empty());
    }

    #[test]
    fn transactional_immediate_guard_is_nested_and_raii_scoped() {
        let first = ViewportId::from_hash_of("nested-immediate-first");
        let second = ViewportId::from_hash_of("nested-immediate-second");
        let outer = HostedViewportTransactionGuard::enter(crate::HostedViewportMode::Transactional);

        {
            let inner =
                HostedViewportTransactionGuard::enter(crate::HostedViewportMode::Transactional);
            assert_eq!(
                check_hosted_immediate_viewport(first),
                Err(HostedImmediateViewportViolation::new(first))
            );
            assert_eq!(
                inner.violations(),
                [HostedImmediateViewportViolation::new(first)]
            );
        }

        assert_eq!(
            check_hosted_immediate_viewport(second),
            Err(HostedImmediateViewportViolation::new(second))
        );
        assert_eq!(
            outer.finish(),
            [
                HostedImmediateViewportViolation::new(first),
                HostedImmediateViewportViolation::new(second),
            ]
        );
        assert_eq!(check_hosted_immediate_viewport(first), Ok(()));
    }

    #[test]
    fn transactional_guard_cleans_up_after_early_error() {
        fn reject(viewport_id: ViewportId) -> Result<(), HostedImmediateViewportViolation> {
            let _guard =
                HostedViewportTransactionGuard::enter(crate::HostedViewportMode::Transactional);
            check_hosted_immediate_viewport(viewport_id)?;
            Ok(())
        }

        let viewport_id = ViewportId::from_hash_of("early-error-immediate");
        assert_eq!(
            reject(viewport_id),
            Err(HostedImmediateViewportViolation::new(viewport_id))
        );
        assert_eq!(check_hosted_immediate_viewport(viewport_id), Ok(()));
    }

    #[test]
    fn compatibility_guard_is_the_default_and_allows_immediate_viewports() {
        assert_eq!(
            crate::HostedViewportMode::default(),
            crate::HostedViewportMode::Compatibility
        );
        let _guard =
            HostedViewportTransactionGuard::enter(crate::HostedViewportMode::Compatibility);

        assert_eq!(
            check_hosted_immediate_viewport(ViewportId::from_hash_of("compatible-immediate")),
            Ok(())
        );
    }

    #[test]
    fn child_callback_cannot_resurrect_child_removed_by_parent_output() {
        let child = ViewportId::from_hash_of("hosted-child");
        let root_output = OrderedViewportIdMap::from([(
            ViewportId::ROOT,
            viewport_output(ViewportId::ROOT, ViewportClass::Root),
        )]);
        let child_output = OrderedViewportIdMap::from([
            (
                ViewportId::ROOT,
                viewport_output(ViewportId::ROOT, ViewportClass::Root),
            ),
            (
                child,
                viewport_output(ViewportId::ROOT, ViewportClass::Deferred),
            ),
        ]);

        let consolidated = consolidate_hosted_viewport_outputs(&[
            (ViewportId::ROOT, root_output),
            (child, child_output),
        ])
        .expect("valid hosted outputs should consolidate");

        assert_eq!(
            consolidated.keys().copied().collect::<Vec<_>>(),
            vec![ViewportId::ROOT]
        );
    }

    #[test]
    fn real_egui_child_output_cannot_resurrect_parent_removed_viewport() {
        let child = ViewportId::from_hash_of("real-egui-removed-child");
        let context = egui::Context::default();
        context.set_embed_viewports(false);

        let _created = context.run_ui(egui::RawInput::default(), |ui| {
            ui.ctx()
                .show_viewport_deferred(child, ViewportBuilder::default(), |_, _| {});
        });
        let child_input = || {
            let mut input = egui::RawInput {
                viewport_id: child,
                ..Default::default()
            };
            input.viewports.insert(
                child,
                egui::ViewportInfo {
                    parent: Some(ViewportId::ROOT),
                    ..Default::default()
                },
            );
            input
        };
        let _created_child = context.run_ui(child_input(), |_| {});

        let root_output = context.run_ui(egui::RawInput::default(), |_| {});
        let child_output = context.run_ui(child_input(), |_| {});
        assert!(
            child_output.viewport_output.contains_key(&child),
            "a frozen physical owner must still publish its own output record"
        );

        let consolidated = consolidate_hosted_viewport_outputs(&[
            (ViewportId::ROOT, root_output.viewport_output),
            (child, child_output.viewport_output),
        ])
        .expect("real egui outputs should preserve the physical owner record");

        assert_eq!(
            consolidated.keys().copied().collect::<Vec<_>>(),
            vec![ViewportId::ROOT]
        );
    }

    #[test]
    fn active_child_retains_commands_from_every_staged_output() {
        let child = ViewportId::from_hash_of("hosted-command-child");
        let mut root_child = viewport_output(ViewportId::ROOT, ViewportClass::Deferred);
        root_child
            .commands
            .push(egui::ViewportCommand::Title("root phase".to_owned()));
        let root_output = OrderedViewportIdMap::from([
            (
                ViewportId::ROOT,
                viewport_output(ViewportId::ROOT, ViewportClass::Root),
            ),
            (child, root_child),
        ]);
        let mut child_record = viewport_output(ViewportId::ROOT, ViewportClass::Deferred);
        child_record
            .commands
            .push(egui::ViewportCommand::Visible(true));
        let child_output = OrderedViewportIdMap::from([
            (
                ViewportId::ROOT,
                viewport_output(ViewportId::ROOT, ViewportClass::Root),
            ),
            (child, child_record),
        ]);

        let consolidated = consolidate_hosted_viewport_outputs(&[
            (ViewportId::ROOT, root_output),
            (child, child_output),
        ])
        .expect("valid hosted outputs should consolidate");

        assert_eq!(
            consolidated[&child].commands,
            [
                egui::ViewportCommand::Title("root phase".to_owned()),
                egui::ViewportCommand::Visible(true),
            ]
        );
    }

    #[test]
    fn duplicate_output_owner_is_structurally_rejected() {
        let root_output = OrderedViewportIdMap::from([(
            ViewportId::ROOT,
            viewport_output(ViewportId::ROOT, ViewportClass::Root),
        )]);

        let Err(error) = consolidate_hosted_viewport_outputs(&[
            (ViewportId::ROOT, root_output.clone()),
            (ViewportId::ROOT, root_output),
        ]) else {
            panic!("one physical callback cannot own two staged outputs");
        };

        assert_eq!(
            error,
            HostedViewportOutputConsolidationError::DuplicateOwnerOutput {
                viewport_id: ViewportId::ROOT,
            }
        );
    }

    #[test]
    fn root_output_must_publish_the_root_record() {
        let child = ViewportId::from_hash_of("root-record-owned-by-child");
        let root_output = OrderedViewportIdMap::from([(
            child,
            viewport_output(ViewportId::ROOT, ViewportClass::Deferred),
        )]);

        let Err(error) = consolidate_hosted_viewport_outputs(&[(ViewportId::ROOT, root_output)])
        else {
            panic!("the physical root output must publish the root record");
        };

        assert_eq!(
            error,
            HostedViewportOutputConsolidationError::MissingRootRecord
        );
    }

    #[test]
    fn every_physical_output_owner_must_publish_its_own_record() {
        let child = ViewportId::from_hash_of("owner-without-self-record");
        let root_output = OrderedViewportIdMap::from([
            (
                ViewportId::ROOT,
                viewport_output(ViewportId::ROOT, ViewportClass::Root),
            ),
            (
                child,
                viewport_output(ViewportId::ROOT, ViewportClass::Deferred),
            ),
        ]);
        let child_output = OrderedViewportIdMap::from([(
            ViewportId::ROOT,
            viewport_output(ViewportId::ROOT, ViewportClass::Root),
        )]);

        let Err(error) = consolidate_hosted_viewport_outputs(&[
            (ViewportId::ROOT, root_output),
            (child, child_output),
        ]) else {
            panic!("a physical output owner must publish its own record");
        };

        assert_eq!(
            error,
            HostedViewportOutputConsolidationError::MissingOwnerRecord { viewport_id: child }
        );
    }

    #[test]
    fn shared_texture_updates_follow_output_order_and_frees_are_cycle_terminal() {
        let texture_id = egui::TextureId::User(77);
        let root_image = image_delta(egui::Color32::RED);
        let child_image = image_delta(egui::Color32::BLUE);
        let mut root_delta = egui::TexturesDelta {
            set: vec![(texture_id, root_image.clone())],
            free: vec![texture_id],
        };
        let mut child_delta = egui::TexturesDelta {
            set: vec![(texture_id, child_image.clone())],
            free: vec![texture_id],
        };
        let mut observed_sets = Vec::new();
        let mut synchronizer = HostedTextureSynchronizer::default();

        synchronizer.synchronize_output(&mut root_delta, |id, image| {
            observed_sets.push((id, image.clone()));
        });
        synchronizer.synchronize_output(&mut child_delta, |id, image| {
            observed_sets.push((id, image.clone()));
        });

        assert_eq!(observed_sets.len(), 2);
        assert_eq!(observed_sets[0].0, texture_id);
        assert!(observed_sets[0].1 == root_image);
        assert_eq!(observed_sets[1].0, texture_id);
        assert!(observed_sets[1].1 == child_image);
        assert!(root_delta.is_empty());
        assert!(child_delta.is_empty());
        assert_eq!(synchronizer.into_deferred_frees(), [texture_id]);
    }
}
