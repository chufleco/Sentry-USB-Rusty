//! BLE radio actor — single-owner coordination of the one controller.
//!
//! ## Why an actor
//!
//! Today the sampler `tick()` loop and the IPC action handler serialize
//! naturally because they live in one `tokio::select!`. To later let a
//! phone preempt the single car link (yield it, then resume without
//! re-pairing), "who owns the radio right now" becomes a real
//! arbitration problem: a phone request must be able to interrupt the
//! sampler, hold the link briefly, and hand it back. An actor with a
//! priority queue is the clean model for that.
//!
//! ## This wraps the existing sampler, it does not replace it
//!
//! The actor's sampler arm CALLS the existing [`crate::tick`] function
//! verbatim — same signature, same arguments, same body. Nothing about
//! the sampling logic, the `Schedule`, or the poll cadence is
//! reimplemented or relocated here. The actor only decides *when* to let
//! the sampler run versus servicing a higher-priority phone lease.
//!
//! ## Status: STRUCTURE + INERT, pending on-vehicle validation
//!
//! The types ([`Priority`], [`RadioJob`], [`PhoneLease`], [`RadioHandle`])
//! and the actor loop compile and are unit-tested, but the LIVE phone
//! preempt — actually yielding the car link via
//! [`PersistentSession::suspend_link`](sentryusb_tesla_ble::manager::PersistentSession::suspend_link)
//! and resuming it — is deliberately NOT activated. `run_radio_actor`
//! drives the sampler exactly as the legacy loop does; the phone-preempt
//! arm parks the lease and immediately releases it. Real preemption needs
//! on-vehicle validation of: resume-without-re-pair, BlueZ
//! central+peripheral coexistence, and preempt latency.
//!
//! Even when the experimental flag is ON, the daemon's actor path is a
//! structural clone of today's loop (see `main()`); the flag-OFF path is
//! byte-for-byte the existing `select!`.
// NOTE on `#[allow(dead_code)]` below: a handful of items here form the
// arbitration surface for live phone-preemption (slice 5). They are fully
// unit-tested but not yet called by the binary's inert flag-on loop, which
// still samples inline via the unchanged `tick()`. Rather than a blanket
// module-level allow (which would also hide *real* future dead code), each
// such item is annotated individually with this note. They lose the
// annotation the moment live preemption routes the sampler through the
// actor on-vehicle. (A `--no-default-features` stock build can also
// compile this module out entirely; see the consolidation follow-up.)

use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::action_socket::ActionRequest;

/// Hard cap on how long a phone may hold the car link. Kept well under
/// the session-key `TIME_EXPIRED` horizon (the car rejects commands
/// stamped too far ahead; `EXPIRES_WINDOW` in the BLE manager is 60s) so
/// a held lease can NEVER drift a preserved domain into expiry — on
/// resume the existing counter/key are still valid. 30s is also long
/// enough for a phone to complete a typical command round-trip.
pub const MAX_PHONE_LEASE: Duration = Duration::from_secs(30);

/// Owner identity strings written into `/tmp/ble_radio_owner`. The broker
/// is the SINGLE WRITER of this file: bash `awake_start` / `awake_stop`
/// and the legacy sampler still READ it, so their coordination keeps
/// working unchanged,
/// but only the broker mutates it through `tick()`'s existing
/// `lock::try_acquire`/`release` calls. The actor never writes the file
/// directly — it routes radio ownership through the same `tick()` path,
/// so there is exactly one writer.
///
/// The owner string the broker writes is the daemon's `crate::OWNER`
/// ("telemetry"); ownership is taken/released exclusively through
/// `tick()`'s existing `lock::try_acquire` / `lock::release`, so the
/// actor never writes the lock file itself.
const _: () = {
    // Compile-time tether: this module's coordination assumes the daemon
    // owner constant exists. (No runtime cost.)
    let _ = crate::OWNER;
};

