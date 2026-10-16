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

/// Maximum accepted length (in bytes) of a `resource_id`.
///
/// Bounds the size of the persistent-storage key and the emitted event topics
/// so a caller cannot store or emit an unbounded identifier.
pub const MAX_RESOURCE_ID_LEN: u32 = 64;

/// Contract version, sourced from Cargo.toml at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A time-limited access grant issued to a buyer for a specific resource.
///
/// A lease is considered **active** (valid) while
/// `expires_at > env.ledger().sequence()`. Once the current ledger sequence
/// reaches or passes `expires_at`, the lease is expired and `is_valid`
/// returns `false`, even though the record may still be stored on-chain.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    /// Opaque identifier of the vault resource the lease grants access to.
    pub resource_id: String,
    /// Wallet address entitled to access the resource while the lease is active.
    pub buyer: Address,
    /// Ledger sequence at which the lease was granted.
    pub granted_at: u32,
    /// Ledger sequence at which the lease stops being valid (exclusive).
    pub expires_at: u32,
    /// Length of the lease in ledgers, as originally requested.
    pub duration_ledgers: u32,
}

/// Storage keys used by the contract.
#[contracttype]
pub enum DataKey {
    /// Instance-storage key holding the admin [`Address`] set by
    /// [`AccessLease::init`]. Its presence is also the "initialised" flag.
    Admin,
    /// Persistent-storage key holding the [`Lease`] for a
    /// `(resource_id, buyer)` pair.
    Lease(String, Address),
    /// Instance-storage key holding the version recorded at `init`.
    Version,
}

/// Errors returned by the contract's fallible entry points.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    /// The caller is not the configured admin (or failed auth).
    NotAdmin = 1,
    /// No lease exists for the given `(resource_id, buyer)` pair.
    LeaseNotFound = 2,
    /// An unexpired lease already exists for the given pair.
    AlreadyActive = 3,
    /// The requested duration was zero or would overflow the ledger sequence.
    InvalidDuration = 4,
    /// `init` has not been called, so no admin is configured.
    NotInitialised = 5,
    /// `init` has already been called; the admin cannot be overwritten.
    AlreadyInitialised = 7,
    /// The lease has already expired; there is nothing to revoke.
    LeaseExpired = 8,
    /// `resource_id` is empty or exceeds [`MAX_RESOURCE_ID_LEN`] bytes.
    InvalidResourceId = 9,
}

/// Emitted by [`AccessLease::init`]. Topics: `("init",)`; data: admin.
#[contractevent(topics = ["init"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitEvent {
    pub admin: Address,
}

/// Emitted by [`AccessLease::set_admin`]. Topics: `("setadmin",)`; data: new admin.
#[contractevent(topics = ["setadmin"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetAdminEvent {
    pub new_admin: Address,
}

/// Emitted by [`AccessLease::grant_lease`]. Topics: `("grant", resource_id, buyer)`;
/// data: `expires_at`.
#[contractevent(topics = ["grant"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantEvent {
    #[topic]
    pub resource_id: String,
    #[topic]
    pub buyer: Address,
    pub expires_at: u32,
}

/// Emitted by [`AccessLease::extend_lease`]. Topics: `("extend", resource_id, buyer)`;
/// data: new `expires_at`.
#[contractevent(topics = ["extend"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtendEvent {
    #[topic]
    pub resource_id: String,
    #[topic]
    pub buyer: Address,
    pub expires_at: u32,
}

/// Emitted by [`AccessLease::revoke_lease`]. Topics: `("revoke", resource_id, buyer)`;
/// no data.
#[contractevent(topics = ["revoke"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevokeEvent {
    #[topic]
    pub resource_id: String,
    #[topic]
    pub buyer: Address,
}

/// Emitted by [`AccessLease::upgrade`]. Topics: `("upgrade",)`; data: new WASM hash.
#[contractevent(topics = ["upgrade"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeEvent {
    pub new_wasm_hash: BytesN<32>,
}

#[contract]
pub struct AccessLease;

