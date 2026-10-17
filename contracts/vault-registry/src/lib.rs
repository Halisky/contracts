#![no_std]
//! MindVault on-chain vault registry.
//!
//! Records each vault resource on Stellar: its creator, price (in USDC
//! stroops, 7 decimals), and a metadata pointer (e.g. an IPFS URI or content
//! hash). Payment itself still flows through x402 + the USDC SAC off this
//! contract — this registry is the transparent, on-chain source of truth for
//! *what* exists, *who* owns it, and *what it costs*.
//!
//! Only the recorded creator can mutate a resource (enforced via
//! `require_auth`). Ownership can be transferred.

use common::{BUMP, BUMP_THRESH};
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, BytesN,
    ContractExecutable, Env, IntoVal, String, Val, Vec,
};

// ~5s ledgers → 17,280 per day.
//
// Shared persistent-TTL policy (see `docs/TTL_POLICY.md` for the cross-contract
// rationale). The three MindVault contracts deliberately use different
// retention windows because their data has different value-over-time:
//
// - `vault-registry` (this contract): 30 days. Registry entries are cheap to
//   re-create and are actively managed by their creators, so a short window
//   keeps rent low while still protecting live listings.
// - `access-lease`: 90 days. Leases represent paid access rights that must
//   outlive the payment window even if the parties go quiet.
// - `subscription`: 365 days. Subscription records are long-lived billing
//   anchors and must survive a full annual cycle without renewal traffic.
//
// All three share the same *shape*: a `BUMP_AMOUNT` retention window and a
// `LIFETIME_THRESHOLD` one day below it, so entries are only extended when
// they are close to archival. Only the window length differs per contract.
const DAY_IN_LEDGERS: u32 = 17280;
/// Registry retention window: 30 days (see the shared TTL policy above).
pub const REGISTRY_TTL_DAYS: u32 = 30;
/// Lease retention window: 90 days (see the shared TTL policy above).
pub const LEASE_TTL_DAYS: u32 = 90;
/// Subscription retention window: 365 days (see the shared TTL policy above).
pub const SUBSCRIPTION_TTL_DAYS: u32 = 365;
/// Retention window applied to this contract's persistent entries.
const BUMP_AMOUNT: u32 = REGISTRY_TTL_DAYS * DAY_IN_LEDGERS;
const LIFETIME_THRESHOLD: u32 = BUMP_AMOUNT - DAY_IN_LEDGERS;
// ~5s ledgers → 17,280 per day. Persistent entries are bumped ~30 days on each
// write so an actively-managed resource is never archived out from under us.
const BUMP_AMOUNT: u32 = BUMP;
const LIFETIME_THRESHOLD: u32 = BUMP_THRESH;
/// Maximum byte length of a metadata pointer (IPFS URI, content hash, compact
/// JSON anchor). Enforced on `register` and `update_metadata`; longer values
/// are rejected with [`Error::MetadataTooLong`].
pub const MAX_METADATA_POINTER_LEN: u32 = 512;
/// Maximum number of discovery tags allowed per resource. Exceeding this is
/// rejected with [`Error::InvalidTag`].
pub const MAX_TAGS: u32 = 8;
/// Maximum byte length of a single discovery tag. Tags must be non-empty and
/// no longer than this; violations are rejected with [`Error::InvalidTag`].
pub const MAX_TAG_LEN: u32 = 32;

/// Maximum page size returned by [`VaultRegistry::list`].
pub const MAX_PAGE_SIZE: u32 = 20;
/// Maximum byte length of a resource id. Ids must be non-empty and no longer
/// than this; violations are rejected with [`Error::InvalidResourceId`].
pub const MAX_RESOURCE_ID_LEN: u32 = 64;
/// Contract version, sourced from the crate manifest at compile time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A registered vault resource.
///
/// Invariants:
/// - `id` is unique across the registry and immutable once registered.
/// - `price` is strictly positive (in USDC stroops, 7 decimals).
/// - `metadata.len() <= MAX_METADATA_POINTER_LEN`.
/// - `tags.len() <= MAX_TAGS`, each tag non-empty and `<= MAX_TAG_LEN` bytes.
/// - Only `creator` may mutate the resource (enforced via `require_auth`).
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Resource {
    /// Unique, immutable resource identifier.
    pub id: String,
    /// Current owner; the only address authorized to mutate this resource.
    pub creator: Address,
    /// Price in USDC stroops (7 decimals); always strictly positive.
    pub price: i128,
    /// Off-chain content anchor (IPFS URI, content hash, etc.).
    pub metadata: String,
    /// Whether the resource is discoverable via `list`. Defaults to `true`.
    pub listed: bool,
    /// Discovery labels (e.g. "dataset", "research"). Distinct from `metadata`,
    /// which remains the off-chain content anchor (IPFS URI, content hash, etc.).
    pub tags: Vec<String>,
}

