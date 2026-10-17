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

/// Maximum length of a plan identifier in bytes.
pub const MAX_PLAN_ID_LEN: u32 = 64;

/// Shared persistent-storage TTL policy (see `docs/CONTRACTS.md`).
const BUMP: u32 = 365 * DAY; // 1-year TTL
const BUMP_THRESH: u32 = BUMP - DAY;

/// Contract version, sourced from Cargo.toml at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A recurring subscription plan offered by a publisher.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub plan_id: String,
    pub publisher: Address,
    pub price_per_cycle: i128,
    pub active: bool,
}

/// A subscriber's enrolment in a [`Plan`].
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Subscription {
    pub plan_id: String,
    pub subscriber: Address,
    pub started_at: u32,
    pub current_period_end: u32,
    pub cancelled: bool,
    pub total_renewals: u32,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Plan(String),
    Sub(String, Address),
    Version,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    // 1 was NotAdmin, removed as it is never returned (admin auth traps on failure)
    PlanNotFound = 2,
    PlanInactive = 3,
    AlreadySubbed = 4,
    SubNotFound = 5,
    Cancelled = 6,
    InvalidPrice = 7,
    NotInitialised = 8,
    AlreadyInitialised = 9,
    PlanExists = 10,
    Overflow = 11,
    /// `plan_id` is empty or exceeds the maximum allowed length.
    InvalidPlanId = 12,
}

#[contractevent(topics = ["init"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitEvent {
    pub admin: Address,
}

#[contractevent(topics = ["setadmin"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetAdminEvent {
    pub new_admin: Address,
}

#[contractevent(topics = ["plan"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanCreatedEvent {
    #[topic]
    pub plan_id: String,
    #[topic]
    pub publisher: Address,
    pub price_per_cycle: i128,
}

#[contractevent(topics = ["setactive"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanActiveEvent {
    #[topic]
    pub plan_id: String,
    pub active: bool,
}

#[contractevent(topics = ["subscribe"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscribedEvent {
    #[topic]
    pub plan_id: String,
    #[topic]
    pub subscriber: Address,
    pub current_period_end: u32,
    pub price: i128,
    pub payment_reference: String,
}

#[contractevent(topics = ["renew"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenewedEvent {
    #[topic]
    pub plan_id: String,
    #[topic]
    pub subscriber: Address,
    pub current_period_end: u32,
    pub price: i128,
    pub payment_reference: String,
}

#[contractevent(topics = ["cancel"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelledEvent {
    #[topic]
    pub plan_id: String,
    #[topic]
    pub subscriber: Address,
}

#[contractevent(topics = ["upgrade"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeEvent {
    pub new_wasm_hash: BytesN<32>,
}

#[contract]
pub struct SubscriptionManager;

#[contractimpl]
impl SubscriptionManager {
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

    pub fn admin(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialised)
    }

    pub fn set_admin(env: Env, new_admin: Address) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        SetAdminEvent { new_admin }.publish(&env);
        Ok(())
    }

    pub fn get_version(env: Env) -> String {
        common::get_version(&env, &DataKey::Version, VERSION)
    }

    pub fn is_compatible(env: Env) -> bool {
        common::is_compatible(&env, &DataKey::Version, VERSION)
    }

    pub fn create_plan(
        env: Env,
        publisher: Address,
        plan_id: String,
        price_per_cycle: i128,
    ) -> Result<Plan, Error> {
        publisher.require_auth();
        if plan_id.len() == 0 || plan_id.len() > MAX_PLAN_ID_LEN {
            return Err(Error::InvalidPlanId);
        }
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

    pub fn set_plan_active(env: Env, plan_id: String, active: bool) -> Result<(), Error> {
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
        plan.active = active;
        env.storage().persistent().set(&key, &plan);
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
            
        PlanActiveEvent { plan_id, active }.publish(&env);
        Ok(())
    }

    pub fn subscribe(
        env: Env,
        plan_id: String,
        subscriber: Address,
        payment_reference: String,
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
        let mut started_at = now;
        let mut total_renewals = 0;

        if let Some(s) = env.storage().persistent().get::<_, Subscription>(&key) {
            started_at = s.started_at;
            total_renewals = s.total_renewals;
            
            if s.current_period_end > now {
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
            price: plan.price_per_cycle,
            payment_reference,
        }
        .publish(&env);
        
        Ok(sub)
    }

    pub fn renew(
        env: Env, 
        plan_id: String, 
        subscriber: Address, 
        payment_reference: String
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
            price: plan.price_per_cycle,
            payment_reference,
        }
        .publish(&env);
        
        Ok(sub)
    }

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

    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.deployer()
            .update_current_contract(ContractExecutable::Wasm(new_wasm_hash.clone()));
        UpgradeEvent { new_wasm_hash }.publish(&env);
        Ok(())
    }

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
        // We catch the internal error here, but the contract entrypoint itself traps instead of returning Error::NotAdmin natively due to require_auth on the extracted wallet.
        common::require_admin(env, &DataKey::Admin).map_err(|_| Error::NotInitialised)
    }
}