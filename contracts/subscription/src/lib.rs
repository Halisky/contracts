#![no_std]
//! SynapsVault — Subscription Manager (Soroban / Stellar)
//!
//! Manages recurring 30-day subscription plans for SynapsVault publishers.
//! Publishers create plans; the backend subscribes buyers after payment confirmation
//! and renews each billing cycle. Subscribers can self-cancel with access until period end.

use common::{BUMP, BUMP_THRESH, DAY};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, BytesN,
    ContractExecutable, Env, String,
};

/// Length of one billing cycle in ledgers (30 days).
pub const CYCLE: u32 = 30 * DAY;

/// Shared persistent-storage TTL policy (see `docs/CONTRACTS.md`).
///
/// All SynapsVault contracts use the same rationale for persistent entries:
/// a subscription is a long-lived, user-owned record that must survive long
/// periods of inactivity, so entries are bumped to a full year of ledgers and
/// refreshed whenever they are written or read through an admin-gated path.
/// The threshold is one day short of the bump so that any write refreshes the
/// TTL well before expiry. Contracts that store shorter-lived data (e.g. the
/// registry's 30-day and the lease's 90-day windows) do so because their
/// records are re-created on every use; the subscription record is not, hence
/// the 365-day window here.
const BUMP: u32 = 365 * DAY; // 1-year TTL
const BUMP_THRESH: u32 = BUMP - DAY;

/// Contract version, sourced from Cargo.toml at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A recurring subscription plan offered by a publisher.
///
/// Plans are keyed by `plan_id` and can be deactivated by their publisher to
/// prevent new subscriptions. Existing subscribers are unaffected by
/// deactivation and continue to renew until they cancel.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Unique, publisher-chosen identifier for the plan.
    pub plan_id: String,
    /// Wallet that created the plan and is the only party allowed to deactivate it.
    pub publisher: Address,
    /// Price charged per 30-day billing cycle, denominated in USDC stroops
    /// (7 decimal places). Must be strictly positive.
    pub price_per_cycle: i128,
    /// Whether new subscriptions are currently allowed. Set to `false` by
    /// [`SubscriptionManager::deactivate_plan`].
    pub active: bool,
}

/// A subscriber's enrolment in a [`Plan`].
///
/// Access is granted while `current_period_end > ledger.sequence()`. Each
/// billing cycle is exactly 30 days ([`CYCLE`] ledgers). Cancellation is
/// non-destructive: it only flips `cancelled` (which blocks renewals),
/// leaving access intact until the end of the already-paid period.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Subscription {
    /// Identifier of the plan this subscription belongs to.
    pub plan_id: String,
    /// Wallet receiving access to the plan's content.
    pub subscriber: Address,
    /// Ledger sequence at which the subscription was first created.
    pub started_at: u32,
    /// Ledger sequence at which the current paid period expires. Renewals
    /// extend this by one [`CYCLE`] (30 days).
    pub current_period_end: u32,
    /// Set to `true` by [`SubscriptionManager::cancel`]. Cancelled
    /// subscriptions cannot be renewed.
    pub cancelled: bool,
    /// Number of successful renewals applied to this subscription.
    pub total_renewals: u32,
}

/// Storage keys used by the contract.
///
/// `Admin` and `Version` live in instance storage; `Plan` and `Sub` entries
/// live in persistent storage and are TTL-bumped on every write.
#[contracttype]
pub enum DataKey {
    /// Address authorised to call [`SubscriptionManager::subscribe`] and
    /// [`SubscriptionManager::renew`]. Set once by [`SubscriptionManager::init`].
    Admin,
    /// A [`Plan`] keyed by its `plan_id`.
    Plan(String),
    /// A [`Subscription`] keyed by `(plan_id, subscriber)`.
    Sub(String, Address),
    /// Contract version recorded at `init`.
    Version,
}

/// Error codes returned by the subscription manager.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    /// Caller failed the admin `require_auth` check.
    NotAdmin = 1,
    /// No plan exists for the supplied `plan_id`.
    PlanNotFound = 2,
    /// The plan exists but has been deactivated; new subscriptions are rejected.
    PlanInactive = 3,
    /// The subscriber already has an active, non-cancelled subscription to this plan.
    AlreadySubbed = 4,
    /// No subscription exists for the supplied `(plan_id, subscriber)` pair.
    SubNotFound = 5,
    /// The subscription has been cancelled and can no longer be renewed.
    Cancelled = 6,
    /// `price_per_cycle` was not strictly positive.
    InvalidPrice = 7,
    /// `init` has not been called, so no admin is configured.
    NotInitialised = 8,
    /// `init` has already been called; the admin cannot be overwritten.
    AlreadyInitialised = 9,
    /// A plan with this `plan_id` already exists.
    PlanExists = 10,
    /// The new period end would overflow the ledger sequence.
    Overflow = 11,
}