/// Storage keys for the registry.
///
/// `Resource(id)` and `Index(i)` live in persistent storage and have their TTL
/// bumped on every write; `Count` lives in instance storage and is bumped
/// alongside it.
#[contracttype]
pub enum DataKey {
    /// Maps a resource id to its [`Resource`] record.
    Resource(String),
    /// Monotonic count of resources ever registered (never decremented).
    Count,
    /// Maps insertion index `i` (in `0..Count`) to a resource id.
    Index(u32),
    /// Admin address allowed to upgrade the contract (instance storage).
    Admin,
    /// Contract version recorded at `init` (instance storage).
    Version,
    /// Pending new owner proposed by the current creator for a two-step
    /// ownership transfer. Cleared on [`VaultRegistry::accept_transfer`] or
    /// [`VaultRegistry::cancel_transfer`].
    PendingOwner(String),
}

/// Errors returned by registry operations.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// A resource with the given id is already registered.
    AlreadyRegistered = 1,
    /// No resource exists with the given id.
    NotFound = 2,
    /// Price must be strictly positive.
    InvalidPrice = 3,
    /// Metadata pointer exceeds [`MAX_METADATA_POINTER_LEN`].
    MetadataTooLong = 4,
    /// Tags exceed [`MAX_TAGS`], a tag is empty or exceeds [`MAX_TAG_LEN`], or
    /// duplicate tags were supplied (after case-normalisation).
    InvalidTag = 5,
    /// The caller is not the configured admin.
    NotAdmin = 6,
    /// `init` has not been called, so no admin is configured.
    NotInitialised = 7,
    /// `init` has already been called; the admin cannot be overwritten.
    AlreadyInitialised = 8,
    /// Metadata pointer must not be empty.
    MetadataEmpty = 9,
    /// Resource id is empty or exceeds [`MAX_RESOURCE_ID_LEN`].
    InvalidResourceId = 10,
    /// No pending ownership transfer exists for this resource id.
    NoPendingTransfer = 11,
    /// Only the proposed new owner may accept the transfer.
    NotPendingOwner = 12,
}

/// Emitted by `init`. Topics: `("init",)`; data: admin.
#[contractevent(topics = ["init"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitEvent {
    pub admin: Address,
}

/// Emitted by `upgrade`. Topics: `("upgrade",)`; data: new WASM hash.
#[contractevent(topics = ["upgrade"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeEvent {
    pub new_wasm_hash: BytesN<32>,
}

/// Emitted by `register`. Topics: `("register", creator)`; data: resource id.
#[contractevent(topics = ["register"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterEvent {
    #[topic]
    pub creator: Address,
    pub id: String,
}

/// Emitted by `set_price`. Topics: `("setprice", id)`; data: new price.
#[contractevent(topics = ["setprice"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetPriceEvent {
    #[topic]
    pub id: String,
    pub price: i128,
}

/// Emitted by `update_metadata`. Topics: `("updmeta", id)`; data: new metadata pointer.
///
/// The `new_metadata` field is included so that indexers can observe the new
/// content pointer directly from the event without an additional ledger read.
#[contractevent(topics = ["updmeta"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateMetadataEvent {
    #[topic]
    pub id: String,
    pub new_metadata: String,
}

/// Emitted by `set_tags`. Topics: `("settags", id)`; data: tags.
#[contractevent(topics = ["settags"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetTagsEvent {
    #[topic]
    pub id: String,
    pub tags: Vec<String>,
}

/// Emitted by `propose_transfer`. Topics: `("proptrans", id)`; data: proposed new creator.
#[contractevent(topics = ["proptrans"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposeTransferEvent {
    #[topic]
    pub id: String,
    pub proposed_creator: Address,
}

