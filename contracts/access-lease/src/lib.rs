#![no_std]
//! SynapsVault — Access Lease Contract (Soroban / Stellar)
//!
//! Issues time-limited on-chain access grants to vault resources.
//! Any third party can verify a buyer's access via `is_valid` without
//! trusting the SynapsVault backend — the Stellar ledger is the source of truth.

use common::{BUMP, BUMP_THRESH};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, BytesN,
    ContractExecutable, Env, String,
};

const DAY: u32 = 17_280; // ~5s/ledger × 17280 = 1 day

/// Persistent-storage TTL policy for this contract.
const PERSISTENT_TTL: u32 = 90 * DAY;
/// Threshold at which a persistent entry's TTL is refreshed on access.
const PERSISTENT_TTL_THRESHOLD: u32 = PERSISTENT_TTL - DAY;

/// Instance-storage TTL policy. Instance storage holds the admin/version
/// metadata, which must stay alive for the whole life of the contract, so it
/// shares the persistent policy here.
const BUMP: u32 = PERSISTENT_TTL;
const BUMP_THRESH: u32 = PERSISTENT_TTL_THRESHOLD;

/// Maximum accepted length (in bytes) of a `resource_id`.
pub const MAX_RESOURCE_ID_LEN: u32 = 64;

/// Contract version, sourced from Cargo.toml at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A time-limited access grant issued to a buyer for a specific resource.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    pub resource_id: String,
    pub buyer: Address,
    pub granted_at: u32,
    pub expires_at: u32,
    pub duration_ledgers: u32,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Lease(String, Address),
    Version,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    NotAdmin = 1,
    LeaseNotFound = 2,
    AlreadyActive = 3,
    InvalidDuration = 4,
    NotInitialised = 5,
    AlreadyInitialised = 7,
    LeaseExpired = 8,
    InvalidResourceId = 9,
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

#[contractevent(topics = ["grant"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantEvent {
    #[topic]
    pub resource_id: String,
    #[topic]
    pub buyer: Address,
    pub expires_at: u32,
}

#[contractevent(topics = ["extend"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtendEvent {
    #[topic]
    pub resource_id: String,
    #[topic]
    pub buyer: Address,
    pub expires_at: u32,
}

#[contractevent(topics = ["revoke"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevokeEvent {
    #[topic]
    pub resource_id: String,
    #[topic]
    pub buyer: Address,
}

#[contractevent(topics = ["upgrade"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeEvent {
    pub new_wasm_hash: BytesN<32>,
}

#[contractevent(topics = ["migrate"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrateEvent {
    pub new_version: String,
}

#[contract]
pub struct AccessLease;

#[contractimpl]
impl AccessLease {
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
        env.storage().instance().extend_ttl(BUMP_THRESH, BUMP);
        SetAdminEvent { new_admin }.publish(&env);
        Ok(())
    }

    pub fn get_version(env: Env) -> String {
        let stored: Option<String> = env.storage().instance().get(&DataKey::Version);
        common::get_version(&env, stored.as_ref())
    }

    pub fn is_compatible(env: Env) -> bool {
        let stored: Option<String> = env.storage().instance().get(&DataKey::Version);
        common::is_compatible(&env, stored.as_ref())
    }

    pub fn grant_lease(
        env: Env,
        resource_id: String,
        buyer: Address,
        duration_ledgers: u32,
    ) -> Result<Lease, Error> {
        Self::require_admin(&env)?;
        Self::grant_lease_internal(&env, resource_id, buyer, duration_ledgers)
    }

    pub fn grant_leases(
        env: Env,
        requests: soroban_sdk::Vec<(String, Address, u32)>,
    ) -> Result<soroban_sdk::Vec<Lease>, Error> {
        Self::require_admin(&env)?;
        let mut leases = soroban_sdk::Vec::new(&env);
        for req in requests.iter() {
            leases.push_back(Self::grant_lease_internal(&env, req.0, req.1, req.2)?);
        }
        Ok(leases)
    }

    pub fn extend_lease(
        env: Env,
        resource_id: String,
        buyer: Address,
        extra_ledgers: u32,
    ) -> Result<Lease, Error> {
        Self::require_admin(&env)?;
        if extra_ledgers == 0 {
            return Err(Error::InvalidDuration);
        }
        let key = DataKey::Lease(resource_id.clone(), buyer.clone());
        let mut lease: Lease = env.storage().persistent().get(&key).ok_or(Error::LeaseNotFound)?;
        
        let now = env.ledger().sequence();
        let base = lease.expires_at.max(now);
        lease.expires_at = base.checked_add(extra_ledgers).ok_or(Error::InvalidDuration)?;
        
        env.storage().persistent().set(&key, &lease);
        env.storage().persistent().extend_ttl(&key, BUMP_THRESH, BUMP);
        ExtendEvent { resource_id, buyer, expires_at: lease.expires_at }.publish(&env);
        Ok(lease)
    }

    pub fn is_valid(env: Env, resource_id: String, buyer: Address) -> bool {
        let key = DataKey::Lease(resource_id, buyer);
        match env.storage().persistent().get::<_, Lease>(&key) {
            Some(lease) => {
                env.storage()
                    .persistent()
                    .extend_ttl(&key, BUMP_THRESH, BUMP);
                lease.expires_at > env.ledger().sequence()
            }
            None => false,
        }
    }

    pub fn get_lease(env: Env, resource_id: String, buyer: Address) -> Result<Lease, Error> {
        let key = DataKey::Lease(resource_id, buyer);
        let lease = env
            .storage()
            .persistent()
            .get::<_, Lease>(&key)
            .ok_or(Error::LeaseNotFound)?;
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
        Ok(lease)
    }

    pub fn revoke_lease(env: Env, resource_id: String, buyer: Address) -> Result<(), Error> {
        Self::require_admin(&env)?;
        let key = DataKey::Lease(resource_id.clone(), buyer.clone());
        if !env.storage().persistent().has(&key) {
            return Err(Error::LeaseNotFound);
        }
        env.storage().persistent().remove(&key);
        RevokeEvent { resource_id, buyer }.publish(&env);
        Ok(())
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
        let new_version = String::from_str(&env, VERSION);
        env.storage()
            .instance()
            .set(&DataKey::Version, &new_version);
        env.storage().instance().extend_ttl(BUMP_THRESH, BUMP);
        MigrateEvent { new_version }.publish(&env);
        Ok(())
    }
}