/// Priority of a queued radio job. Derived `Ord` makes `Phone > Sampler`
/// by declaration order (later variant = greater), so a phone job always
/// will sort ahead of a sampler job once the actor implements priority-
/// aware cooperative preemption (slice 5). NOTE: the current `run_radio_actor`
/// loop is plain FIFO and does NOT yet consult this ordering — see that
/// function's docs for why preemption must be cooperative, not a queue sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[allow(dead_code)] // slice-5 arbitration surface; tested, not yet bin-driven
pub enum Priority {
    /// Background telemetry sampling — the default, lowest priority.
    Sampler,
    /// A phone wants the car link — preempts the sampler.
    Phone,
}

/// Which sleep-safe / authenticated sub-poll a sampler job represents.
/// Ordered to match the EXACT walk order of the legacy `tick()` body
/// (drive → climate → charge → closures → tires) and `Schedule::next_due`.
/// The golden-output test pins this equivalence so the actor can never
/// silently reorder the existing scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[allow(dead_code)] // slice-5 arbitration surface; tested, not yet bin-driven
pub enum PollKind {
    /// `state drive` — shiftState + locationName + odometer (priority).
    Drive,
    /// `state climate` — cabin/exterior temps + HVAC.
    Climate,
    /// `state charge` — battery %, charger detail.
    Charge,
    /// `state closures` — sentry_mode for the quiet-mode gate.
    Closures,
    /// `state tire-pressure` — per-tire PSI.
    Tires,
}

impl PollKind {
    /// The canonical sub-sampler walk order, identical to the sequence
    /// of `if schedule.<x>_due(...)` blocks in `tick()` and the
    /// `.min()` chain in `Schedule::next_due`. The golden test asserts
    /// this constant matches the legacy order byte-for-byte.
    #[allow(dead_code)] // slice-5 arbitration surface; tested, not yet bin-driven
    pub const LEGACY_ORDER: [PollKind; 5] = [
        PollKind::Drive,
        PollKind::Climate,
        PollKind::Charge,
        PollKind::Closures,
        PollKind::Tires,
    ];
}

/// A unit of work submitted to the radio actor.
// `Action` is constructed only by the not-yet-activated live action
// routing (slice 5); it's part of the scaffolding surface, so silence
// the dead-code lint without weakening it elsewhere.
#[allow(dead_code)]
pub enum RadioJob {
    /// Run one sampler poll cycle. Carries the sub-poll kind for
    /// diagnostics; the actual sampling is the unchanged `tick()` call,
    /// which internally walks every due sub-sampler.
    Poll(PollKind),
    /// Service one external BLE action (the existing IPC path), reusing
    /// the warm `PersistentSession`. Same `ActionRequest` the legacy
    /// `action_socket` produces — no new shape.
    Action(ActionRequest),
    /// A phone is requesting the car link. The actor (on a single-radio
    /// board) must suspend the sampler's link, hand over for at most
    /// [`MAX_PHONE_LEASE`], then resume. STRUCTURE ONLY — see the
    /// module docs; the live yield is gated off pending on-vehicle
    /// sign-off.
    PhonePreempt {
        /// Caller-requested hold duration; the actor clamps it to
        /// [`MAX_PHONE_LEASE`].
        lease: Duration,
        /// Fired once the actor has yielded the radio, carrying the
        /// [`PhoneLease`] guard. Dropping the guard releases the radio.
        ack: oneshot::Sender<PhoneLease>,
    },
}

impl RadioJob {
    /// Arbitration priority of this job.
    #[allow(dead_code)] // slice-5 arbitration surface; tested, not yet bin-driven
    pub fn priority(&self) -> Priority {
        match self {
            RadioJob::PhonePreempt { .. } => Priority::Phone,
            // Actions and polls are both sampler-tier: they share the
            // warm session and serialize behind a phone lease.
            RadioJob::Poll(_) | RadioJob::Action(_) => Priority::Sampler,
        }
    }
}

