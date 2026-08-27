use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
};

use anyhow::{Result, ensure};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
pub const DEFAULT_TERMINAL_HISTORY_ROWS: u64 = 10_000;
pub const MAX_TERMINAL_HISTORY_ROWS: u64 = 1_000_000;
pub const MAX_TERMINAL_HISTORY_BYTES: u64 = GIB;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceClaim {
    pub connections: u64,
    pub streams: u64,
    pub workers: u64,
    pub terminals: u64,
    pub attachments: u64,
    pub terminal_memory_bytes: u64,
    pub history_bytes: u64,
    pub file_handles: u64,
    pub uploads: u64,
    pub upload_bytes: u64,
}

impl ResourceClaim {
    pub fn connection() -> Self {
        Self {
            connections: 1,
            ..Self::default()
        }
    }

    pub fn stream() -> Self {
        Self {
            streams: 1,
            ..Self::default()
        }
    }

    pub fn attachment() -> Self {
        Self {
            attachments: 1,
            ..Self::default()
        }
    }

    pub fn file_handle() -> Self {
        Self {
            file_handles: 1,
            ..Self::default()
        }
    }

    pub fn upload(bytes: u64) -> Self {
        Self {
            uploads: 1,
            upload_bytes: bytes,
            ..Self::default()
        }
    }

