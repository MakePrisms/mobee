//! MCP spend authority — budget caps before any pay reaches piece-6 SM.
//!
//! Caps bind from `~/.mobee` config only. Tool args that try to set/override
//! `per_job` / `total` are ignored by callers; this gate never reads them.

use crate::home::MobeeConfig;

/// Fail-closed refusal — never a silent clamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetRefuse {
    /// Amount exceeds the per-job cap (checked first).
    PerJob { amount: u64, per_job_cap: u64 },
    /// Amount fits per-job but exceeds remaining total budget.
    Total {
        amount: u64,
        remaining: u64,
        total_cap: u64,
    },
}

impl std::fmt::Display for BudgetRefuse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PerJob {
                amount,
                per_job_cap,
            } => write!(
                formatter,
                "budget refused: amount {amount} exceeds per-job cap {per_job_cap}"
            ),
            Self::Total {
                amount,
                remaining,
                total_cap,
            } => write!(
                formatter,
                "budget refused: amount {amount} exceeds remaining total {remaining} (total cap {total_cap})"
            ),
        }
    }
}

impl std::error::Error for BudgetRefuse {}

/// In-process allowance gate. Session-scoped spent total (MCP process lifetime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetGate {
    per_job_cap: u64,
    total_cap: u64,
    spent: u64,
}

impl BudgetGate {
    pub fn new(per_job_cap: u64, total_cap: u64) -> Self {
        Self {
            per_job_cap,
            total_cap,
            spent: 0,
        }
    }

    pub fn from_config(config: &MobeeConfig) -> Self {
        Self::new(config.per_job_budget_sats, config.total_budget_sats)
    }

    pub fn per_job_cap(&self) -> u64 {
        self.per_job_cap
    }

    pub fn total_cap(&self) -> u64 {
        self.total_cap
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }

    pub fn remaining(&self) -> u64 {
        self.total_cap.saturating_sub(self.spent)
    }

    /// Check only — does not mutate. Distinct errors for per-job vs total.
    pub fn check(&self, amount: u64) -> Result<(), BudgetRefuse> {
        if amount > self.per_job_cap {
            return Err(BudgetRefuse::PerJob {
                amount,
                per_job_cap: self.per_job_cap,
            });
        }
        let remaining = self.remaining();
        if amount > remaining {
            return Err(BudgetRefuse::Total {
                amount,
                remaining,
                total_cap: self.total_cap,
            });
        }
        Ok(())
    }

    /// Fail-closed check then commit. On `Err`, spent is unchanged (refuse before effect).
    pub fn authorize_and_commit(&mut self, amount: u64) -> Result<(), BudgetRefuse> {
        self.check(amount)?;
        self.spent = self.spent.saturating_add(amount);
        Ok(())
    }

    /// Authorize, run `effect` only on allow, commit spent. Refuse leaves spent untouched
    /// and never calls `effect`.
    pub fn authorize_then<T>(
        &mut self,
        amount: u64,
        effect: impl FnOnce() -> T,
    ) -> Result<T, BudgetRefuse> {
        self.check(amount)?;
        let result = effect();
        self.spent = self.spent.saturating_add(amount);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn exceed_per_job_refuses_with_distinct_error() {
        let mut gate = BudgetGate::new(21, 100);
        let err = gate.authorize_and_commit(22).expect_err("refuse");
        assert!(matches!(
            err,
            BudgetRefuse::PerJob {
                amount: 22,
                per_job_cap: 21
            }
        ));
        assert_eq!(gate.spent(), 0);
        assert!(err.to_string().contains("per-job"));
    }

    #[test]
    fn boundary_per_job_pass_then_plus_one_refuse() {
        let mut gate = BudgetGate::new(21, 100);
        gate.authorize_and_commit(21).expect("boundary pass");
        assert_eq!(gate.spent(), 21);
        let err = gate.authorize_and_commit(22).expect_err("plus one");
        assert!(matches!(err, BudgetRefuse::PerJob { .. }));
        assert_eq!(gate.spent(), 21);
    }

    #[test]
    fn boundary_remaining_total_pass_then_plus_one_refuse() {
        let mut gate = BudgetGate::new(50, 100);
        gate.authorize_and_commit(50).expect("first");
        gate.authorize_and_commit(50).expect("exact remaining");
        assert_eq!(gate.spent(), 100);
        let err = gate.authorize_and_commit(1).expect_err("over total");
        assert!(matches!(
            err,
            BudgetRefuse::Total {
                amount: 1,
                remaining: 0,
                total_cap: 100
            }
        ));
        assert!(err.to_string().contains("remaining total"));
    }

    #[test]
    fn per_job_vs_total_distinct_errors() {
        let mut gate = BudgetGate::new(30, 50);
        // Fits per-job, exceeds remaining total (remaining == total at start? use spent)
        gate.authorize_and_commit(30).expect("seed spend");
        // remaining=20; amount=25 ≤ per_job 30 but > remaining → Total
        let total_err = gate.authorize_and_commit(25).expect_err("total");
        assert!(matches!(total_err, BudgetRefuse::Total { remaining: 20, .. }));

        let mut gate2 = BudgetGate::new(30, 100);
        // amount > per_job with headroom in total → PerJob
        let job_err = gate2.authorize_and_commit(31).expect_err("per-job");
        assert!(matches!(job_err, BudgetRefuse::PerJob { .. }));
        assert_eq!(gate2.spent(), 0);
    }

    #[test]
    fn refuse_before_effect() {
        let mut gate = BudgetGate::new(10, 10);
        let mut fired = false;
        let err = gate
            .authorize_then(11, || {
                fired = true;
                "paid"
            })
            .expect_err("refuse");
        assert!(!fired);
        assert_eq!(gate.spent(), 0);
        assert!(matches!(err, BudgetRefuse::PerJob { .. }));

        let out = gate
            .authorize_then(10, || {
                fired = true;
                "paid"
            })
            .expect("allow");
        assert!(fired);
        assert_eq!(out, "paid");
        assert_eq!(gate.spent(), 10);
    }

    #[test]
    fn concurrent_spends_never_exceed_total() {
        let gate = Arc::new(Mutex::new(BudgetGate::new(50, 100)));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let gate = Arc::clone(&gate);
            handles.push(thread::spawn(move || {
                let mut guard = gate.lock().expect("lock");
                guard.authorize_and_commit(50).is_ok()
            }));
        }
        let oks: usize = handles
            .into_iter()
            .map(|handle| usize::from(handle.join().expect("join")))
            .sum();
        let spent = gate.lock().expect("lock").spent();
        assert!(oks <= 2, "oks={oks}");
        assert!(spent <= 100, "spent={spent}");
        assert_eq!(spent, oks as u64 * 50);
    }

    #[test]
    fn from_config_binds_caps_not_tool_args() {
        let config = MobeeConfig {
            per_job_budget_sats: 7,
            total_budget_sats: 21,
            ..MobeeConfig::default()
        };
        let gate = BudgetGate::from_config(&config);
        assert_eq!(gate.per_job_cap(), 7);
        assert_eq!(gate.total_cap(), 21);
        // Tool-arg-shaped numbers must not appear unless config says so.
        assert_ne!(gate.per_job_cap(), 999);
    }
}