/// Emitted by `accept_transfer`. Topics: `("transfer", id)`; data: new creator.
#[contractevent(topics = ["transfer"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferEvent {
    #[topic]
    pub id: String,
    pub new_creator: Address,
}

/// Emitted by `cancel_transfer`. Topics: `("canceltrans", id)`; no data.
#[contractevent(topics = ["canceltrans"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelTransferEvent {
    #[topic]
    pub id: String,
}

/// Emitted by `set_listed`. Topics: `("setlisted", id)`; data: listed flag.
#[contractevent(topics = ["setlisted"], data_format = "single-value")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetListedEvent {
    #[topic]
    pub id: String,
    pub listed: bool,
}

#[contract]
pub struct VaultRegistry;

#[contractimpl]
impl VaultRegistry {
    /// Initialise the contract with an admin address. Only callable once.
    ///
    /// The admin is only needed for [`upgrade`](VaultRegistry::upgrade); the registry
    /// itself is usable without initialisation.
    ///
    /// # Errors
    /// - [`Error::AlreadyInitialised`] if an admin is already set.
    pub fn init(env: Env, admin: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialised);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::Version, &String::from_str(&env, VERSION));
        Self::bump_instance(&env);
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

    /// Upgrade the contract's WASM. Only the stored admin may call this.
    ///
    /// The stored version is intentionally **not** updated here: the new WASM
    /// is not live until after this call returns, so the version slot is
    /// refreshed by [`migrate`](VaultRegistry::migrate) once the new code is
    /// running. This keeps `get_version` honest across upgrades.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.deployer()
            .update_current_contract(ContractExecutable::Wasm(new_wasm_hash.clone()));
        UpgradeEvent { new_wasm_hash }.publish(&env);
        Ok(())
    }

    /// Admin-gated migration entrypoint, run after [`upgrade`](VaultRegistry::upgrade).
    ///
    /// Writes the running contract's compile-time version into the stored
    /// version slot so that [`get_version`](VaultRegistry::get_version) no
    /// longer reports the stale value recorded at `init`, and re-applies the
    /// shared persistent-TTL policy to the instance entry.
    ///
    /// This is idempotent: calling it repeatedly simply re-writes the current
    /// version and re-bumps the TTL.
    ///
    /// # Errors
    /// - [`Error::NotInitialised`] if `init` has not been called.
    /// - [`Error::NotAdmin`] / auth failure if the caller is not the admin.
    pub fn migrate(env: Env) -> Result<(), Error> {
        Self::require_admin(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Version, &String::from_str(&env, VERSION));
        Self::bump_instance(&env);
        Ok(())
    }

    /// Register a new resource.
    ///
    /// Requires the creator's authorization. The resource is listed by default.
    /// Tags are normalised to lowercase before storage so that `"Dataset"` and
    /// `"dataset"` are treated as identical labels; duplicate tags (after
    /// normalisation) are rejected.
    ///
    /// # Errors
    /// - [`Error::InvalidPrice`] if `price <= 0`.
    /// - [`Error::MetadataEmpty`] if `metadata` is an empty string.
    /// - [`Error::MetadataTooLong`] if `metadata` exceeds
    ///   [`MAX_METADATA_POINTER_LEN`].
    /// - [`Error::InvalidTag`] if `tags` exceed [`MAX_TAGS`], any tag is empty
    ///   or exceeds [`MAX_TAG_LEN`], or duplicate tags are supplied (after
    ///   case-normalisation).
    /// - [`Error::AlreadyRegistered`] if `id` already exists.
    ///
    /// # Invariants
    /// On success, `Count` is incremented by one (monotonic) and the new
    /// resource plus its index entry have their persistent TTL bumped.
    pub fn register(
        env: Env,
        creator: Address,
        id: String,
        price: i128,
        metadata: String,
        tags: Vec<String>,
    ) -> Result<(), Error> {
        creator.require_auth();
        Self::validate_resource_id(&id)?;
        if price <= 0 {
            return Err(Error::InvalidPrice);
        }
        Self::validate_metadata_pointer(&metadata)?;
        let normalised_tags = Self::validate_and_normalise_tags(&env, &tags)?;
        let key = DataKey::Resource(id.clone());
        if env.storage().persistent().has(&key) {
            return Err(Error::AlreadyRegistered);
        }

        let resource = Resource {
            id: id.clone(),
            creator: creator.clone(),
            price,
            metadata,
            listed: true, // Resources are listed by default when registered
            tags: normalised_tags,
        };
        env.storage().persistent().set(&key, &resource);
        Self::bump_persistent(&env, &key);

        let count: u32 = env.storage().instance().get(&DataKey::Count).unwrap_or(0);
        let idx_key = DataKey::Index(count);
        env.storage().persistent().set(&idx_key, &id);
        Self::bump_persistent(&env, &idx_key);
        env.storage().instance().set(&DataKey::Count, &(count + 1));
        Self::bump_instance(&env);

        RegisterEvent { creator, id }.publish(&env);
        Ok(())
    }

    /// Update a resource's price.
    ///
    /// Only the creator may call this (enforced via `require_auth`).
    ///
    /// # Errors
    /// - [`Error::InvalidResourceId`] if `id` is empty or exceeds [`MAX_RESOURCE_ID_LEN`].
    /// - [`Error::NotFound`] if no resource exists with `id`.
    /// - [`Error::InvalidPrice`] if `new_price <= 0`.
    pub fn set_price(env: Env, id: String, new_price: i128) -> Result<(), Error> {
        Self::validate_resource_id(&id)?;
        let mut resource = Self::load(&env, &id)?;
        if new_price <= 0 {
            return Err(Error::InvalidPrice);
        }
        resource.creator.require_auth();
        resource.price = new_price;
        Self::save(&env, &resource);
        SetPriceEvent {
            id,
            price: new_price,
        }
        .publish(&env);
        Ok(())
    }

    /// Update a resource's metadata pointer.
    ///
    /// Only the creator may call this (enforced via `require_auth`).
    ///
    /// # Errors
    /// - [`Error::InvalidResourceId`] if `id` is empty or exceeds [`MAX_RESOURCE_ID_LEN`].
    /// - [`Error::NotFound`] if no resource exists with `id`.
    /// - [`Error::MetadataEmpty`] if `metadata` is an empty string.
    /// - [`Error::MetadataTooLong`] if `metadata` exceeds
    ///   [`MAX_METADATA_POINTER_LEN`].
    pub fn update_metadata(env: Env, id: String, metadata: String) -> Result<(), Error> {
        Self::validate_resource_id(&id)?;
        let mut resource = Self::load(&env, &id)?;
        resource.creator.require_auth();
        Self::validate_metadata_pointer(&metadata)?;
        resource.metadata = metadata.clone();
        Self::save(&env, &resource);
        UpdateMetadataEvent { id, new_metadata: metadata }.publish(&env);
        Ok(())
    }

    /// Replace a resource's discovery tags.
    ///
    /// Only the creator may call this (enforced via `require_auth`). Does not
    /// modify `metadata` (the off-chain content pointer). Tags are normalised
    /// to lowercase before storage; duplicate tags (after normalisation) are
    /// rejected.
    ///
    /// # Errors
    /// - [`Error::InvalidResourceId`] if `id` is empty or exceeds [`MAX_RESOURCE_ID_LEN`].
    /// - [`Error::NotFound`] if no resource exists with `id`.
    /// - [`Error::InvalidTag`] if `tags` exceed [`MAX_TAGS`], any tag is empty
    ///   or exceeds [`MAX_TAG_LEN`], or duplicate tags are supplied (after
    ///   case-normalisation).
    pub fn set_tags(env: Env, id: String, tags: Vec<String>) -> Result<(), Error> {
        Self::validate_resource_id(&id)?;
        let mut resource = Self::load(&env, &id)?;
        let normalised_tags = Self::validate_and_normalise_tags(&env, &tags)?;
        resource.creator.require_auth();
        resource.tags = normalised_tags.clone();
        Self::save(&env, &resource);
        SetTagsEvent { id, tags: normalised_tags }.publish(&env);
        Ok(())
    }

    /// Propose a two-step ownership transfer to `new_creator`.
    ///
    /// Step 1 of 2: the current creator proposes a new owner. This stores a
    /// pending proposal on-chain but does **not** transfer ownership yet.
    /// The proposed new creator must call [`accept_transfer`](VaultRegistry::accept_transfer)
    /// to complete the transfer. The current creator may call
    /// [`cancel_transfer`](VaultRegistry::cancel_transfer) to abort.
    ///
    /// If `new_creator` is already the current creator the call returns early
    /// with no write and no event (no-op).
    ///
    /// # Errors
    /// - [`Error::InvalidResourceId`] if `id` is empty or exceeds [`MAX_RESOURCE_ID_LEN`].
    /// - [`Error::NotFound`] if no resource exists with `id`.
    pub fn propose_transfer(env: Env, id: String, new_creator: Address) -> Result<(), Error> {
        Self::validate_resource_id(&id)?;
        let resource = Self::load(&env, &id)?;
        resource.creator.require_auth();
        // #78: no-op when proposed owner is already the current creator.
        if resource.creator == new_creator {
            return Ok(());
        }
        let key = DataKey::PendingOwner(id.clone());
        env.storage().persistent().set(&key, &new_creator);
        Self::bump_persistent(&env, &key);
        ProposeTransferEvent {
            id,
            proposed_creator: new_creator,
        }
        .publish(&env);
        Ok(())
    }

    /// Accept a pending ownership transfer.
    ///
    /// Step 2 of 2: the proposed new owner accepts and becomes the creator.
    /// The pending proposal is removed; the resource's `creator` field is
    /// updated atomically.
    ///
    /// # Errors
    /// - [`Error::InvalidResourceId`] if `id` is empty or exceeds [`MAX_RESOURCE_ID_LEN`].
    /// - [`Error::NotFound`] if no resource exists with `id`.
    /// - [`Error::NoPendingTransfer`] if no proposal exists for `id`.
    /// - [`Error::NotPendingOwner`] if the caller is not the proposed new owner.
    pub fn accept_transfer(env: Env, id: String, new_creator: Address) -> Result<(), Error> {
        Self::validate_resource_id(&id)?;
        let mut resource = Self::load(&env, &id)?;
        let pending_key = DataKey::PendingOwner(id.clone());
        let pending: Address = env
            .storage()
            .persistent()
            .get(&pending_key)
            .ok_or(Error::NoPendingTransfer)?;
        if pending != new_creator {
            return Err(Error::NotPendingOwner);
        }
        new_creator.require_auth();
        env.storage().persistent().remove(&pending_key);
        resource.creator = new_creator.clone();
        Self::save(&env, &resource);
        TransferEvent {
            id,
            new_creator,
        }
        .publish(&env);
        Ok(())
    }

    /// Cancel a pending ownership transfer.
    ///
    /// Only the current creator may cancel. Removes the pending proposal
    /// without transferring ownership. If no proposal exists the call is a
    /// no-op (no write, no event).
    ///
    /// # Errors
    /// - [`Error::InvalidResourceId`] if `id` is empty or exceeds [`MAX_RESOURCE_ID_LEN`].
    /// - [`Error::NotFound`] if no resource exists with `id`.
    pub fn cancel_transfer(env: Env, id: String) -> Result<(), Error> {
        Self::validate_resource_id(&id)?;
        let resource = Self::load(&env, &id)?;
        resource.creator.require_auth();
        let pending_key = DataKey::PendingOwner(id.clone());
        if !env.storage().persistent().has(&pending_key) {
            return Ok(());
        }
        env.storage().persistent().remove(&pending_key);
        CancelTransferEvent { id }.publish(&env);
        Ok(())
    }

    /// Set the listing state of a resource.
    ///
    /// Only the creator may call this (enforced via `require_auth`).
    /// If `listed` already matches the current value, the call is a no-op
    /// (no storage write, no event emitted).
    ///
    /// # Errors
    /// - [`Error::InvalidResourceId`] if `id` is empty or exceeds [`MAX_RESOURCE_ID_LEN`].
    /// - [`Error::NotFound`] if no resource exists with `id`.
    pub fn set_listed(env: Env, id: String, listed: bool) -> Result<(), Error> {
        Self::validate_resource_id(&id)?;
        let mut resource = Self::load(&env, &id)?;
        resource.creator.require_auth();
        if resource.listed == listed {
            return Ok(());
        }
        resource.listed = listed;
        Self::save(&env, &resource);
        SetListedEvent { id, listed }.publish(&env);
        Ok(())
    }

    /// Delist a resource (convenience method for `set_listed(false)`).
    ///
    /// Only the creator may call this (enforced via `require_auth`).
    ///
    /// # Errors
    /// - [`Error::NotFound`] if no resource exists with `id`.
    pub fn delist(env: Env, id: String) -> Result<(), Error> {
        Self::set_listed(env, id, false)
    }

    /// Paginated resource list in insertion order.
    ///
    /// Returns up to `limit` full [`Resource`] values starting at insertion
    /// index `start`; `limit` is capped at [`MAX_PAGE_SIZE`]. Missing index or
    /// resource entries are skipped.
    ///
    /// For a lighter alternative that avoids transferring large metadata
    /// strings, see [`VaultRegistry::list_ids`].
    pub fn list(env: Env, start: u32, limit: u32) -> Vec<Resource> {
        let total: u32 = env.storage().instance().get(&DataKey::Count).unwrap_or(0);
        let page_size = limit.min(MAX_PAGE_SIZE);
        let mut result: Vec<Resource> = Vec::new(&env);
        let mut i = start;
        while i < total && result.len() < page_size {
            if let Some(id) = env
                .storage()
                .persistent()
                .get::<DataKey, String>(&DataKey::Index(i))
            {
                if let Some(resource) = env
                    .storage()
                    .persistent()
                    .get::<DataKey, Resource>(&DataKey::Resource(id))
                {
                    result.push_back(resource);
                }
            }
            i += 1;
        }
        result
    }

    /// Paginated summary list — returns only resource IDs in insertion order.
    ///
    /// This is the lightweight alternative to [`VaultRegistry::list`]: instead
    /// of loading full [`Resource`] values (each carrying up to
    /// `MAX_METADATA_POINTER_LEN` bytes of metadata), this function returns
    /// only the resource id strings, drastically reducing read bandwidth and
    /// compute fees on large registries.
    ///
    /// Returns up to `limit` ids starting at insertion index `start`; `limit`
    /// is capped at [`MAX_PAGE_SIZE`]. Missing index entries are skipped.
    /// Callers can then fetch individual resources with [`VaultRegistry::get`]
    /// for any ids they are interested in.
    pub fn list_ids(env: Env, start: u32, limit: u32) -> Vec<String> {
        let total: u32 = env.storage().instance().get(&DataKey::Count).unwrap_or(0);
        let page_size = limit.min(MAX_PAGE_SIZE);
        let mut result: Vec<String> = Vec::new(&env);
        let mut i = start;
        while i < total && result.len() < page_size {
            if let Some(id) = env
                .storage()
                .persistent()
                .get::<DataKey, String>(&DataKey::Index(i))
            {
                result.push_back(id);
            }
            i += 1;
        }
        result
    }

    /// Fetch a resource.
    ///
    /// # Errors
    /// - [`Error::NotFound`] if no resource exists with `id`.
    pub fn get(env: Env, id: String) -> Result<Resource, Error> {
        Self::load(&env, &id)
    }

    /// Whether a resource with `id` is registered.
    pub fn exists(env: Env, id: String) -> bool {
        env.storage().persistent().has(&DataKey::Resource(id))
    }

    /// Get the owner address of a resource.
    ///
    /// # Errors
    /// - [`Error::NotFound`] if no resource exists with `id`.
    pub fn get_owner(env: Env, id: String) -> Result<Address, Error> {
        let resource = Self::load(&env, &id)?;
        Ok(resource.creator)
    }

    /// Total number of resources successfully registered.
    ///
    /// Monotonic: incremented on each successful `register` and never
    /// decremented (including on ownership transfer).
    pub fn count(env: Env) -> u32 {
        env.storage().instance().get(&DataKey::Count).unwrap_or(0)
    }

    /// Return the compile-time contract version of the running WASM.
    pub fn version(env: Env) -> String {
        String::from_str(&env, VERSION)
    }

    /// Return the version recorded in storage.
    ///
    /// The stored value is written by `init` and refreshed by
    /// [`migrate`](VaultRegistry::migrate) after every upgrade, so this
    /// reflects the currently-running WASM rather than the version that was
    /// first deployed. Falls back to the compile-time `VERSION` constant if no
    /// version has been stored yet.
    pub fn get_version(env: Env) -> String {
        common::get_version(&env)
    }

    /// Whether `version` is compatible with this contract. A version is
    /// considered compatible when its major/minor components match the
    /// compile-time `VERSION`.
    pub fn is_compatible(version: String) -> bool {
        common::is_compatible(&version)
    }
}

