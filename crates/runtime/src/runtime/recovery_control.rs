//! Process-local graph reservations. Durable control rows never reconstruct these.
use super::*;
use storage::recovery::graph_control::{ActivationControl, GraphEffectKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationReservationProblem {
    Contended,
    Capacity,
}

#[derive(Default)]
pub(super) struct GraphReservations {
    next: u64,
    #[cfg(test)]
    effect_counts: [usize; 3],
    graphs: HashMap<String, (u64, bool)>,
    supervised: HashMap<String, (ActivationReservation, storage::recovery::RecoveryClaim)>,
}
struct Reservation {
    runtime: Weak<Shared>,
    manifest: String,
    nonce: u64,
    unknown: bool,
    // Keep the reservation-bearing ActivationFailure small; the observation is
    // retained with identical ownership and never clears its fence on Drop.
    observation: Option<Box<super::graph_observation::GraphChange>>,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.unknown {
            return;
        }
        if let Some(runtime) = self.runtime.upgrade() {
            let mut state = runtime
                .graph_reservations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.graphs.get(&self.manifest) == Some(&(self.nonce, false)) {
                state.graphs.remove(&self.manifest);
            }
        }
    }
}
/// A single automatic attempt, including binding publication and FULL activation.
///
/// ```no_run
/// # fn reserve(rt: &runtime::AgentRuntime) -> Result<(), runtime::RuntimeError> {
/// let reservation = rt.reserve_automatic_activation("graph")?;
/// rt.check_automatic_activation(&reservation)?;
/// # Ok(()) }
/// ```
/// ```compile_fail
/// fn duplicate(rt: &runtime::AgentRuntime) {
///     let reservation = rt.reserve_automatic_activation("graph").unwrap();
///     let first = reservation;
///     let second = reservation;
/// }
/// ```
pub struct AutomaticActivationReservation(Reservation);
impl AutomaticActivationReservation {
    pub fn mark_graph_observation_unavailable(&self) {
        self.0.mark_graph_observation_unavailable();
    }
    pub fn confirm_graph_observation(&self) {
        self.0.confirm_graph_observation();
    }
}
impl Reservation {
    fn mark_graph_observation_unavailable(&self) {
        if let Some(observation) = &self.observation {
            observation.mark_unavailable();
        }
    }
    fn confirm_graph_observation(&self) {
        if let Some(observation) = &self.observation
            && let Err(error) = observation.confirm()
        {
            tracing::warn!(%error, "graph observation remains fenced");
        }
    }
}
/// Retains the original graph reservation after FULL activation ACK until the
/// caller has finished publishing the root binding (including failure).
///
/// This is not execution authority. Release explicitly after publication, or
/// let Drop return the slot when a caller exits early.
///
/// ```no_run
/// fn published(guard: runtime::ActivatedReservation) { guard.release(); }
/// ```
/// ```compile_fail
/// fn duplicate(guard: runtime::ActivatedReservation) {
///     let second = guard.clone();
/// }
/// ```
#[must_use = "retain until complete_root_recovery finishes, then release"]
pub struct ActivatedReservation(ActivationReservation);
impl ActivatedReservation {
    pub(super) fn new(reservation: ActivationReservation) -> Self {
        Self(reservation)
    }
    pub fn release(self) {
        self.0.confirm_graph_observation();
        drop(self.0);
    }
}
impl std::fmt::Debug for ActivatedReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivatedReservation")
            .finish_non_exhaustive()
    }
}

/// A failed activation returns its original reservation for caller-side cleanup.
/// When the ACK is uncertain, the runtime supervisor owns it instead.
/// No conversion to RuntimeError is provided: callers must decide the lifetime.
pub struct ActivationFailure {
    pub error: Box<RuntimeError>,
    pub reservation: Option<ActivationReservation>,
}
impl std::fmt::Debug for ActivationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivationFailure")
            .field("error", &self.error)
            .field("reservation_retained", &self.reservation.is_some())
            .finish()
    }
}
impl std::fmt::Display for ActivationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for ActivationFailure {}

