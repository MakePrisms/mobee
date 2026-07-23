//! The buyer's **reservation ledger** — the in-flight commitment half of the buyer's
//! money accounting, layered on the daemon-owned state DB (`buyer.sqlite`).
//!
//! # The available invariant
//!
//! ```text
//! available = balance − reserved − spent
//! ```
//!
//! - `balance`  — spendable ecash the wallet reports (passed in; the store never opens the wallet).
//! - `spent`    — the EXISTING budget ledger's spent total (`crate::budget`, folded from
//!                `spent.jsonl`); this crate is the ONLY spend authority. The reservation ledger
//!                never adds to `spent` — a `spent`-state row is a *label*, not a second spend.
//! - `reserved` — the sum of reservations still `Reserved` (in-flight). This is the new concept.
//!
//! Award **reserves** `max_sats` for a job and is refused if that would push `reserved` past
//! `available`; a successful collect **converts** the reservation `reserved → spent`; a job that
//! can no longer be paid has its reservation **released** so the funds become available again.
//!
//! # Why `reserved` counts ONLY `Reserved`-state rows (no double-count)
//!
//! `reserved` sums rows whose state is [`ReservationState::Reserved`] and NOTHING else. When a
//! collect converts a reservation to [`ReservationState::Spent`], the amount leaves the `reserved`
//! term (the row is no longer counted) at the same time the budget ledger's `spent` term takes it
//! up. The amount is therefore in exactly ONE term at a time — never subtracted twice. A
//! [`ReservationState::Released`] row is likewise excluded from `reserved`, so a release frees the
//! funds and a re-release can never free them a second time.
//!
//! # Atomicity
//!
//! Every mutation runs inside a single `BEGIN IMMEDIATE` transaction (see
//! [`crate::buyer::store::BuyerStore`]), so the available-check and the reserve write are ONE
//! write-locked step: two concurrent awards can never both read a stale `reserved` and both slip
//! past the balance.

use std::collections::BTreeMap;

/// Lifecycle state of a single job's reservation.
///
/// The state machine is monotone toward the two terminal labels, which is what makes release and
/// convert idempotent:
///
/// ```text
///   (none) ──reserve──▶ Reserved ──convert──▶ Spent      (terminal)
///                          │
///                          └────release────▶ Released    (terminal for `reserved` accounting)
/// ```
///
/// `Released` may still be converted to `Spent` by reconcile if relay/disk truth shows the job was
/// in fact paid (a bookkeeping correction — neither state counts toward `reserved`, so it never
/// frees or spends twice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationState {
    /// Funds set aside for an in-flight job. The ONLY state counted toward `reserved`.
    Reserved,
    /// The reservation converted on a successful collect. A label only — the budget ledger is the
    /// spend authority; excluded from `reserved`.
    Spent,
    /// The reservation was freed (job no longer payable). Excluded from `reserved`.
    Released,
}

impl ReservationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Spent => "spent",
            Self::Released => "released",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "reserved" => Some(Self::Reserved),
            "spent" => Some(Self::Spent),
            "released" => Some(Self::Released),
            _ => None,
        }
    }
}

/// Success of a [`reserve`](crate::buyer::store::BuyerStore::reserve): the guard passed and the
/// row is `Reserved`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reserved {
    /// A new reservation was written (fresh job, or a previously `Released` row re-reserved).
    /// Carries the `available` computed at the check for observability.
    New { available_before: u64 },
    /// The job was already `Reserved` for the SAME amount — an idempotent replayed award. No new
    /// commitment was made (the amount was already counted), so no available-check is re-applied.
    Idempotent,
}

/// Refusal of a [`reserve`](crate::buyer::store::BuyerStore::reserve). Every refusal leaves the
/// ledger byte-for-byte unchanged (ZERO reserve written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveRefused {
    /// The reservation would push `reserved` past `available` — refused, nothing written.
    InsufficientAvailable { requested: u64, available: u64 },
    /// The job already holds a `Reserved` row for a DIFFERENT amount. A job's amount is fixed by
    /// its signed offer, so a divergent re-reserve is a bug, not an idempotent retry — refused.
    AmountMismatch {
        job_id: String,
        existing: u64,
        requested: u64,
    },
    /// The job's reservation was already converted to `Spent` (already paid); re-reserving it would
    /// be a double commitment. Refused.
    AlreadySpent { job_id: String },
    /// Store / SQLite failure — the effect must not run.
    Store(String),
}