impl VaultRegistry {
    fn require_admin(env: &Env) -> Result<(), Error> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialised)?;
        common::require_admin(env, &admin);
        Ok(())
    }

    /// Validate a resource id: must be non-empty and no longer than
    /// [`MAX_RESOURCE_ID_LEN`] bytes.
    fn validate_resource_id(id: &String) -> Result<(), Error> {
        let len = id.len();
        if len == 0 || len > MAX_RESOURCE_ID_LEN {
            return Err(Error::InvalidResourceId);
        }
        Ok(())
    }

    fn validate_metadata_pointer(metadata: &String) -> Result<(), Error> {
        if metadata.len() == 0 {
            return Err(Error::MetadataEmpty);
        }
        if metadata.len() > MAX_METADATA_POINTER_LEN {
            return Err(Error::MetadataTooLong);
        }
        Ok(())
    }

    /// Validate and normalise a tag list.
    ///
    /// Each tag is lower-cased in-place so that "Dataset" and "dataset" are
    /// treated as the same label (fixes #74). After normalisation, duplicate
    /// tags are detected and rejected with [`Error::InvalidTag`] (fixes #73).
    /// The count and per-tag length limits are enforced before de-dup so that
    /// callers receive a fast early error on obviously-invalid input.
    fn validate_and_normalise_tags(env: &Env, tags: &Vec<String>) -> Result<Vec<String>, Error> {
        if tags.len() > MAX_TAGS {
            return Err(Error::InvalidTag);
        }

        let mut normalised: Vec<String> = Vec::new(env);

        for tag in tags.iter() {
            let len = tag.len();
            if len == 0 || len > MAX_TAG_LEN {
                return Err(Error::InvalidTag);
            }

            // Lower-case the tag byte-by-byte (ASCII only; Soroban Strings are
            // arbitrary bytes but tags are documented as ASCII labels).
            let mut buf = [0u8; MAX_TAG_LEN as usize];
            tag.copy_into_slice(&mut buf[..len as usize]);
            for b in buf[..len as usize].iter_mut() {
                if b.is_ascii_uppercase() {
                    *b = b.to_ascii_lowercase();
                }
            }
            let lower = String::from_bytes(env, &soroban_sdk::Bytes::from_slice(env, &buf[..len as usize]));

            // Reject duplicate tags (after normalisation).
            for existing in normalised.iter() {
                if existing == lower {
                    return Err(Error::InvalidTag);
                }
            }

            normalised.push_back(lower);
        }

        Ok(normalised)
    }

    fn load(env: &Env, id: &String) -> Result<Resource, Error> {
        let key = DataKey::Resource(id.clone());
        let resource = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::NotFound)?;
        // Bump on read so frequently-read resources are not archived out from
        // under us even when they are never written again.
        Self::bump_persistent(env, &key);
        Ok(resource)
    }

    fn save(env: &Env, resource: &Resource) {
        let key = DataKey::Resource(resource.id.clone());
        env.storage().persistent().set(&key, resource);
        Self::bump_persistent(env, &key);
    }

    /// Extend a persistent entry's TTL when below threshold (Soroban archival
    /// safety). Called on every persistent write so actively-managed resources
    /// are never archived out from under us.
    fn bump_persistent<K>(env: &Env, key: &K)
    where
        K: IntoVal<Env, Val>,
    {
        env.storage()
            .persistent()
            .extend_ttl(key, LIFETIME_THRESHOLD, BUMP_AMOUNT);
    }

    /// Extend the instance entry's TTL when below threshold. Called alongside
    /// persistent bumps so `Count` and the index stay live.
    fn bump_instance(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(LIFETIME_THRESHOLD, BUMP_AMOUNT);
    }

}

#[cfg(test)]
pub(crate) const TTL_BUMP_AMOUNT: u32 = BUMP_AMOUNT;

mod test;
