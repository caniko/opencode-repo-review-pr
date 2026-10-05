//! Account-wide reservations. No calendar-based or guessed provider balance resets.
use crate::storage;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Period {
    pub start: u64,
    pub end: u64,
    pub allowance: u32,
    pub reconciled_usage: u32,
    pub evidence: String,
    pub no_overage: bool,
    /// Charge receipts proven to be included in reconciled_usage.
    #[serde(default)]
    pub included_requests: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Settlement {
    Reserved,
    Charged,
    Released,
    Reconciled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    cost: u32,
    period: u64,
    settlement: Settlement,
    evidence: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    account: String,
    last_seen: u64,
    periods: Vec<Period>,
    entries: BTreeMap<String, Entry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Balance {
    pub account: String,
    pub period: Option<Period>,
    pub reserved: u32,
    pub charged: u32,
    pub available: u32,
    pub confirmed: bool,
}

pub struct Budget {
    path: PathBuf,
    account: String,
}

impl Budget {
    pub fn new(root: &Path, account: &str) -> Self {
        Self {
            path: root
                .join("accounts")
                .join(format!("{}.json", storage::digest(account.as_bytes()))),
            account: account.into(),
        }
    }

    fn load(&self) -> Result<Ledger> {
        let ledger = storage::read(&self.path)?.unwrap_or(Ledger {
            version: 1,
            account: self.account.clone(),
            last_seen: 0,
            periods: vec![],
            entries: BTreeMap::new(),
        });
        if ledger.version != 1 || ledger.account != self.account {
            bail!("incompatible credit ledger");
        }
        Ok(ledger)
    }

    fn balance(ledger: &Ledger, now: u64) -> Balance {
        let period = ledger.periods.last().cloned();
        let reserved = ledger
            .entries
            .values()
            .filter(|e| matches!(e.settlement, Settlement::Reserved))
            .map(|e| e.cost)
            .fold(0u32, u32::saturating_add);
        let charged = ledger
            .entries
            .values()
            .filter(|e| {
                matches!(e.settlement, Settlement::Charged)
                    && period.as_ref().is_some_and(|p| e.period == p.start)
            })
            .map(|e| e.cost)
            .fold(0u32, u32::saturating_add);
        let confirmed = now >= ledger.last_seen
            && period
                .as_ref()
                .is_some_and(|p| p.no_overage && p.start <= now && now < p.end);
        let available = if confirmed {
            period.as_ref().map_or(0, |p| {
                p.allowance
                    .saturating_sub(p.reconciled_usage)
                    .saturating_sub(reserved)
                    .saturating_sub(charged)
            })
        } else {
            0
        };
        Balance {
            account: ledger.account.clone(),
            period,
            reserved,
            charged,
            available,
            confirmed,
        }
    }

    pub fn show(&self, now: u64) -> Result<Balance> {
        if !self.path.exists() {
            return Ok(Self::balance(&self.load()?, now));
        }
        let _lock = storage::lock(&self.path.with_extension("lock"))?;
        Ok(Self::balance(&self.load()?, now))
    }

    pub fn reconcile(&self, period: Period, now: u64) -> Result<()> {
        if !period.no_overage
            || period.allowance > 50
            || period.reconciled_usage > period.allowance
            || period.evidence.trim().is_empty()
            || period.start > now
            || now >= period.end
        {
            bail!("require a current, evidenced allowance <= 50 and confirmed zero overage");
        }
        let _lock = storage::lock(&self.path.with_extension("lock"))?;
        let mut ledger = self.load()?;
        if now < ledger.last_seen {
            bail!("clock moved backwards");
        }
        let mut included_cost = 0u32;
        let mut seen = std::collections::BTreeSet::new();
        for id in &period.included_requests {
            if !seen.insert(id) {
                bail!("duplicate reconciled charge receipt");
            }
            let entry = ledger
                .entries
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("included charge receipt is unknown"))?;
            if entry.period != period.start
                || !matches!(
                    entry.settlement,
                    Settlement::Charged | Settlement::Reconciled
                )
            {
                bail!("included charge is not settled in this billing period");
            }
            included_cost = included_cost.saturating_add(entry.cost);
        }
        if included_cost > period.reconciled_usage {
            bail!("reconciled usage cannot exclude explicitly included charges");
        }
        if let Some(previous) = ledger.periods.last() {
            if period.start < previous.start
                || (period.start == previous.start && period.end != previous.end)
                || (period.start > previous.start && period.start < previous.end)
            {
                bail!("billing periods cannot overlap or move backwards");
            }
            if period.start == previous.start {
                if period.reconciled_usage < previous.reconciled_usage {
                    bail!("provider usage cannot decrease within a billing period");
                }
                for (id, entry) in &mut ledger.entries {
                    if entry.period == period.start
                        && matches!(entry.settlement, Settlement::Charged)
                        && period.included_requests.contains(id)
                    {
                        entry.settlement = Settlement::Reconciled;
                    }
                }
            }
        }
        ledger.periods.push(period);
        ledger.last_seen = now;
        storage::save(&self.path, &ledger)
    }

    pub fn reserve(&self, id: &str, cost: u32, now: u64) -> Result<bool> {
        if ![1, 3, 10].contains(&cost) || id.is_empty() {
            bail!("invalid credit reservation");
        }
        let _lock = storage::lock(&self.path.with_extension("lock"))?;
        let mut ledger = self.load()?;
        if now < ledger.last_seen {
            return Ok(false);
        }
        if let Some(entry) = ledger.entries.get(id) {
            if entry.cost != cost {
                bail!("request cost changed");
            }
            return Ok(!matches!(entry.settlement, Settlement::Released)
                && ledger.periods.last().is_some_and(|p| {
                    p.no_overage && p.start == entry.period && p.start <= now && now < p.end
                }));
        }
        let balance = Self::balance(&ledger, now);
        if balance.available < cost {
            return Ok(false);
        }
        let period = balance
            .period
            .map(|p| p.start)
            .ok_or_else(|| anyhow::anyhow!("missing billing period"))?;
        ledger.entries.insert(
            id.into(),
            Entry {
                cost,
                period,
                settlement: Settlement::Reserved,
                evidence: "write-ahead reservation".into(),
            },
        );
        ledger.last_seen = now;
        storage::save(&self.path, &ledger)?;
        Ok(true)
    }

    pub fn has_reservation(&self, id: &str, cost: u32, now: u64) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        let _lock = storage::lock(&self.path.with_extension("lock"))?;
        let ledger = self.load()?;
        Ok(now >= ledger.last_seen
            && ledger.entries.get(id).is_some_and(|entry| {
                entry.cost == cost
                    && !matches!(entry.settlement, Settlement::Released)
                    && ledger
                        .periods
                        .last()
                        .is_some_and(|p| p.start == entry.period && p.start <= now && now < p.end)
            }))
    }

    pub fn settle(&self, id: &str, charged: bool, evidence: &str, now: u64) -> Result<()> {
        if evidence.trim().is_empty() {
            bail!("settlement needs provider or operator evidence");
        }
        let _lock = storage::lock(&self.path.with_extension("lock"))?;
        let mut ledger = self.load()?;
        let entry = ledger
            .entries
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("reservation not found"))?;
        if !matches!(entry.settlement, Settlement::Reserved) {
            return Ok(());
        }
        entry.settlement = if charged {
            Settlement::Charged
        } else {
            Settlement::Released
        };
        entry.evidence = evidence.into();
        ledger.last_seen = ledger.last_seen.max(now);
        storage::save(&self.path, &ledger)
    }
}