/// Emitted by `init`. Topics: `("init",)`; data: admin.
#[contractevent(topics = ["init"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitEvent {
    pub admin: Address,
}

/// Emitted by `set_admin`. Topics: `("setadmin",)`; data: new admin.
#[contractevent(topics = ["setadmin"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetAdminEvent {
    pub new_admin: Address,
}

/// Emitted by `create_plan`. Topics: `("plan", plan_id, publisher)`; data: price per cycle.
#[contractevent(topics = ["plan"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanCreatedEvent {
    #[topic]
    pub plan_id: String,
    #[topic]
    pub publisher: Address,
    pub price_per_cycle: i128,
}

/// Emitted by `deactivate_plan`. Topics: `("deactiv", plan_id)`; no data.
#[contractevent(topics = ["deactiv"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanDeactivatedEvent {
    #[topic]
    pub plan_id: String,
}

/// Emitted by `subscribe`. Topics: `("subscribe", plan_id, subscriber)`; data: `current_period_end`.
#[contractevent(topics = ["subscribe"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscribedEvent {
    #[topic]
    pub plan_id: String,
    #[topic]
    pub subscriber: Address,
    pub current_period_end: u32,
}

/// Emitted by `renew`. Topics: `("renew", plan_id, subscriber)`; data: new `current_period_end`.
#[contractevent(topics = ["renew"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenewedEvent {
    #[topic]
    pub plan_id: String,
    #[topic]
    pub subscriber: Address,
    pub current_period_end: u32,
}

/// Emitted by `cancel`. Topics: `("cancel", plan_id, subscriber)`; no data.
#[contractevent(topics = ["cancel"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledEvent {
    #[topic]
    pub plan_id: String,
    #[topic]
    pub subscriber: Address,
}

/// Emitted by `upgrade`. Topics: `("upgrade",)`; data: new WASM hash.
#[contractevent(topics = ["upgrade"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeEvent {
    pub new_wasm_hash: BytesN<32>,
}

#[contract]
pub struct SubscriptionManager;

