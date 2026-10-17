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
    /// cancelled subscription is carried over.
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
        env.storage()
            .persistent()
            .extend_ttl(&plan_key, BUMP_THRESH, BUMP);

        if !plan.active {
            return Err(Error::PlanInactive);
        }

        let key = DataKey::Sub(plan_id.clone(), subscriber.clone());
        let now = env.ledger().sequence();
        let mut base = now;

        if let Some(s) = env.storage().persistent().get::<_, Subscription>(&key) {
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
            started_at: now,
            current_period_end: base.checked_add(CYCLE).ok_or(Error::Overflow)?,
            cancelled: false,
            total_renewals: 0,
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
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.deployer()
            .update_current_contract(ContractExecutable::Wasm(new_wasm_hash.clone()));
        UpgradeEvent { new_wasm_hash }.publish(&env);
        Ok(())
    }
}

impl SubscriptionManager {
    fn require_admin(env: &Env) -> Result<(), Error> {
        common::require_admin(env, &DataKey::Admin).map_err(|_| Error::NotInitialised)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        symbol_short,
        testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
        vec, Address, Env, IntoVal, String,
    };

    struct Ctx<'a> {
        env: Env,
        client: SubscriptionManagerClient<'a>,
        admin: Address,
        publisher: Address,
        plan_id: String,
    }

    fn setup<'a>() -> Ctx<'a> {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(SubscriptionManager, ());
        let client = SubscriptionManagerClient::new(&env, &id);
        let admin = Address::generate(&env);
        let publisher = Address::generate(&env);
        let plan_id = String::from_str(&env, "plan");
        client.init(&admin);
        client.create_plan(&publisher, &plan_id, &1_000_000i128);
        Ctx {
            env,
            client,
            admin,
            publisher,
            plan_id,
        }
    }

    fn advance(env: &Env, ledgers: u32) {
        env.ledger()
            .set_sequence_number(env.ledger().sequence() + ledgers);
    }

    #[test]
    fn test_create_plan() {
        let env = Env::default();
        env.mock_all_auths();

        let id = env.register(SubscriptionManager, ());
        let client = SubscriptionManagerClient::new(&env, &id);

        let admin = Address::generate(&env);
        let publisher = Address::generate(&env);
        let plan_id = String::from_str(&env, "plan_001");

        client.init(&admin);

        let plan = client.create_plan(&publisher, &plan_id, &10_000_000i128);
        assert_eq!(plan.price_per_cycle, 10_000_000i128);
        assert!(plan.active);
        assert_eq!(client.get_plan(&plan_id), plan);
    }

    #[test]
    fn test_subscribe_and_active() {
        let env = Env::default();
        env.mock_all_auths();

        let id = env.register(SubscriptionManager, ());
        let client = SubscriptionManagerClient::new(&env, &id);

        let admin = Address::generate(&env);
        let publisher = Address::generate(&env);
        let subscriber = Address::generate(&env);
        let plan_id = String::from_str(&env, "plan_002");

        client.init(&admin);
        client.create_plan(&publisher, &plan_id, &1_000_000i128);

        assert!(!client.is_active(&plan_id, &subscriber));
        client.subscribe(&plan_id, &subscriber);
        assert!(client.is_active(&plan_id, &subscriber));
    }

    #[test]
    fn init_twice_is_rejected() {
        let c = setup();
        let attacker = Address::generate(&c.env);
        assert_eq!(
            c.client.try_init(&attacker),
            Err(Ok(Error::AlreadyInitialised))
        );
        assert_eq!(c.client.admin(), c.admin);
    }

    #[test]
    fn subscribe_fails_before_init() {
        let env = Env::default();
        env.mock_all_auths();
        let client = SubscriptionManagerClient::new(&env, &env.register(SubscriptionManager, ()));
        let sub = Address::generate(&env);
        let pid = String::from_str(&env, "p");
        assert_eq!(client.try_admin(), Err(Ok(Error::NotInitialised)));
        assert_eq!(
            client.try_subscribe(&pid, &sub),
            Err(Ok(Error::NotInitialised))
        );
    }

    #[test]
    fn existing_plan_cannot_be_hijacked() {
        let c = setup();
        let attacker = Address::generate(&c.env);
        assert_eq!(
            c.client.try_create_plan(&attacker, &c.plan_id, &1i128),
            Err(Ok(Error::PlanExists))
        );
        assert_eq!(c.client.get_plan(&c.plan_id).publisher, c.publisher);
    }

    #[test]
    fn non_positive_price_rejected() {
        let c = setup();
        let pid = String::from_str(&c.env, "free");
        assert_eq!(
            c.client.try_create_plan(&c.publisher, &pid, &0i128),
            Err(Ok(Error::InvalidPrice))
        );
        assert_eq!(
            c.client.try_create_plan(&c.publisher, &pid, &-1i128),
            Err(Ok(Error::InvalidPrice))
        );
    }

    #[test]
    fn subscribe_by_non_admin_is_rejected() {
        let c = setup();
        let mallory = Address::generate(&c.env);
        let sub = Address::generate(&c.env);
        c.env.mock_auths(&[MockAuth {
            address: &mallory,
            invoke: &MockAuthInvoke {
                contract: &c.client.address,
                fn_name: "subscribe",
                args: (c.plan_id.clone(), sub.clone()).into_val(&c.env),
                sub_invokes: &[],
            },
        }]);
        assert!(c.client.try_subscribe(&c.plan_id, &sub).is_err());
    }

    #[test]
    fn subscribe_missing_or_inactive_plan_fails() {
        let c = setup();
        let sub = Address::generate(&c.env);
        let missing = String::from_str(&c.env, "missing");
        assert_eq!(
            c.client.try_subscribe(&missing, &sub),
            Err(Ok(Error::PlanNotFound))
        );
        c.client.deactivate_plan(&c.plan_id);
        assert!(!c.client.get_plan(&c.plan_id).active);
        assert_eq!(
            c.client.try_subscribe(&c.plan_id, &sub),
            Err(Ok(Error::PlanInactive))
        );
    }

    #[test]
    fn deactivate_requires_publisher_auth() {
        let c = setup();
        c.client.deactivate_plan(&c.plan_id);
        assert_eq!(c.env.auths()[0].0, c.publisher);
    }

    #[test]
    fn double_subscribe_rejected_until_expiry() {
        let c = setup();
        let sub = Address::generate(&c.env);
        c.client.subscribe(&c.plan_id, &sub);
        assert_eq!(
            c.client.try_subscribe(&c.plan_id, &sub),
            Err(Ok(Error::AlreadySubbed))
        );
        advance(&c.env, CYCLE);
        assert!(!c.client.is_active(&c.plan_id, &sub));
        let s = c.client.subscribe(&c.plan_id, &sub);
        assert_eq!(s.current_period_end, c.env.ledger().sequence() + CYCLE);
    }

    #[test]
    fn renew_stacks_from_period_end_or_now() {
        let c = setup();
        let sub = Address::generate(&c.env);
        let s = c.client.subscribe(&c.plan_id, &sub);

        advance(&c.env, 10);
        let r = c.client.renew(&c.plan_id, &sub);
        assert_eq!(r.current_period_end, s.current_period_end + CYCLE);
        assert_eq!(r.total_renewals, 1);

        advance(&c.env, 5 * CYCLE);
        let r2 = c.client.renew(&c.plan_id, &sub);
        assert_eq!(r2.current_period_end, c.env.ledger().sequence() + CYCLE);
        assert_eq!(r2.total_renewals, 2);
    }

    #[test]
    fn renew_works_on_deactivated_plan() {
        let c = setup();
        let sub = Address::generate(&c.env);
        c.client.subscribe(&c.plan_id, &sub);
        c.client.deactivate_plan(&c.plan_id);
        assert_eq!(c.client.renew(&c.plan_id, &sub).total_renewals, 1);
    }

    #[test]
    fn cancel_keeps_access_until_period_end_and_blocks_renewal() {
        let c = setup();
        let sub = Address::generate(&c.env);
        let s = c.client.subscribe(&c.plan_id, &sub);

        c.client.cancel(&c.plan_id, &sub);
        assert_eq!(c.env.auths()[0].0, sub);
        assert!(c.client.get_subscription(&c.plan_id, &sub).cancelled);
        assert!(c.client.is_active(&c.plan_id, &sub));
        assert_eq!(
            c.client.try_renew(&c.plan_id, &sub),
            Err(Ok(Error::Cancelled))
        );

        // Idempotent.
        c.client.cancel(&c.plan_id, &sub);

        c.env.ledger().set_sequence_number(s.current_period_end - 1);
        assert!(c.client.is_active(&c.plan_id, &sub));
        advance(&c.env, 1);
        assert!(!c.client.is_active(&c.plan_id, &sub));
    }

    #[test]
    fn resubscribe_after_cancel_carries_over_paid_time() {
        let c = setup();
        let sub = Address::generate(&c.env);
        let s = c.client.subscribe(&c.plan_id, &sub);
        c.client.cancel(&c.plan_id, &sub);
        advance(&c.env, 100);
        let s2 = c.client.subscribe(&c.plan_id, &sub);
        assert!(!s2.cancelled);
        assert_eq!(s2.current_period_end, s.current_period_end + CYCLE);
    }

    #[test]
    fn missing_subscription_errors() {
        let c = setup();
        let sub = Address::generate(&c.env);
        assert_eq!(
            c.client.try_renew(&c.plan_id, &sub),
            Err(Ok(Error::SubNotFound))
        );
        assert_eq!(
            c.client.try_cancel(&c.plan_id, &sub),
            Err(Ok(Error::SubNotFound))
        );
        assert_eq!(
            c.client.try_get_subscription(&c.plan_id, &sub),
            Err(Ok(Error::SubNotFound))
        );
    }

    #[test]
    fn set_admin_rotates_admin() {
        let c = setup();
        let new_admin = Address::generate(&c.env);
        c.client.set_admin(&new_admin);
        assert_eq!(c.env.auths()[0].0, c.admin);
        assert_eq!(c.client.admin(), new_admin);
    }

    /// Event topics/data must stay wire-compatible with what indexers expect
    /// (see docs/CONTRACTS.md).
    #[test]
    fn events_have_documented_shape() {
        let c = setup();
        let sub = Address::generate(&c.env);

        let s = c.client.subscribe(&c.plan_id, &sub);
        assert_eq!(
            c.env.events().all().filter_by_contract(&c.client.address),
            vec![
                &c.env,
                (
                    c.client.address.clone(),
                    (symbol_short!("subscribe"), c.plan_id.clone(), sub.clone()).into_val(&c.env),
                    s.current_period_end.into_val(&c.env),
                ),
            ]
        );

        c.client.deactivate_plan(&c.plan_id);
        assert_eq!(
            c.env.events().all().filter_by_contract(&c.client.address),
            vec![
                &c.env,
                (
                    c.client.address.clone(),
                    (symbol_short!("deactiv"), c.plan_id.clone()).into_val(&c.env),
                    ().into_val(&c.env),
                ),
            ]
        );
    }

    #[test]
    fn version_check() {
        let c = setup();
        assert_eq!(c.client.get_version(), String::from_str(&c.env, VERSION));
        assert!(c.client.is_compatible());
    }
}