impl std::fmt::Display for ReserveRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientAvailable {
                requested,
                available,
            } => write!(
                formatter,
                "reservation refused: {requested} sat exceeds available {available} sat \
                 (available = balance − reserved − spent)"
            ),
            Self::AmountMismatch {
                job_id,
                existing,
                requested,
            } => write!(
                formatter,
                "reservation refused: job {job_id} already reserves {existing} sat, \
                 cannot re-reserve {requested} sat (offer amount is fixed)"
            ),
            Self::AlreadySpent { job_id } => write!(
                formatter,
                "reservation refused: job {job_id} already converted to spent (already paid)"
            ),
            Self::Store(detail) => write!(formatter, "reservation store error: {detail}"),
        }
    }
}

impl std::error::Error for ReserveRefused {}

/// Outcome of a [`release`](crate::buyer::store::BuyerStore::release). Release is idempotent: only
/// [`Released::Freed`] actually frees funds; every other outcome is a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Released {
    /// A `Reserved` row moved to `Released`; its amount is now available again.
    Freed { amount: u64 },
    /// The reservation was already `Released` — no-op (never frees twice).
    AlreadyReleased,
    /// The reservation is `Spent` (already paid) — NOT freed. Freeing spent funds would be a
    /// phantom credit; release refuses to touch a spent row.
    WasSpent,
    /// No reservation exists for the job — no-op (buyer declined / never awarded).
    NoReservation,
}

/// Outcome of a [`convert_to_spent`](crate::buyer::store::BuyerStore::convert_to_spent). Conversion
/// is exactly-once: only [`Converted::FromReserved`] is the first-time transition; a replayed
/// collect sees [`Converted::AlreadySpent`] and does nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Converted {
    /// The expected path: a `Reserved` row moved to `Spent` on a successful collect.
    FromReserved,
    /// Idempotent replay: the row was already `Spent`. No-op — never double-labels the spend.
    AlreadySpent,
    /// No prior reservation existed (e.g. a collect on a job never awarded through the ledger); a
    /// `Spent` row was inserted so the job is recorded, not left invisible.
    InsertedSpent,
    /// The row was `Released` but relay/disk truth shows the job was in fact paid; corrected to
    /// `Spent`. A bookkeeping fix — neither state counts toward `reserved`.
    FromReleased,
}

/// Caller-supplied disposition of a reserved job during [`reconcile`](crate::buyer::store::BuyerStore::reconcile).
///
/// The store never does relay I/O; the daemon derives these from relay truth + disk (offer/claim
/// liveness via `derive_claim_liveness`, the accept-bind, and the payment journal) and hands the
/// store a verdict per job. This keeps the reconcile transition pure and exhaustively testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobDisposition {
    /// Still payable (awardable, or delivered inside the pay window). Keep the reservation.
    Payable,
    /// No longer payable — offer/claim expired with no delivery, delivery pay-window lapsed,
    /// declined, canceled/superseded, or terminal pay failure. Release the reservation.
    ///
    /// NOTE (phase-3 boundary): a PAYMENT_UNCERTAIN outcome is NOT a `Dead` verdict. Ambiguous pay
    /// results must never auto-release (the funds may have moved); the daemon must classify those as
    /// `Payable` (keep) until the crash-safe payment saga resolves them. The relay-truth → disposition
    /// reconcile driver (and the reserve-on-award / convert-on-collect wiring) lands with the daemon
    /// trade RPCs in #126; this crate ships the pure, exhaustively-tested transition it will call.
    Dead,
    /// Already paid (the payment journal shows a closed/settled attempt). Ensure the reservation is
    /// `Spent`, not a dangling `Reserved`.
    Paid,
}

/// What [`reconcile`](crate::buyer::store::BuyerStore::reconcile) changed. Idempotent: a second run
/// with the same dispositions returns empty `released`/`converted` (nothing left to change).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Job ids whose reservations this pass freed (`Dead`).
    pub released: Vec<String>,
    /// Job ids this pass converted to `Spent` (`Paid`).
    pub converted: Vec<String>,
    /// Job ids left `Reserved` (`Payable`, or already terminal).
    pub kept: Vec<String>,
}

/// Compute `available = balance − reserved − spent`, saturating at 0.
///
/// Done in `i128` so a `reserved + spent` that exceeds `balance` yields 0 (never a wrapping
/// underflow that would fabricate a huge available and let an award slip past).
pub(crate) fn compute_available(balance: u64, reserved: u64, spent: u64) -> u64 {
    let available = balance as i128 - reserved as i128 - spent as i128;
    if available < 0 {
        0
    } else {
        available as u64
    }
}

/// A per-job disposition map for [`reconcile`](crate::buyer::store::BuyerStore::reconcile).
pub type Dispositions = BTreeMap<String, JobDisposition>;