#[contractimpl]
impl SubscriptionManager {
    /// Initialise the contract by setting the admin wallet.
    ///
    /// Must be called exactly once after deployment. The admin is the only
    /// address permitted to call [`SubscriptionManager::subscribe`] and [`SubscriptionManager::renew`].
    ///
    /// # Errors
    /// - [`Error::AlreadyInitialised`] if an admin is already set. Use
    ///   [`SubscriptionManager::set_admin`] to rotate it.
    pub fn init(env: Env, admin: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialised);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::Version, &String::from_str(&env, VERSION));
        env.storage().instance().extend_ttl(BUMP_THRESH, BUMP);
        InitEvent { admin }.publish(&env);
        Ok(())
    }

    /// Return the configured admin address.
    ///
    /// # Errors
    /// - [`Error::NotInitialised`] if `init` has not been called.
    pub fn admin(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialised)
    }

    /// Rotate the admin to `new_admin`. Requires the current admin's auth.
    ///
    /// # Errors
    /// - [`Error::NotInitialised`] if `init` has not been called.
    pub fn set_admin(env: Env, new_admin: Address) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        SetAdminEvent { new_admin }.publish(&env);
        Ok(())
    }

    /// Return the version recorded at `init`, falling back to the compiled
    /// version. Permissionless.
    pub fn get_version(env: Env) -> String {
        common::get_version(&env, &DataKey::Version, VERSION)
    }

    /// Returns `true` if the stored version matches the compiled version.
    /// Useful for detecting a contract that needs migration after an upgrade.
    pub fn is_compatible(env: Env) -> bool {
        common::is_compatible(&env, &DataKey::Version, VERSION)
    }

    /// Create a new subscription plan.
    ///
    /// Requires authorisation from `publisher`. The plan becomes immediately
    /// active and can be subscribed to by the admin.
    ///
    /// # Parameters
    /// - `publisher`: wallet owning the plan; must sign the transaction.
    /// - `plan_id`: unique identifier for the plan.
    /// - `price_per_cycle`: price per 30-day cycle in USDC stroops; must be `> 0`.
    ///
    /// # Errors
    /// - [`Error::InvalidPrice`] if `price_per_cycle <= 0`.
    /// - [`Error::PlanExists`] if `plan_id` is already taken.
    pub fn create_plan(
        env: Env,
        publisher: Address,
        plan_id: String,
        price_per_cycle: i128,
    ) -> Result<Plan, Error> {
        publisher.require_auth();
        if price_per_cycle <= 0 {
            return Err(Error::InvalidPrice);
        }
        let key = DataKey::Plan(plan_id.clone());
        if env.storage().persistent().has(&key) {
            return Err(Error::PlanExists);
        }
        let plan = Plan {
            plan_id: plan_id.clone(),
            publisher: publisher.clone(),
            price_per_cycle,
            active: true,
        };
        env.storage().persistent().set(&key, &plan);
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
        PlanCreatedEvent {
            plan_id,
            publisher,
            price_per_cycle,
        }
        .publish(&env);
        Ok(plan)
    }

    /// Return the [`Plan`] for `plan_id`. Permissionless.
    ///
    /// # Errors
    /// - [`Error::PlanNotFound`] if no plan exists for `plan_id`.
    pub fn get_plan(env: Env, plan_id: String) -> Result<Plan, Error> {
        let key = DataKey::Plan(plan_id);
        let plan: Plan = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::PlanNotFound)?;
        // Extending TTL on reads to ensure accessed Plans are kept alive
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
        Ok(plan)
    }

    /// Deactivate a plan so no new subscribers can join.
    ///
    /// Requires authorisation from the plan's publisher. Existing subscriptions
    /// remain valid and can still be renewed; only new subscriptions are blocked.
    ///
    /// # Errors
    /// - [`Error::PlanNotFound`] if no plan exists for `plan_id`.
    pub fn deactivate_plan(env: Env, plan_id: String) -> Result<(), Error> {
        let key = DataKey::Plan(plan_id.clone());
        let mut plan: Plan = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::PlanNotFound)?;
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
        plan.publisher.require_auth();
        plan.active = false;
        env.storage().persistent().set(&key, &plan);
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
        PlanDeactivatedEvent { plan_id }.publish(&env);
        Ok(())
    }

    /// Subscribe a buyer to a plan after off-chain payment confirmation.
    ///
    /// Admin-only. Creates a fresh subscription with a 30-day period starting
    /// at the current ledger sequence. If a previous subscription exists but is
    /// cancelled or expired, it is replaced; any still-paid time left on a
    /// cancelled subscription is carried over. `started_at` and `total_renewals` 
    /// are retained if resuming an old subscription.
    ///
    /// # Errors
    /// - [`Error::NotInitialised`] if `init` has not been called.
    /// - [`Error::PlanNotFound`] if the plan does not exist.
    /// - [`Error::PlanInactive`] if the plan has been deactivated.
    /// - [`Error::AlreadySubbed`] if an active, non-cancelled subscription exists.
    pub fn subscribe(
        env: Env,
        plan_id: String,
        subscriber: Address,
    ) -> Result<Subscription, Error> {
        Self::require_admin(&env)?;

        let plan_key = DataKey::Plan(plan_id.clone());
        let plan: Plan = env
            .storage()
            .persistent()
            .get(&plan_key)
            .ok_or(Error::PlanNotFound)?;
        // Extending TTL on reads ensures accessed Plans are kept alive
        env.storage()
            .persistent()
            .extend_ttl(&plan_key, BUMP_THRESH, BUMP);

        if !plan.active {
            return Err(Error::PlanInactive);
        }

        let key = DataKey::Sub(plan_id.clone(), subscriber.clone());
        let now = env.ledger().sequence();
        let mut base = now;
        let mut started_at = now;
        let mut total_renewals = 0;

        if let Some(s) = env.storage().persistent().get::<_, Subscription>(&key) {
            started_at = s.started_at;
            total_renewals = s.total_renewals;
            
            if s.current_period_end > now {
                // Block double-subscribe if still active and not cancelled.
                if !s.cancelled {
                    return Err(Error::AlreadySubbed);
                }
                base = s.current_period_end;
            }
        }

        let sub = Subscription {
            plan_id: plan_id.clone(),
            subscriber: subscriber.clone(),
            started_at,
            current_period_end: base.checked_add(CYCLE).ok_or(Error::Overflow)?,
            cancelled: false,
            total_renewals,
        };
        
        env.storage().persistent().set(&key, &sub);
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
            
        SubscribedEvent {
            plan_id,
            subscriber,
            current_period_end: sub.current_period_end,
        }
        .publish(&env);
        
        Ok(sub)
    }

    /// Renew a subscription by one 30-day billing cycle after payment.
    ///
    /// Admin-only. The new period end is computed as
    /// `max(current_period_end, now) + CYCLE`, so renewals stack from the end
    /// of the current period when it is still in the future, or from `now`
    /// when the subscription has already lapsed.
    ///
    /// # Errors
    /// - [`Error::NotInitialised`] if `init` has not been called.
    /// - [`Error::SubNotFound`] if no subscription exists for the pair.
    /// - [`Error::Cancelled`] if the subscription has been cancelled.
    pub fn renew(env: Env, plan_id: String, subscriber: Address) -> Result<Subscription, Error> {
        Self::require_admin(&env)?;
        let key = DataKey::Sub(plan_id.clone(), subscriber.clone());
        let mut sub: Subscription = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::SubNotFound)?;
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);

        if sub.cancelled {
            return Err(Error::Cancelled);
        }

        // Extend from end of current period, or from now if expired.
        let base = sub.current_period_end.max(env.ledger().sequence());
        sub.current_period_end = base.checked_add(CYCLE).ok_or(Error::Overflow)?;
        sub.total_renewals = sub.total_renewals.saturating_add(1);

        env.storage().persistent().set(&key, &sub);
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
        RenewedEvent {
            plan_id,
            subscriber,
            current_period_end: sub.current_period_end,
        }
        .publish(&env);
        Ok(sub)
    }

    /// Cancel the caller's own subscription.
    ///
    /// Requires authorisation from `subscriber`. Access continues until
    /// `current_period_end`; the subscription can no longer be renewed.
    /// Cancelling an already-cancelled subscription is a no-op.
    ///
    /// # Errors
    /// - [`Error::SubNotFound`] if no subscription exists for the pair.
    pub fn cancel(env: Env, plan_id: String, subscriber: Address) -> Result<(), Error> {
        subscriber.require_auth();
        let key = DataKey::Sub(plan_id.clone(), subscriber.clone());
        let mut sub: Subscription = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::SubNotFound)?;
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
        if sub.cancelled {
            return Ok(());
        }
        sub.cancelled = true;
        env.storage().persistent().set(&key, &sub);
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
        CancelledEvent {
            plan_id,
            subscriber,
        }
        .publish(&env);
        Ok(())
    }

    /// Return `true` if the subscriber currently has access.
    ///
    /// Access is active when the subscription exists and `current_period_end`
    /// is strictly greater than the current ledger sequence — including for a
    /// cancelled subscription whose paid period has not yet ended.
    /// Permissionless — safe to call from any context.
    pub fn is_active(env: Env, plan_id: String, subscriber: Address) -> bool {
        let key = DataKey::Sub(plan_id, subscriber);
        match env.storage().persistent().get::<_, Subscription>(&key) {
            Some(s) => {
                env.storage()
                    .persistent()
                    .extend_ttl(&key, BUMP_THRESH, BUMP);
                s.current_period_end > env.ledger().sequence()
            }
            None => false,
        }
    }

    /// Return the full [`Subscription`] record for a `(plan_id, subscriber)` pair.
    ///
    /// Permissionless. Returns the stored record regardless of whether it is
    /// cancelled or expired.
    ///
    /// # Errors
    /// - [`Error::SubNotFound`] if no subscription exists for the pair.
    pub fn get_subscription(
        env: Env,
        plan_id: String,
        subscriber: Address,
    ) -> Result<Subscription, Error> {
        let key = DataKey::Sub(plan_id, subscriber);
        let sub: Subscription = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::SubNotFound)?;
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
        Ok(sub)
    }

    /// Admin upgrades the contract WASM. Admin only.
    ///
    /// The stored version is intentionally *not* updated here: the new code
    /// only takes effect after this call returns, so the version must be
    /// advanced by [`SubscriptionManager::migrate`] once the new WASM is live.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.deployer()
            .update_current_contract(ContractExecutable::Wasm(new_wasm_hash.clone()));
        UpgradeEvent { new_wasm_hash }.publish(&env);
        Ok(())
    }

    /// Migrate the contract after an upgrade. Admin only.
    ///
    /// Records the currently compiled [`VERSION`] in instance storage and
    /// refreshes the instance TTL. Must be called after every
    /// [`SubscriptionManager::upgrade`]; until it is, [`SubscriptionManager::get_version`]
    /// keeps reporting the previous version and [`SubscriptionManager::is_compatible`]
    /// returns `false`.
    ///
    /// # Errors
    /// - [`Error::NotInitialised`] if `init` has not been called.
    pub fn migrate(env: Env) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Version, &String::from_str(&env, VERSION));
        env.storage().instance().extend_ttl(BUMP_THRESH, BUMP);
        Ok(())
    }
}

impl SubscriptionManager {
    fn require_admin(env: &Env) -> Result<(), Error> {
        common::require_admin(env, &DataKey::Admin).map_err(|_| Error::NotInitialised)
    }
}