#[contractimpl]
impl AccessLease {
    /// Initialise the contract — set the admin wallet (backend platform wallet).
    ///
    /// Must be called once immediately after deployment. Stores `admin` under
    /// [`DataKey::Admin`] in instance storage and bumps the instance TTL.
    ///
    /// # Parameters
    /// * `admin` — the platform wallet authorised to perform admin-only
    ///   mutations ([`grant_lease`](AccessLease::grant_lease),
    ///   [`extend_lease`](AccessLease::extend_lease),
    ///   [`revoke_lease`](AccessLease::revoke_lease)).
    ///
    /// # Errors
    /// * [`Error::AlreadyInitialised`] — an admin is already set. Use
    ///   [`set_admin`](AccessLease::set_admin) to rotate it.
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
    /// * [`Error::NotInitialised`] — `init` has not been called.
    pub fn admin(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialised)
    }

    /// Rotate the admin to `new_admin`. Requires the current admin's auth.
    ///
    /// # Errors
    /// * [`Error::NotInitialised`] — `init` has not been called.
    pub fn set_admin(env: Env, new_admin: Address) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().extend_ttl(BUMP_THRESH, BUMP);
        SetAdminEvent { new_admin }.publish(&env);
        Ok(())
    }

    /// Return the version recorded at `init`, falling back to the compiled
    /// version. Permissionless.
    pub fn get_version(env: Env) -> String {
        let stored: Option<String> = env.storage().instance().get(&DataKey::Version);
        common::get_version(&env, stored.as_ref())
    }

    /// Returns `true` if the stored version matches the compiled-in version.
    /// Used to detect stale deployments that need re-initialisation.
    pub fn is_compatible(env: Env) -> bool {
        let stored: Option<String> = env.storage().instance().get(&DataKey::Version);
        common::is_compatible(&env, stored.as_ref())
    }

    /// Grant a time-limited lease to `buyer` for `resource_id`.
    ///
    /// Admin-only: the configured admin must have authorised the call.
    ///
    /// # Parameters
    /// * `resource_id` — identifier of the resource being leased. Must be

/* … truncated 609 chars — edit only what you need near the top … */
        env.storage().persistent().set(&key, &lease);
        env.storage()
            .persistent()
            .extend_ttl(&key, BUMP_THRESH, BUMP);
        ExtendEvent {
            resource_id,
            buyer,
            expires_at: lease.expires_at,
        }
        .publish(&env);
        Ok(lease)
    }

    /// Returns `true` if `buyer` currently holds a valid lease for `resource_id`.
    ///
    /// Permissionless — any caller can verify without trusting the backend.
    /// A lease is valid iff it exists and `expires_at > env.ledger().sequence()`.
    pub fn is_valid(env: Env, resource_id: String, buyer: Address) -> bool {
        let key = DataKey::Lease(resource_id, buyer);
        match env.storage().persistent().get::<_, Lease>(&key) {
            Some(lease) => lease.expires_at > env.ledger().sequence(),
            None => false,
        }
    }

    /// Return the full [`Lease`] struct for the given pair.
    ///
    /// Permissionless. Unlike [`is_valid`](AccessLease::is_valid), this returns the
    /// record even if the lease has expired.
    ///
    /// # Errors
    /// * [`Error::LeaseNotFound`] — no lease exists for the pair.
    pub fn get_lease(env: Env, resource_id: String, buyer: Address) -> Result<Lease, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Lease(resource_id, buyer))
            .ok_or(Error::LeaseNotFound)
    }

    /// Revoke a lease immediately.
    ///
    /// Admin-only. Used on refund or Terms-of-Service violation. Removes the
    /// lease record from persistent storage, so subsequent
    /// [`is_valid`](AccessLease::is_valid) calls return `false`.
    ///
    /// # Errors
    /// * [`Error::NotInitialised`] — admin check failed.
    /// * [`Error::LeaseNotFound`] — no lease exists for the pair.
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

    /// Upgrade the contract WASM to `new_wasm_hash` (admin only).
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.deployer()
            .update_current_contract(ContractExecutable::Wasm(new_wasm_hash.clone()));
        UpgradeEvent { new_wasm_hash }.publish(&env);
        Ok(())
    }
}

impl AccessLease {
    /// Load the configured admin from instance storage and require its auth.
    ///
    /// # Errors
    /// * [`Error::NotInitialised`] — no admin has been set via `init`.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        symbol_short,
        testutils::{Address as _, Events as _, Ledger as _, MockAuth, MockAuthInvoke},
        vec, Address, Env, IntoVal, String,
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
        let (env, client, _admin) = setup();
        let buyer = Address::generate(&env);
        let rid = String::from_str(&env, "r");

        let lease = client.grant_lease(&rid, &buyer, &10u32);
        assert_eq!(
            env.events().all().filter_by_contract(&client.address),
            vec![
                &env,
                (
                    client.address.clone(),
                    (symbol_short!("grant"), rid.clone(), buyer.clone()).into_val(&env),
                    lease.expires_at.into_val(&env),
                ),
            ]
        );

        client.revoke_lease(&rid, &buyer);
        assert_eq!(
            env.events().all().filter_by_contract(&client.address),
            vec![
                &env,
                (
                    client.address.clone(),
                    (symbol_short!("revoke"), rid.clone(), buyer.clone()).into_val(&env),
                    ().into_val(&env),
                ),
            ]
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
}