    pub fn terminal(rows: u32, columns: u32, policy: &ResourcePolicy) -> Result<Self> {
        let cells = u64::from(rows)
            .checked_mul(u64::from(columns))
            .ok_or_else(|| anyhow::anyhow!("terminal cell count overflow"))?;
        let grid_bytes = cells
            .checked_mul(policy.terminal_cell_memory_bytes)
            .ok_or_else(|| anyhow::anyhow!("terminal memory claim overflow"))?;
        Ok(Self {
            terminals: 1,
            terminal_memory_bytes: grid_bytes.max(policy.terminal_base_memory_bytes),
            history_bytes: policy.terminal_history_bytes,
            ..Self::default()
        })
    }

    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            connections: self.connections.checked_add(other.connections)?,
            streams: self.streams.checked_add(other.streams)?,
            workers: self.workers.checked_add(other.workers)?,
            terminals: self.terminals.checked_add(other.terminals)?,
            attachments: self.attachments.checked_add(other.attachments)?,
            terminal_memory_bytes: self
                .terminal_memory_bytes
                .checked_add(other.terminal_memory_bytes)?,
            history_bytes: self.history_bytes.checked_add(other.history_bytes)?,
            file_handles: self.file_handles.checked_add(other.file_handles)?,
            uploads: self.uploads.checked_add(other.uploads)?,
            upload_bytes: self.upload_bytes.checked_add(other.upload_bytes)?,
        })
    }

    fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self {
            connections: self.connections.checked_sub(other.connections)?,
            streams: self.streams.checked_sub(other.streams)?,
            workers: self.workers.checked_sub(other.workers)?,
            terminals: self.terminals.checked_sub(other.terminals)?,
            attachments: self.attachments.checked_sub(other.attachments)?,
            terminal_memory_bytes: self
                .terminal_memory_bytes
                .checked_sub(other.terminal_memory_bytes)?,
            history_bytes: self.history_bytes.checked_sub(other.history_bytes)?,
            file_handles: self.file_handles.checked_sub(other.file_handles)?,
            uploads: self.uploads.checked_sub(other.uploads)?,
            upload_bytes: self.upload_bytes.checked_sub(other.upload_bytes)?,
        })
    }

    fn dimensions(self) -> [(&'static str, u64); 10] {
        [
            ("connections", self.connections),
            ("streams", self.streams),
            ("workers", self.workers),
            ("terminals", self.terminals),
            ("attachments", self.attachments),
            ("terminal_memory_bytes", self.terminal_memory_bytes),
            ("history_bytes", self.history_bytes),
            ("file_handles", self.file_handles),
            ("uploads", self.uploads),
            ("upload_bytes", self.upload_bytes),
        ]
    }

    fn value(self, name: &str) -> u64 {
        self.dimensions()
            .into_iter()
            .find_map(|(candidate, value)| (candidate == name).then_some(value))
            .expect("resource dimension must exist")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLimits {
    pub connections: u64,
    pub streams: u64,
    pub workers: u64,
    pub terminals: u64,
    pub attachments: u64,
    pub terminal_memory_bytes: u64,
    pub history_bytes: u64,
    pub file_handles: u64,
    pub uploads: u64,
    pub upload_bytes: u64,
}

impl ResourceLimits {
    pub fn global_defaults() -> Self {
        Self {
            connections: 1_024,
            streams: 8_192,
            workers: 64,
            terminals: 4_096,
            attachments: 16_384,
            terminal_memory_bytes: 16 * GIB,
            history_bytes: 32 * GIB,
            file_handles: 16_384,
            uploads: 1_024,
            upload_bytes: 512 * GIB,
        }
    }

    pub fn user_defaults() -> Self {
        Self {
            connections: 8,
            streams: 256,
            workers: 1,
            terminals: 64,
            attachments: 256,
            terminal_memory_bytes: 256 * MIB,
            history_bytes: 512 * MIB,
            file_handles: 256,
            uploads: 16,
            upload_bytes: 8 * GIB,
        }
    }

    fn as_claim(self) -> ResourceClaim {
        ResourceClaim {
            connections: self.connections,
            streams: self.streams,
            workers: self.workers,
            terminals: self.terminals,
            attachments: self.attachments,
            terminal_memory_bytes: self.terminal_memory_bytes,
            history_bytes: self.history_bytes,
            file_handles: self.file_handles,
            uploads: self.uploads,
            upload_bytes: self.upload_bytes,
        }
    }

    fn validate(self, scope: &str) -> Result<()> {
        for (resource, limit) in self.as_claim().dimensions() {
            ensure!(limit > 0, "{scope} {resource} limit must be nonzero");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourcePolicy {
    pub global: ResourceLimits,
    pub user: ResourceLimits,
    pub terminal_base_memory_bytes: u64,
    pub terminal_cell_memory_bytes: u64,
    pub terminal_history_rows: u64,
    pub terminal_history_bytes: u64,
}

impl Default for ResourcePolicy {
    fn default() -> Self {
        Self {
            global: ResourceLimits::global_defaults(),
            user: ResourceLimits::user_defaults(),
            terminal_base_memory_bytes: 4 * MIB,
            terminal_cell_memory_bytes: 64,
            terminal_history_rows: DEFAULT_TERMINAL_HISTORY_ROWS,
            terminal_history_bytes: 8 * MIB,
        }
    }
}

impl ResourcePolicy {
    pub fn validate(&self) -> Result<()> {
        self.global.validate("global")?;
        self.user.validate("user")?;
        ensure!(
            self.user.workers == 1,
            "each Unix user must own exactly one managed worker capacity"
        );
        ensure!(
            self.terminal_base_memory_bytes > 0
                && self.terminal_cell_memory_bytes > 0
                && self.terminal_history_rows > 0
                && self.terminal_history_bytes > 0,
            "per-terminal capacity values must be nonzero"
        );
        ensure!(
            self.terminal_history_rows <= MAX_TERMINAL_HISTORY_ROWS,
            "per-terminal history rows exceed the server hard limit"
        );
        ensure!(
            self.terminal_history_bytes <= MAX_TERMINAL_HISTORY_BYTES,
            "per-terminal history bytes exceed the server hard limit"
        );
        ensure!(
            self.user.terminal_memory_bytes >= self.terminal_base_memory_bytes,
            "user terminal memory must fit one terminal base reservation"
        );
        ensure!(
            self.user.history_bytes >= self.terminal_history_bytes,
            "user history capacity must fit one terminal reservation"
        );
        for (resource, user_limit) in self.user.as_claim().dimensions() {
            let global_limit = self.global.as_claim().value(resource);
            ensure!(
                global_limit >= user_limit,
                "global {resource} limit must fit one complete user capacity"
            );
        }
        Ok(())
    }

    pub fn worker_capacity_claim(&self) -> ResourceClaim {
        ResourceClaim {
            workers: 1,
            terminals: self.user.terminals,
            attachments: self.user.attachments,
            terminal_memory_bytes: self.user.terminal_memory_bytes,
            history_bytes: self.user.history_bytes,
            file_handles: self.user.file_handles,
            uploads: self.user.uploads,
            upload_bytes: self.user.upload_bytes,
            ..ResourceClaim::default()
        }
    }
}

#[derive(Debug)]
pub struct QuotaExceeded {
    pub scope: String,
    pub resource: &'static str,
    pub requested: u64,
    pub current: u64,
    pub limit: u64,
}

impl fmt::Display for QuotaExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} quota exceeded for {}: requested {}, current {}, limit {}",
            self.scope, self.resource, self.requested, self.current, self.limit
        )
    }
}

impl std::error::Error for QuotaExceeded {}

#[derive(Clone)]
pub struct ResourcePool {
    inner: Arc<ResourcePoolInner>,
}

struct ResourcePoolInner {
    scope: String,
    limits: ResourceLimits,
    usage: Mutex<ResourceClaim>,
}

impl ResourcePool {
    pub fn new(scope: impl Into<String>, limits: ResourceLimits) -> Result<Self> {
        let scope = scope.into();
        limits.validate(&scope)?;
        Ok(Self {
            inner: Arc::new(ResourcePoolInner {
                scope,
                limits,
                usage: Mutex::new(ResourceClaim::default()),
            }),
        })
    }

    fn reserve(&self, claim: ResourceClaim) -> std::result::Result<PoolReservation, QuotaExceeded> {
        let mut usage = self.inner.usage.lock().expect("resource usage poisoned");
        let next = usage.checked_add(claim).ok_or_else(|| QuotaExceeded {
            scope: self.inner.scope.clone(),
            resource: "counter",
            requested: 1,
            current: u64::MAX,
            limit: u64::MAX,
        })?;
        let limits = self.inner.limits.as_claim();
        for (resource, requested) in claim.dimensions() {
            let current = usage.value(resource);
            let limit = limits.value(resource);
            if next.value(resource) > limit {
                return Err(QuotaExceeded {
                    scope: self.inner.scope.clone(),
                    resource,
                    requested,
                    current,
                    limit,
                });
            }
        }
        *usage = next;
        Ok(PoolReservation {
            pool: self.clone(),
            claim,
        })
    }

    #[cfg(test)]
    fn usage(&self) -> ResourceClaim {
        *self.inner.usage.lock().expect("resource usage poisoned")
    }
}

struct PoolReservation {
    pool: ResourcePool,
    claim: ResourceClaim,
}

impl Drop for PoolReservation {
    fn drop(&mut self) {
        let mut usage = self
            .pool
            .inner
            .usage
            .lock()
            .expect("resource usage poisoned");
        *usage = usage
            .checked_sub(self.claim)
            .expect("resource reservation accounting underflow");
    }
}

pub struct ResourceReservation {
    _reservations: Vec<PoolReservation>,
}

#[derive(Clone)]
pub struct ResourceAccount {
    pools: Vec<ResourcePool>,
}

impl ResourceAccount {
    pub fn standalone(scope: impl Into<String>, limits: ResourceLimits) -> Result<Self> {
        Ok(Self {
            pools: vec![ResourcePool::new(scope, limits)?],
        })
    }

    fn layered(global: ResourcePool, user: ResourcePool) -> Self {
        Self {
            pools: vec![global, user],
        }
    }

    pub fn reserve(
        &self,
        claim: ResourceClaim,
    ) -> std::result::Result<ResourceReservation, QuotaExceeded> {
        let mut reservations = Vec::with_capacity(self.pools.len());
        for pool in &self.pools {
            reservations.push(pool.reserve(claim)?);
        }
        Ok(ResourceReservation {
            _reservations: reservations,
        })
    }
}

#[derive(Clone)]
pub struct ResourceGovernor {
    global: ResourcePool,
    users: Arc<Mutex<HashMap<String, ResourcePool>>>,
    user_limits: ResourceLimits,
}

impl ResourceGovernor {
    pub fn new(policy: &ResourcePolicy) -> Result<Self> {
        policy.validate()?;
        Ok(Self {
            global: ResourcePool::new("global", policy.global)?,
            users: Arc::new(Mutex::new(HashMap::new())),
            user_limits: policy.user,
        })
    }

    pub fn account(&self, user: &str) -> Result<ResourceAccount> {
        ensure!(!user.is_empty(), "resource account user is empty");
        let mut users = self
            .users
            .lock()
            .expect("resource account registry poisoned");
        let pool = match users.get(user) {
            Some(pool) => pool.clone(),
            None => {
                let pool = ResourcePool::new(format!("user {user}"), self.user_limits)?;
                users.insert(user.to_owned(), pool.clone());
                pool
            }
        };
        Ok(ResourceAccount::layered(self.global.clone(), pool))
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Barrier, thread};

    use super::*;

    fn limits(connections: u64) -> ResourceLimits {
        ResourceLimits {
            connections,
            ..ResourceLimits::user_defaults()
        }
    }

    #[test]
    fn failed_reservation_is_atomic_and_drop_releases_usage() {
        let pool = ResourcePool::new("test", limits(1)).unwrap();
        let account = ResourceAccount {
            pools: vec![pool.clone()],
        };
        let reservation = account.reserve(ResourceClaim::connection()).unwrap();
        let error = match account.reserve(ResourceClaim::connection()) {
            Ok(_) => panic!("second connection reservation unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error.resource, "connections");
        assert_eq!(pool.usage().connections, 1);
        drop(reservation);
        assert_eq!(pool.usage(), ResourceClaim::default());
    }

    #[test]
    fn layered_failure_rolls_back_the_global_pool() {
        let global = ResourcePool::new("global", limits(2)).unwrap();
        let user = ResourcePool::new("user", limits(1)).unwrap();
        let account = ResourceAccount::layered(global.clone(), user);
        let first = account.reserve(ResourceClaim::connection()).unwrap();
        assert!(account.reserve(ResourceClaim::connection()).is_err());
        assert_eq!(global.usage().connections, 1);
        drop(first);
        assert_eq!(global.usage().connections, 0);
    }

    #[test]
    fn concurrent_admission_never_exceeds_the_limit() {
        let pool = ResourcePool::new("test", limits(4)).unwrap();
        let account = Arc::new(ResourceAccount {
            pools: vec![pool.clone()],
        });
        let barrier = Arc::new(Barrier::new(9));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let account = account.clone();
            let barrier = barrier.clone();
            threads.push(thread::spawn(move || {
                barrier.wait();
                account.reserve(ResourceClaim::connection()).ok()
            }));
        }
        barrier.wait();
        let reservations = threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(reservations.len(), 4);
        assert_eq!(pool.usage().connections, 4);
        drop(reservations);
        assert_eq!(pool.usage().connections, 0);
    }

    #[test]
    fn terminal_claim_grows_with_initial_grid() {
        let policy = ResourcePolicy::default();
        let normal = ResourceClaim::terminal(24, 80, &policy).unwrap();
        let large = ResourceClaim::terminal(1_000, 1_000, &policy).unwrap();
        assert_eq!(normal.terminal_memory_bytes, 4 * MIB);
        assert_eq!(normal.history_bytes, 8 * MIB);
        assert!(large.terminal_memory_bytes > normal.terminal_memory_bytes);
    }

    #[test]
    fn worker_bundle_reserves_the_complete_user_capacity() {
        let policy = ResourcePolicy::default();
        let claim = policy.worker_capacity_claim();
        assert_eq!(claim.workers, 1);
        assert_eq!(claim.terminals, policy.user.terminals);
        assert_eq!(claim.history_bytes, policy.user.history_bytes);
        assert_eq!(claim.connections, 0);
        assert_eq!(claim.streams, 0);
    }
}