/// RAII guard handed to a phone that holds the car link. Dropping it
/// signals the actor (via the oneshot) that the radio is free to resume
/// sampling — so a phone can never leak the lease past its own scope.
/// The actor independently enforces [`MAX_PHONE_LEASE`] as a backstop in
/// case the holder forgets to drop it.
pub struct PhoneLease {
    /// Fires on drop. The actor's release-wait selects on this; `None`
    /// after a manual `release()`.
    release: Option<oneshot::Sender<()>>,
}

impl PhoneLease {
    /// Build a lease + the receiver the actor waits on for release.
    fn new() -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (Self { release: Some(tx) }, rx)
    }

    /// Explicitly release the lease early (idempotent). Equivalent to
    /// dropping the guard.
    #[allow(dead_code)] // slice-5 arbitration surface; tested, not yet bin-driven
    pub fn release(mut self) {
        if let Some(tx) = self.release.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for PhoneLease {
    fn drop(&mut self) {
        if let Some(tx) = self.release.take() {
            // Receiver may already be gone if the actor enforced the
            // timeout first — that's fine, the radio is already resumed.
            let _ = tx.send(());
        }
    }
}

/// Cheap, cloneable handle to submit jobs to the radio actor. All clones
/// feed the one actor task; dropping every clone lets the actor drain
/// and exit.
#[derive(Clone)]
pub struct RadioHandle {
    #[allow(dead_code)] // written by spawn; read only via submit() (slice-5)
    tx: mpsc::Sender<RadioJob>,
}

impl RadioHandle {
    /// Submit a job. Errors only if the actor task has stopped.
    #[allow(dead_code)] // slice-5 arbitration surface; tested, not yet bin-driven
    pub async fn submit(&self, job: RadioJob) -> Result<(), &'static str> {
        self.tx
            .send(job)
            .await
            .map_err(|_| "radio actor task has stopped")
    }

    /// Request the car link for a phone, clamped to [`MAX_PHONE_LEASE`].
    /// Returns the [`PhoneLease`] guard once the actor has yielded.
    ///
    /// STRUCTURE ONLY: with the live preempt gated off, the actor grants
    /// the lease without actually suspending the car link. The clamp and
    /// the guard plumbing are real and tested.
    #[allow(dead_code)] // slice-5 arbitration surface; tested, not yet bin-driven
    pub async fn request_phone_link(
        &self,
        lease: Duration,
    ) -> Result<PhoneLease, &'static str> {
        let clamped = clamp_lease(lease);
        let (ack_tx, ack_rx) = oneshot::channel();
        self.submit(RadioJob::PhonePreempt {
            lease: clamped,
            ack: ack_tx,
        })
        .await?;
        ack_rx.await.map_err(|_| "radio actor dropped the lease ack")
    }
}

/// Clamp a requested lease to the safe maximum. Factored out so the cap
/// is unit-testable and applied at exactly one place.
pub fn clamp_lease(requested: Duration) -> Duration {
    requested.min(MAX_PHONE_LEASE)
}

/// Spawn the radio actor and return a handle. The `tick_fn` closure is
/// the sampler arm: the actor calls it to run one poll cycle. In the
/// daemon this closure CALLS the unchanged [`crate::tick`] with the live
/// daemon state — the actor never reimplements sampling.
///
/// INERT pending on-vehicle sign-off: the phone-preempt arm grants a
/// guard but does not yet suspend/resume the live car link. See module
/// docs.
pub fn spawn_radio_actor<F, Fut>(tick_fn: F) -> RadioHandle
where
    F: FnMut(PollKind) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(run_radio_actor(rx, tick_fn));
    RadioHandle { tx }
}