impl AccessLease {
    fn require_admin(env: &Env) -> Result<(), Error> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialised)?;
        common::require_admin(env, &admin);
        env.storage().instance().extend_ttl(BUMP_THRESH, BUMP);
        Ok(())
    }

    fn grant_lease_internal(
        env: &Env,
        resource_id: String,
        buyer: Address,
        duration_ledgers: u32,
    ) -> Result<Lease, Error> {
        if resource_id.len() == 0 || resource_id.len() > MAX_RESOURCE_ID_LEN {
            return Err(Error::InvalidResourceId);
        }
        if duration_ledgers == 0 {
            return Err(Error::InvalidDuration);
        }
        let key = DataKey::Lease(resource_id.clone(), buyer.clone());
        let now = env.ledger().sequence();
        
        if let Some(existing) = env.storage().persistent().get::<_, Lease>(&key) {
            if existing.expires_at > now {
                return Err(Error::AlreadyActive);
            }
        }
        
        let expires_at = now.checked_add(duration_ledgers).ok_or(Error::InvalidDuration)?;
        let lease = Lease {
            resource_id: resource_id.clone(),
            buyer: buyer.clone(),
            granted_at: now,
            expires_at,
            duration_ledgers,
        };
        
        env.storage().persistent().set(&key, &lease);
        env.storage().persistent().extend_ttl(&key, BUMP_THRESH, BUMP);
        GrantEvent { resource_id, buyer, expires_at }.publish(env);
        Ok(lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        symbol_short,
        testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
        vec, Address, BytesN, Env, IntoVal, String,
    };

    fn setup<'a>() -> (Env, AccessLeaseClient<'a>, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let id = env.register(AccessLease, ());
        let client = AccessLeaseClient::new(&env, &id);
        let admin = Address::generate(&env);
        client.init(&admin);
        (env, client, admin)
    }

    fn advance(env: &Env, ledgers: u32) {
        env.ledger()
            .set_sequence_number(env.ledger().sequence() + ledgers);
    }

    #[test]
    fn test_grant_and_check_lease() {
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let resource_id = String::from_str(&env, "res_001");

        assert!(!client.is_valid(&resource_id, &buyer));

        let lease = client.grant_lease(&resource_id, &buyer, &1000u32);
        assert_eq!(lease.duration_ledgers, 1000u32);
        assert_eq!(lease.expires_at, lease.granted_at + 1000);
        assert!(client.is_valid(&resource_id, &buyer));
        assert_eq!(client.get_lease(&resource_id, &buyer), lease);
    }

    #[test]
    fn test_grant_leases_batch() {
        let (env, client, _admin) = setup();
        let b1 = Address::generate(&env);
        let b2 = Address::generate(&env);
        let rid1 = String::from_str(&env, "r1");
        let rid2 = String::from_str(&env, "r2");

        let mut requests = soroban_sdk::Vec::new(&env);
        requests.push_back((rid1.clone(), b1.clone(), 100u32));
        requests.push_back((rid2.clone(), b2.clone(), 200u32));

        let leases = client.grant_leases(&requests);
        assert_eq!(leases.len(), 2);
        assert!(client.is_valid(&rid1, &b1));
        assert!(client.is_valid(&rid2, &b2));
        assert_eq!(leases.get_unchecked(0).duration_ledgers, 100);
        assert_eq!(leases.get_unchecked(1).duration_ledgers, 200);
    }

    #[test]
    fn test_revoke_lease() {
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let resource_id = String::from_str(&env, "res_001");

        client.grant_lease(&resource_id, &buyer, &500u32);
        assert!(client.is_valid(&resource_id, &buyer));

        client.revoke_lease(&resource_id, &buyer);
        assert!(!client.is_valid(&resource_id, &buyer));
        assert_eq!(
            client.try_get_lease(&resource_id, &buyer),
            Err(Ok(Error::LeaseNotFound))
        );
    }

    #[test]
    fn init_twice_is_rejected() {
        let (env, client, admin) = setup();
        let attacker = Address::generate(&env);
        assert_eq!(
            client.try_init(&attacker),
            Err(Ok(Error::AlreadyInitialised))
        );
        assert_eq!(client.admin(), admin);
    }

    #[test]
    fn admin_calls_fail_before_init() {
        let env = Env::default();
        env.mock_all_auths();
        let client = AccessLeaseClient::new(&env, &env.register(AccessLease, ()));
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");
        assert_eq!(client.try_admin(), Err(Ok(Error::NotInitialised)));
        assert_eq!(
            client.try_grant_lease(&rid, &buyer, &10u32),
            Err(Ok(Error::NotInitialised))
        );
    }

    #[test]
    fn grant_requires_admin_auth() {
        let (env, client, admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");
        client.grant_lease(&rid, &buyer, &10u32);
        assert_eq!(env.auths()[0].0, admin);
    }

    #[test]
    fn grant_by_non_admin_is_rejected() {
        let (env, client, _admin) = setup();
        let mallory = Address::generate(&env);
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");
        env.mock_auths(&[MockAuth {
            address: &mallory,
            invoke: &MockAuthInvoke {
                contract: &client.address,
                fn_name: "grant_lease",
                args: (rid.clone(), buyer.clone(), 10u32).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert!(client.try_grant_lease(&rid, &buyer, &10u32).is_err());
    }
    
    #[test]
    fn set_admin_by_non_admin_is_rejected() {
        let (env, client, _admin) = setup();
        let mallory = Address::generate(&env);
        let new_admin = Address::generate(&env);
        env.mock_auths(&[MockAuth {
            address: &mallory,
            invoke: &MockAuthInvoke {
                contract: &client.address,
                fn_name: "set_admin",
                args: (new_admin.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert!(client.try_set_admin(&new_admin).is_err());
    }

    #[test]
    fn upgrade_success() {
        let (env, client, _admin) = setup();
        let hash = BytesN::from_array(&env, &[1; 32]);
        client.upgrade(&hash);
        // Auth checks passed successfully
    }

    #[test]
    fn upgrade_by_non_admin_is_rejected() {
        let (env, client, _admin) = setup();
        let mallory = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1; 32]);
        env.mock_auths(&[MockAuth {
            address: &mallory,
            invoke: &MockAuthInvoke {
                contract: &client.address,
                fn_name: "upgrade",
                args: (hash.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert!(client.try_upgrade(&hash).is_err());
    }

    #[test]
    fn upgrade_fails_before_init() {
        let env = Env::default();
        env.mock_all_auths();
        let client = AccessLeaseClient::new(&env, &env.register(AccessLease, ()));
        let hash = BytesN::from_array(&env, &[1; 32]);
        assert_eq!(client.try_upgrade(&hash), Err(Ok(Error::NotInitialised)));
    }

    #[test]
    fn zero_duration_rejected() {
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");
        assert_eq!(
            client.try_grant_lease(&rid, &buyer, &0u32),
            Err(Ok(Error::InvalidDuration))
        );
        client.grant_lease(&rid, &buyer, &10u32);
        assert_eq!(
            client.try_extend_lease(&rid, &buyer, &0u32),
            Err(Ok(Error::InvalidDuration))
        );
    }

    #[test]
    fn overflowing_duration_rejected() {
        let (env, client, _admin) = setup();
        advance(&env, 10);
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");
        assert_eq!(
            client.try_grant_lease(&rid, &buyer, &u32::MAX),
            Err(Ok(Error::InvalidDuration))
        );
        client.grant_lease(&rid, &buyer, &10u32);
        assert_eq!(
            client.try_extend_lease(&rid, &buyer, &u32::MAX),
            Err(Ok(Error::InvalidDuration))
        );
    }

    #[test]
    fn lease_expires_exactly_at_expiry() {
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");
        client.grant_lease(&rid, &buyer, &100u32);
        advance(&env, 99);
        assert!(client.is_valid(&rid, &buyer));
        advance(&env, 1);
        assert!(!client.is_valid(&rid, &buyer));
        // Record is still readable after expiry.
        assert_eq!(client.get_lease(&rid, &buyer).duration_ledgers, 100);
    }

    #[test]
    fn active_lease_cannot_be_regranted_but_expired_can() {
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");
        client.grant_lease(&rid, &buyer, &100u32);
        assert_eq!(
            client.try_grant_lease(&rid, &buyer, &100u32),
            Err(Ok(Error::AlreadyActive))
        );
        advance(&env, 100);
        let lease = client.grant_lease(&rid, &buyer, &50u32);
        assert_eq!(lease.granted_at, env.ledger().sequence());
        assert!(client.is_valid(&rid, &buyer));
    }

    #[test]
    fn extend_active_lease_stacks_on_expiry() {
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");
        let lease = client.grant_lease(&rid, &buyer, &100u32);
        advance(&env, 40);
        let extended = client.extend_lease(&rid, &buyer, &50u32);
        assert_eq!(extended.expires_at, lease.expires_at + 50);
    }

    #[test]
    fn extend_expired_lease_restarts_from_now() {
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");
        client.grant_lease(&rid, &buyer, &10u32);
        advance(&env, 500);
        let extended = client.extend_lease(&rid, &buyer, &50u32);
        assert_eq!(extended.expires_at, env.ledger().sequence() + 50);
        assert!(client.is_valid(&rid, &buyer));
    }

    #[test]
    fn extend_and_revoke_missing_lease_fail() {
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "missing");
        assert_eq!(
            client.try_extend_lease(&rid, &buyer, &10u32),
            Err(Ok(Error::LeaseNotFound))
        );
        assert_eq!(
            client.try_revoke_lease(&rid, &buyer),
            Err(Ok(Error::LeaseNotFound))
        );
    }

    #[test]
    fn leases_are_scoped_per_resource_and_buyer() {
        let (env, client, _admin) = setup();
        let alice = Address::generate(&env);
        let bob = Address::generate(&env);
        let r1 = String::from_str(&env, "r1");
        let r2 = String::from_str(&env, "r2");
        client.grant_lease(&r1, &alice, &10u32);
        assert!(client.is_valid(&r1, &alice));
        assert!(!client.is_valid(&r2, &alice));
        assert!(!client.is_valid(&r1, &bob));
    }

    #[test]
    fn set_admin_rotates_admin() {
        let (env, client, admin) = setup();
        let new_admin = Address::generate(&env);
        client.set_admin(&new_admin);
        assert_eq!(env.auths()[0].0, admin);
        assert_eq!(client.admin(), new_admin);
    }

    /// Event topics/data must stay wire-compatible with what indexers expect
    /// (see docs/CONTRACTS.md).
    #[test]
    fn events_have_documented_shape() {
        let (env, client, admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");
        let new_admin = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[1; 32]);

        let lease = client.grant_lease(&rid, &buyer, &10u32);
        let extended = client.extend_lease(&rid, &buyer, &5u32);
        client.revoke_lease(&rid, &buyer);
        client.set_admin(&new_admin);
        client.upgrade(&hash);

        let events = env.events().all();
        let mut idx = events.len() - 6;

        // 1. init (from setup)
        assert_eq!(
            events.get_unchecked(idx),
            (
                client.address.clone(),
                (symbol_short!("init"),).into_val(&env),
                admin.into_val(&env),
            )
        );
        idx += 1;

        // 2. grant
        assert_eq!(
            events.get_unchecked(idx),
            (
                client.address.clone(),
                (symbol_short!("grant"), rid.clone(), buyer.clone()).into_val(&env),
                lease.expires_at.into_val(&env),
            )
        );
        idx += 1;

        // 3. extend
        assert_eq!(
            events.get_unchecked(idx),
            (
                client.address.clone(),
                (symbol_short!("extend"), rid.clone(), buyer.clone()).into_val(&env),
                extended.expires_at.into_val(&env),
            )
        );
        idx += 1;

        // 4. revoke
        assert_eq!(
            events.get_unchecked(idx),
            (
                client.address.clone(),
                (symbol_short!("revoke"), rid.clone(), buyer.clone()).into_val(&env),
                ().into_val(&env),
            )
        );
        idx += 1;
        
        // 5. setadmin
        assert_eq!(
            events.get_unchecked(idx),
            (
                client.address.clone(),
                (symbol_short!("setadmin"),).into_val(&env),
                new_admin.into_val(&env),
            )
        );
        idx += 1;
        
        // 6. upgrade
        assert_eq!(
            events.get_unchecked(idx),
            (
                client.address.clone(),
                (symbol_short!("upgrade"),).into_val(&env),
                hash.into_val(&env),
            )
        );
    }

    #[test]
    fn is_valid_false_exactly_at_expires_at_after_extend_on_expired_lease() {
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");

        // Grant and let it expire.
        client.grant_lease(&rid, &buyer, &10u32);
        advance(&env, 500);
        assert!(!client.is_valid(&rid, &buyer));

        // Extend the expired lease by 50 ledgers — base is now (500), so expires_at = 550.
        let extended = client.extend_lease(&rid, &buyer, &50u32);
        let expected_expiry = env.ledger().sequence() + 50;
        assert_eq!(extended.expires_at, expected_expiry);
        assert!(client.is_valid(&rid, &buyer));

        // One ledger before expiry: still valid.
        advance(&env, 49);
        assert!(client.is_valid(&rid, &buyer));

        // Exactly at expires_at: no longer valid.
        advance(&env, 1);
        assert_eq!(env.ledger().sequence(), expected_expiry);
        assert!(!client.is_valid(&rid, &buyer));
    }

    #[test]
    fn grant_lease_boundary_at_u32_max() {
        let (env, client, _admin) = setup();
        // Position ledger so that now + duration == u32::MAX exactly.
        let now = 100u32;
        env.ledger().set_sequence_number(now);
        let duration = u32::MAX - now;
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");

        let lease = client.grant_lease(&rid, &buyer, &duration);
        assert_eq!(lease.expires_at, u32::MAX);
        assert_eq!(lease.granted_at, now);
        assert_eq!(lease.duration_ledgers, duration);
        assert!(client.is_valid(&rid, &buyer));
    }

    #[test]
    fn revoke_then_regrant_same_ledger() {
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");

        client.grant_lease(&rid, &buyer, &100u32);
        assert!(client.is_valid(&rid, &buyer));

        // Revoke and immediately re-grant without advancing the ledger.
        client.revoke_lease(&rid, &buyer);
        assert!(!client.is_valid(&rid, &buyer));

        let lease = client.grant_lease(&rid, &buyer, &50u32);
        assert_eq!(lease.granted_at, env.ledger().sequence());
        assert_eq!(lease.expires_at, env.ledger().sequence() + 50);
        assert!(client.is_valid(&rid, &buyer));
    }

    #[test]
    fn version_check() {
        let (env, client, _admin) = setup();
        assert_eq!(client.get_version(), String::from_str(&env, VERSION));
        assert!(client.is_compatible());

        let fresh = AccessLeaseClient::new(&env, &env.register(AccessLease, ()));
        assert!(!fresh.is_compatible());
    }

    #[test]
    fn migrate_updates_stored_version() {
        let (env, client, admin) = setup();

        // Simulate a stale deployment: stored version differs from compiled.
        env.as_contract(&client.address, || {
            env.storage()
                .instance()
                .set(&DataKey::Version, &String::from_str(&env, "0.0.0"));
        });
        assert_eq!(client.get_version(), String::from_str(&env, "0.0.0"));
        assert!(!client.is_compatible());

        client.migrate();
        assert_eq!(env.auths()[0].0, admin);
        assert_eq!(client.get_version(), String::from_str(&env, VERSION));
        assert!(client.is_compatible());
    }

    #[test]
    fn migrate_requires_admin_auth() {
        let (env, client, _admin) = setup();
        let mallory = Address::generate(&env);
        env.mock_auths(&[MockAuth {
            address: &mallory,
            invoke: &MockAuthInvoke {
                contract: &client.address,
                fn_name: "migrate",
                args: ().into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert!(client.try_migrate().is_err());
    }

    #[test]
    fn migrate_fails_before_init() {
        let env = Env::default();
        env.mock_all_auths();
        let client = AccessLeaseClient::new(&env, &env.register(AccessLease, ()));
        assert_eq!(client.try_migrate(), Err(Ok(Error::NotInitialised)));
    }
}