/// Manual execution still requires independently resolved execution authority.
impl ManualActivationReservation {
    /// A non-authority readback handle for the original supervisor's pre-effect
    /// error paths. It cannot reserve or execute another activation.
    pub fn observation_readback(&self) -> impl FnOnce() -> Result<(), RuntimeError> + 'static {
        let observation = self.reservation.observation.clone();
        move || observation.map_or(Ok(()), |observation| observation.confirm())
    }
}
pub struct ManualActivationReservation {
    reservation: Reservation,
    effect: GraphEffectKey,
    control_revision: u64,
}
/// Typed activation boundary, distinct from disposal authority.
///
/// ```no_run
/// # fn run(rt: &runtime::AgentRuntime, paused: runtime::PausedRecoveryComponent,
/// # authority: runtime::CurrentRecoveryAuthority) -> Result<(), Box<dyn std::error::Error>> {
/// let reservation = rt.reserve_automatic_activation(&paused.claim().manifest_id)?;
/// let (_runs, publication) = rt.activate_component(paused, authority, reservation)?;
/// publication.release();
/// # Ok(()) }
/// ```
/// ```compile_fail
/// fn unreserved(rt: &runtime::AgentRuntime, paused: runtime::PausedRecoveryComponent,
///     authority: runtime::CurrentRecoveryAuthority) {
///     rt.activate_component(paused, authority);
/// }
/// ```
/// ```compile_fail
/// fn disposal(rt: &runtime::AgentRuntime, paused: runtime::PausedRecoveryComponent,
///     authority: runtime::CurrentRecoveryAuthority, disposal: runtime::DisposalAuthority) {
///     rt.activate_component(paused, authority, disposal);
/// }
/// ```
pub enum ActivationReservation {
    Automatic(AutomaticActivationReservation),
    Manual(ManualActivationReservation),
}
impl From<AutomaticActivationReservation> for ActivationReservation {
    fn from(value: AutomaticActivationReservation) -> Self {
        Self::Automatic(value)
    }
}
impl From<ManualActivationReservation> for ActivationReservation {
    fn from(value: ManualActivationReservation) -> Self {
        Self::Manual(value)
    }
}
impl ActivationReservation {
    pub fn mark_graph_observation_unavailable(&self) {
        self.inner().mark_graph_observation_unavailable();
    }
    /// Original supervisor only, after a confirmed outcome and after all lower locks.
    pub fn confirm_graph_observation(&self) {
        self.inner().confirm_graph_observation();
    }
    fn inner(&self) -> &Reservation {
        match self {
            Self::Automatic(a) => &a.0,
            Self::Manual(m) => &m.reservation,
        }
    }
    pub(super) fn validate(
        &self,
        runtime: &AgentRuntime,
        manifest: &str,
    ) -> Result<(), RuntimeError> {
        let r = self.inner();
        if r.manifest != manifest || !Weak::ptr_eq(&r.runtime, &Arc::downgrade(&runtime.shared)) {
            return Err(error("foreign activation reservation"));
        }
        let state = runtime
            .shared
            .graph_reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.graphs.get(manifest) != Some(&(r.nonce, false)) {
            return Err(error("activation reservation unavailable"));
        }
        Ok(())
    }
    pub(super) fn control(&self) -> ActivationControl {
        match self {
            Self::Automatic(_) => ActivationControl::Automatic,
            Self::Manual(m) => ActivationControl::Manual {
                effect: m.effect.clone(),
                control_revision: m.control_revision,
            },
        }
    }
    pub(super) fn retain_unknown(&mut self) {
        let r = match self {
            Self::Automatic(a) => &mut a.0,
            Self::Manual(m) => &mut m.reservation,
        };
        r.unknown = true;
        if let Some(runtime) = r.runtime.upgrade() {
            let mut state = runtime
                .graph_reservations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.graphs.get(&r.manifest) == Some(&(r.nonce, false)) {
                state.graphs.insert(r.manifest.clone(), (r.nonce, true));
            }
        }
    }
}
impl AgentRuntime {
    #[cfg(test)]
    pub(crate) fn record_graph_effect(&self, index: usize) {
        self.shared.graph_reservations.lock().unwrap().effect_counts[index] += 1;
    }
    #[cfg(test)]
    pub(crate) fn graph_effect_counts(&self) -> [usize; 3] {
        self.shared.graph_reservations.lock().unwrap().effect_counts
    }
    /// Original supervisor readback only. A known result releases the automatic
    /// slot; an unknown result stays owned. This never starts another effect.
    pub fn reconcile_automatic_activation_reservations(&self) -> Result<usize, RuntimeError> {
        self.reconcile_activation_reservations(false)
    }
    /// Original manual supervisor readback only; never starts activation/claim.
    /// Automatic tokens are outside this entry point's authority.
    pub fn reconcile_manual_activation_reservations(&self) -> Result<usize, RuntimeError> {
        self.reconcile_activation_reservations(true)
    }
    fn reconcile_activation_reservations(&self, manual: bool) -> Result<usize, RuntimeError> {
        let claims = {
            let state = self
                .shared
                .graph_reservations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .supervised
                .values()
                .filter_map(|(token, claim)| {
                    (matches!(token, ActivationReservation::Manual(_)) == manual)
                        .then_some((claim.clone(), token.inner().nonce))
                })
                .collect::<Vec<_>>()
        };
        let Some(store) = self.shared.run_store.get() else {
            return Ok(0);
        };
        let mut resolved = 0;
        for (claim, nonce) in claims {
            let observed = store
                .with_database(|db| db.observe_recovery_activation(&claim))
                .map_err(error)?;
            if observed == storage::recovery::RecoveryActivationObservation::Indeterminate {
                continue;
            }
            let mut state = self
                .shared
                .graph_reservations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Manual readback cannot release a replacement token admitted
            // after this read began, including an automatic attempt.
            if manual
                && !state
                    .supervised
                    .get(&claim.manifest_id)
                    .is_some_and(|(token, _)| {
                        matches!(token, ActivationReservation::Manual(_))
                            && token.inner().nonce == nonce
                    })
            {
                continue;
            }
            let settled = state.supervised.remove(&claim.manifest_id);
            if settled.is_some() {
                state.graphs.remove(&claim.manifest_id);
            }
            drop(state);
            if let Some((token, _)) = settled {
                token.confirm_graph_observation();
                resolved += 1;
            }
        }
        Ok(resolved)
    }
    pub(super) fn supervise_activation(
        &self,
        mut reservation: ActivationReservation,
        claim: storage::recovery::RecoveryClaim,
    ) {
        reservation.retain_unknown();
        self.shared
            .graph_reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .supervised
            .insert(claim.manifest_id.clone(), (reservation, claim));
    }
    fn reserve_graph(&self, manifest: &str, automatic: bool) -> Result<Reservation, RuntimeError> {
        if manifest.is_empty() {
            return Err(error("empty recovery graph"));
        }
        let observation = self.begin_graph_change(manifest, automatic)?;
        let mut state = self
            .shared
            .graph_reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reserve = (|| {
            if state.graphs.contains_key(manifest) {
                return Err(RuntimeError::ActivationReservationUnavailable {
                    reason: ActivationReservationProblem::Contended,
                });
            }
            if state.graphs.len() >= 64 {
                return Err(RuntimeError::ActivationReservationUnavailable {
                    reason: ActivationReservationProblem::Capacity,
                });
            }
            state.next = state
                .next
                .checked_add(1)
                .ok_or_else(|| error("reservation sequence exhausted"))?;
            let nonce = state.next;
            state.graphs.insert(manifest.into(), (nonce, false));
            Ok(nonce)
        })();
        drop(state);
        let nonce = match reserve {
            Ok(nonce) => nonce,
            Err(error) => {
                if let Some(observation) = observation {
                    let _ = observation.cancel_unstarted();
                }
                return Err(error);
            }
        };
        Ok(Reservation {
            runtime: Arc::downgrade(&self.shared),
            manifest: manifest.into(),
            nonce,
            unknown: false,
            observation: observation.map(Box::new),
        })
    }
    pub fn reserve_automatic_activation(
        &self,
        manifest_id: &str,
    ) -> Result<AutomaticActivationReservation, RuntimeError> {
        if let Some(store) = self.shared.run_store.get()
            && store
                .with_database(|db| db.recovery_graph_control(manifest_id))
                .map_err(error)?
                .is_some()
        {
            return Err(RuntimeError::ActivationReservationUnavailable {
                reason: ActivationReservationProblem::Contended,
            });
        }
        Ok(AutomaticActivationReservation(
            self.reserve_graph(manifest_id, true)?,
        ))
    }
    pub fn check_automatic_activation(
        &self,
        reservation: &AutomaticActivationReservation,
    ) -> Result<(), RuntimeError> {
        let r = &reservation.0;
        if !Weak::ptr_eq(&r.runtime, &Arc::downgrade(&self.shared))
            || self
                .shared
                .graph_reservations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .graphs
                .get(&r.manifest)
                != Some(&(r.nonce, false))
        {
            return Err(error("stale automatic reservation"));
        }
        Ok(())
    }
    /// Memory-only admission. The continuation persists the marker at this
    /// expected revision; the activation writer independently checks it.
    pub fn reserve_manual_activation(
        &self,
        manifest_id: &str,
        effect: GraphEffectKey,
        expected_control_revision: u64,
    ) -> Result<ManualActivationReservation, RuntimeError> {
        let control_revision = expected_control_revision
            .checked_add(1)
            .ok_or_else(|| error("control revision exhausted"))?;
        Ok(ManualActivationReservation {
            reservation: self.reserve_graph(manifest_id, false)?,
            effect,
            control_revision,
        })
    }
}
fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}

#[cfg(test)]
mod tests;