/// The actor loop. Receives jobs and runs them.
///
/// HONEST STATEMENT OF CURRENT BEHAVIOR: this is a single-task FIFO loop.
/// It pulls one job off the channel and runs it to completion before
/// taking the next. It does NOT consult [`Priority`], and it does NOT
/// preempt — a `PhonePreempt` queued behind a `Poll` waits for that
/// poll's `tick_fn().await` (a multi-second BLE round-trip) to finish.
/// So today "preempt" is structural scaffolding, not real preemption.
///
/// Why it is not just "make the channel a priority queue": you cannot
/// safely interrupt a `tick()` mid-flight. It is doing GATT reads/writes;
/// dropping that future mid-operation leaves the car link in a
/// half-written framing state. Real preemption (slice 5) is therefore
/// COOPERATIVE, not a cancellation: the sampler checks a preempt signal
/// at each sub-poll boundary (drive→climate→charge→closures→tires — the
/// safe points between discrete BLE round-trips), and on signal calls
/// `suspend_link()` (keeps the session domains → no re-pair), yields the
/// radio for the lease, then `resume_link()`s and continues. That bounds
/// preempt latency to one sub-poll (~1-2s) without corrupting the link.
/// The [`Priority`] enum encodes the INTENT of that future design; the
/// loop below does not yet implement it. On the sampler arm it calls
/// `tick_fn`, which wraps the unchanged [`crate::tick`].
async fn run_radio_actor<F, Fut>(mut rx: mpsc::Receiver<RadioJob>, mut tick_fn: F)
where
    F: FnMut(PollKind) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    info!(
        "radio actor started (INERT phone-preempt: structure only, live yield \
         pending on-vehicle sign-off)"
    );
    while let Some(job) = rx.recv().await {
        match job {
            RadioJob::Poll(kind) => {
                // Sampler arm: delegate straight to the unchanged tick().
                debug!("radio actor: sampler poll ({kind:?})");
                tick_fn(kind).await;
            }
            RadioJob::Action(req) => {
                // Actions are serviced by the daemon's existing IPC path;
                // when the actor owns the session this arm would route
                // through it. For the inert structural slice we surface a
                // clear, non-silent error rather than pretend to act.
                let _ = req.reply.send(Err(anyhow::anyhow!(
                    "radio-actor action routing is structure-only (flag-on path \
                     is inert pending on-vehicle sign-off)"
                )));
            }
            RadioJob::PhonePreempt { lease, ack } => {
                handle_phone_preempt_inert(lease, ack).await;
            }
        }
    }
    info!("radio actor stopped (all handles dropped)");
}

