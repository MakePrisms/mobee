//! MCP spend authority — budget caps before any pay reaches piece-6 SM.
//!
//! Caps bind from `~/.mobee` config only. Tool args that try to set/override
//! `per_job` / `total` are ignored by callers; this gate never reads them.
//!
//! Spent-total is durable under `~/.mobee/spent.toml` and is written **before** the
//! pay effect (write-before-effect). Crash after persist / before effect shrinks
//! remaining allowance — fail-closed vs restart-resets-allowance.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::home::{MobeeConfig, MobeeHome};

const SPENT_FILE: &str = "spent.toml";

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
    /// Durable spent persist failed — effect must not run.
    Persist(String),
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
            Self::Persist(detail) => write!(formatter, "budget spent persist failed: {detail}"),
        }
    }
}

impl std::error::Error for BudgetRefuse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SpentFile {
    spent_sats: u64,
}

/// Allowance gate with durable spent-total under the packaged home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetGate {
    per_job_cap: u64,
    total_cap: u64,
    spent: u64,
    /// When set, spent is loaded/persisted here (write-before-effect).
    spent_path: Option<PathBuf>,
}

impl BudgetGate {
    /// In-memory gate (tests / callers that do not need durability).
    pub fn new(per_job_cap: u64, total_cap: u64) -> Self {
        Self {
            per_job_cap,
            total_cap,
            spent: 0,
            spent_path: None,
        }
    }

    /// Caps from config; spent starts at 0 and is not durable.
    pub fn from_config(config: &MobeeConfig) -> Self {
        Self::new(config.per_job_budget_sats, config.total_budget_sats)
    }

    /// Caps from home config; spent loaded from `~/.mobee/spent.toml` (created on first write).
    pub fn from_home(home: &MobeeHome) -> Result<Self, BudgetRefuse> {
        let spent_path = home.root.join(SPENT_FILE);
        let spent = load_spent(&spent_path)?;
        Ok(Self {
            per_job_cap: home.config.per_job_budget_sats,
            total_cap: home.config.total_budget_sats,
            spent,
            spent_path: Some(spent_path),
        })
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

    pub fn spent_path(&self) -> Option<&Path> {
        self.spent_path.as_deref()
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

    /// Fail-closed check then durable commit (write-before any external effect).
    pub fn authorize_and_commit(&mut self, amount: u64) -> Result<(), BudgetRefuse> {
        self.check(amount)?;
        let next = self.spent.saturating_add(amount);
        self.persist_spent(next)?;
        self.spent = next;
        Ok(())
    }

    /// Authorize, **persist spent**, then run `effect`. Refuse leaves spent untouched
    /// and never calls `effect`. Persist failure never calls `effect`.
    pub fn authorize_then<T>(
        &mut self,
        amount: u64,
        effect: impl FnOnce() -> T,
    ) -> Result<T, BudgetRefuse> {
        self.check(amount)?;
        let next = self.spent.saturating_add(amount);
        self.persist_spent(next)?;
        self.spent = next;
        Ok(effect())
    }

    fn persist_spent(&self, spent: u64) -> Result<(), BudgetRefuse> {
        let Some(path) = self.spent_path.as_ref() else {
            return Ok(());
        };
        write_spent(path, spent)
    }
}

fn load_spent(path: &Path) -> Result<u64, BudgetRefuse> {
    if !path.exists() {
        return Ok(0);
    }
    let raw = fs::read_to_string(path).map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    let parsed: SpentFile =
        toml::from_str(&raw).map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    Ok(parsed.spent_sats)
}

fn write_spent(path: &Path, spent_sats: u64) -> Result<(), BudgetRefuse> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    }
    let raw = toml::to_string_pretty(&SpentFile { spent_sats })
        .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    let tmp = path.with_extension("toml.tmp");
    {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        let mut file = options
            .open(&tmp)
            .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
        file.write_all(raw.as_bytes())
            .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
        file.write_all(b"\n")
            .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
        file.sync_all()
            .map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    }
    fs::rename(&tmp, path).map_err(|error| BudgetRefuse::Persist(error.to_string()))?;
    sync_parent_directory(path)?;
    Ok(())
}

fn sync_parent_directory(path: &Path) -> Result<(), BudgetRefuse> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|error| BudgetRefuse::Persist(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_home(label: &str) -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "mobee-budget-{label}-{}-{id}",
            std::process::id()
        ))
    }

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
        gate.authorize_and_commit(30).expect("seed spend");
        let total_err = gate.authorize_and_commit(25).expect_err("total");
        assert!(matches!(total_err, BudgetRefuse::Total { remaining: 20, .. }));

        let mut gate2 = BudgetGate::new(30, 100);
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
        assert_ne!(gate.per_job_cap(), 999);
    }

    #[test]
    fn durable_spent_survives_reload_write_before_effect() {
        let root = temp_home("durable");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        let spent_path = gate.spent_path().expect("path").to_path_buf();
        let mut effect_fired = false;
        gate.authorize_then(21, || {
            // Spent must already be durable before effect runs.
            let on_disk = load_spent(&spent_path).expect("load");
            assert_eq!(on_disk, 21);
            effect_fired = true;
            "ok"
        })
        .expect("allow");
        assert!(effect_fired);
        assert_eq!(gate.spent(), 21);

        let reloaded = BudgetGate::from_home(&home).expect("reload");
        assert_eq!(reloaded.spent(), 21);
        assert_eq!(reloaded.remaining(), home.config.total_budget_sats - 21);
    }

    #[test]
    fn durable_refuse_leaves_spent_file_unchanged() {
        let root = temp_home("refuse-persist");
        let _ = fs::remove_dir_all(&root);
        let home = home::bootstrap(&root).expect("bootstrap");
        let mut gate = BudgetGate::from_home(&home).expect("gate");
        gate.authorize_and_commit(10).expect("seed");
        let err = gate
            .authorize_then(home.config.per_job_budget_sats + 1, || "nope")
            .expect_err("refuse");
        assert!(matches!(err, BudgetRefuse::PerJob { .. }));
        assert_eq!(gate.spent(), 10);
        assert_eq!(load_spent(gate.spent_path().expect("path")).expect("load"), 10);
    }
}