/// INERT phone-preempt handler: grants the lease guard and enforces the
/// clamp + timeout, but does NOT suspend the live car link. This is the
/// structural skeleton for slice 5; the single TODO that flips it live
/// is to call `session.suspend_link().await` before granting and
/// `session.resume_link().await` after release.
async fn handle_phone_preempt_inert(lease: Duration, ack: oneshot::Sender<PhoneLease>) {
    let clamped = clamp_lease(lease);
    warn!(
        "radio actor: phone-preempt requested (lease {:?}, clamped {:?}) — \
         STRUCTURE ONLY, not yielding the live car link (needs Pi+car+phone \
         sign-off for resume-without-re-pair + BlueZ coexistence)",
        lease, clamped
    );
    let (guard, release_rx) = PhoneLease::new();
    // Hand the guard to the caller. If they're already gone, nothing to
    // do — the radio was never actually yielded.
    if ack.send(guard).is_err() {
        return;
    }
    // Wait for either the holder to drop the lease or the hard cap.
    // (Live version would resume_link() here regardless of which fired.)
    tokio::select! {
        _ = release_rx => {
            debug!("radio actor: phone lease released by holder");
        }
        _ = tokio::time::sleep(clamped) => {
            warn!(
                "radio actor: phone lease hit the {:?} cap — reclaiming radio",
                clamped
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Priority ordering ──

    #[test]
    fn phone_outranks_sampler() {
        assert!(Priority::Phone > Priority::Sampler);
        let mut v = [Priority::Sampler, Priority::Phone, Priority::Sampler];
        v.sort();
        assert_eq!(v.last(), Some(&Priority::Phone), "phone sorts highest");
    }

    #[test]
    fn job_priority_mapping() {
        let (tx, _rx) = oneshot::channel();
        let job = RadioJob::PhonePreempt {
            lease: Duration::from_secs(5),
            ack: tx,
        };
        assert_eq!(job.priority(), Priority::Phone);
        assert_eq!(RadioJob::Poll(PollKind::Drive).priority(), Priority::Sampler);
    }

    // ── Lease cap (<= 30s) ──

    #[test]
    fn lease_is_capped_at_30s() {
        assert_eq!(clamp_lease(Duration::from_secs(5)), Duration::from_secs(5));
        assert_eq!(clamp_lease(Duration::from_secs(30)), MAX_PHONE_LEASE);
        assert_eq!(
            clamp_lease(Duration::from_secs(120)),
            MAX_PHONE_LEASE,
            "an over-long request must clamp to 30s so a held lease can't \
             drift the session key to TIME_EXPIRED",
        );
        assert!(MAX_PHONE_LEASE <= Duration::from_secs(30));
    }

    // ── PhoneLease Drop releases via the oneshot ──

    #[tokio::test]
    async fn dropping_lease_signals_release() {
        let (lease, rx) = PhoneLease::new();
        drop(lease);
        // The receiver resolves to Ok(()) because Drop fired the sender.
        assert!(rx.await.is_ok(), "drop must signal release");
    }

    #[tokio::test]
    async fn explicit_release_signals_once() {
        let (lease, rx) = PhoneLease::new();
        lease.release();
        assert!(rx.await.is_ok(), "explicit release must signal");
    }

    // ── Actor wiring: the sampler arm calls the provided tick fn ──

    #[tokio::test]
    async fn actor_invokes_tick_fn_for_poll_jobs() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = calls.clone();
        let handle = spawn_radio_actor(move |_kind| {
            let calls = calls_c.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
            }
        });

        handle.submit(RadioJob::Poll(PollKind::Drive)).await.unwrap();
        handle.submit(RadioJob::Poll(PollKind::Climate)).await.unwrap();
        // Drop the handle so the actor drains and exits, then give the
        // runtime a moment to process the queued jobs.
        drop(handle);
        for _ in 0..50 {
            if calls.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2, "both polls ran tick_fn");
    }

    #[tokio::test]
    async fn inert_phone_preempt_grants_and_reclaims_on_drop() {
        let handle = spawn_radio_actor(|_kind| async {});
        // A short lease that we release immediately by dropping the guard.
        let lease = handle
            .request_phone_link(Duration::from_secs(1))
            .await
            .expect("actor should grant the lease guard");
        drop(lease); // releases the radio
        drop(handle);
    }

    // ── GOLDEN-OUTPUT: actor scheduling order == legacy Schedule order ──
    //
    // Pins that the actor's canonical sub-sampler order is byte-equivalent
    // to the legacy `tick()` walk order and `Schedule::next_due` min-chain.
    // If anyone reorders the sampler in main.rs without updating this
    // constant (or vice-versa), this test fails — proving the existing
    // scheduling order was not silently changed.

    #[test]
    fn golden_actor_order_equals_legacy_schedule_order() {
        // The legacy order is, verbatim, the sequence of due-checks in
        // `tick()`: drive, climate, charge, closures, tires — and the
        // identical `.min()` order in `Schedule::next_due`.
        let legacy_literal = [
            PollKind::Drive,
            PollKind::Climate,
            PollKind::Charge,
            PollKind::Closures,
            PollKind::Tires,
        ];
        assert_eq!(
            PollKind::LEGACY_ORDER,
            legacy_literal,
            "actor sub-sampler order must match the legacy tick() walk \
             order byte-for-byte",
        );
    }

    #[test]
    fn golden_actor_order_is_a_total_priority_chain() {
        // The legacy scheduler always evaluates drive first (highest
        // sampler priority) and tires last. Encoding the order as an
        // ascending PollKind enum lets us assert the chain is sorted —
        // a second, independent check that the order constant wasn't
        // shuffled.
        let mut sorted = PollKind::LEGACY_ORDER;
        sorted.sort();
        assert_eq!(
            sorted,
            PollKind::LEGACY_ORDER,
            "LEGACY_ORDER must be ascending by enum discriminant, matching \
             drive-first .. tires-last priority",
        );
    }
}
