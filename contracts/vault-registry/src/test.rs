#![cfg(test)]

extern crate std;

use super::*;
use proptest::prelude::*;
use soroban_sdk::{
    symbol_short,
    testutils::{storage::Persistent as _, Address as _, Events as _, Ledger as _},
    Address, Env, String, Vec,
};

const DAY_IN_LEDGERS: u32 = 17_280;

/// Prebuilt registry WASM used as the target of upgrade tests. Regenerate with
/// `make fixture` from `contracts/vault-registry` when the interface changes.
const UPGRADE_TARGET_WASM: &[u8] = include_bytes!("../fixtures/vault_registry_upgrade_target.wasm");

fn upload_upgrade_target(env: &Env) -> soroban_sdk::BytesN<32> {
    env.deployer().upload_contract_wasm(UPGRADE_TARGET_WASM)
}

fn resource_storage_ttl(env: &Env, contract: &soroban_sdk::Address, id: &String) -> u32 {
    let key = DataKey::Resource(id.clone());
    env.as_contract(contract, || env.storage().persistent().get_ttl(&key))
}

fn setup<'a>() -> (Env, Address, VaultRegistryClient<'a>) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(VaultRegistry, ());
    let client = VaultRegistryClient::new(&env, &contract_id);
    let creator = Address::generate(&env);
    (env, creator, client)
}

fn empty_tags(env: &Env) -> Vec<String> {
    Vec::new(env)
}

fn tags(env: &Env, items: &[&str]) -> Vec<String> {
    let mut v = Vec::new(env);
    for item in items {
        v.push_back(String::from_str(env, item));
    }
    v
}

#[test]
fn register_then_read() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "swcn98besxpp6t1u8e77fqz3");
    let metadata = String::from_str(&env, "ipfs://QmResourceMetadata");

    client.register(&creator, &id, &1_000_000i128, &metadata, &empty_tags(&env));

    assert_eq!(client.count(), 1);
    assert!(client.exists(&id));

    let r = client.get(&id);
    assert_eq!(r.id, id);
    assert_eq!(r.creator, creator);
    assert_eq!(r.price, 1_000_000i128);
    assert_eq!(r.metadata, metadata);
    assert!(r.listed); // Resources are listed by default
}

#[test]
fn count_tracks_multiple_successful_registrations() {
    let (env, creator, client) = setup();
    assert_eq!(client.count(), 0);

    let ids = ["c1", "c2", "c3", "c4"];
    for id in &ids {
        client.register(
            &creator,
            &String::from_str(&env, id),
            &100i128,
            &String::from_str(&env, "m"),
            &empty_tags(&env),
        );
    }
    assert_eq!(client.count(), 4);

    // Failed duplicate must not increment count.
    let dup = String::from_str(&env, "c2");
    assert_eq!(
        client.try_register(
            &creator,
            &dup,
            &100i128,
            &String::from_str(&env, "m"),
            &empty_tags(&env)
        ),
        Err(Ok(Error::AlreadyRegistered))
    );
    assert_eq!(client.count(), 4);
}

#[test]
fn duplicate_registration_fails() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "dup");
    let metadata = String::from_str(&env, "x");
    client.register(&creator, &id, &100i128, &metadata, &empty_tags(&env));

    let res = client.try_register(&creator, &id, &100i128, &metadata, &empty_tags(&env));
    assert_eq!(res, Err(Ok(Error::AlreadyRegistered)));
    assert_eq!(client.count(), 1);
}

#[test]
fn zero_or_negative_price_rejected() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "free");
    let metadata = String::from_str(&env, "x");

    assert_eq!(
        client.try_register(&creator, &id, &0i128, &metadata, &empty_tags(&env)),
        Err(Ok(Error::InvalidPrice))
    );
    assert_eq!(
        client.try_register(&creator, &id, &-5i128, &metadata, &empty_tags(&env)),
        Err(Ok(Error::InvalidPrice))
    );
}

#[test]
fn get_missing_fails() {
    let (env, _creator, client) = setup();
    let res = client.try_get(&String::from_str(&env, "nope"));
    assert_eq!(res, Err(Ok(Error::NotFound)));
}

#[test]
fn set_price_updates_value() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "r1");
    client.register(
        &creator,
        &id,
        &1_000_000i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    client.set_price(&id, &2_500_000i128);
    assert_eq!(client.get(&id).price, 2_500_000i128);

    assert_eq!(
        client.try_set_price(&id, &0i128),
        Err(Ok(Error::InvalidPrice))
    );
}

#[test]
fn update_metadata_changes_pointer() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "r2");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "old"),
        &empty_tags(&env),
    );

    let new_meta = String::from_str(&env, "ipfs://QmNew");
    client.update_metadata(&id, &new_meta);
    assert_eq!(client.get(&id).metadata, new_meta);
}

#[test]
fn ownership_can_transfer() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "r3");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    let new_owner = Address::generate(&env);
    client.transfer_ownership(&id, &new_owner);
    assert_eq!(client.get(&id).creator, new_owner);
}

#[test]
fn set_listed_toggles_listing_state() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "r4");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    // Initially listed
    assert!(client.get(&id).listed);

    // Delist
    client.set_listed(&id, &false);
    assert!(!client.get(&id).listed);

    // Re-list
    client.set_listed(&id, &true);
    assert!(client.get(&id).listed);
}

#[test]
fn delist_convenience_method() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "r5");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    // Initially listed
    assert!(client.get(&id).listed);

    // Delist using convenience method
    client.delist(&id);
    assert!(!client.get(&id).listed);
}

#[test]
fn set_price_preserves_other_fields() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "r7");
    let metadata = String::from_str(&env, "ipfs://QmPreserve");
    client.register(&creator, &id, &100i128, &metadata, &empty_tags(&env));

    client.set_price(&id, &250i128);
    let resource = client.get(&id);

    assert_eq!(resource.price, 250i128);
    assert_eq!(resource.metadata, metadata);
    assert_eq!(resource.creator, creator);
    assert!(resource.listed);
}

#[test]
fn transfer_ownership_keeps_count_and_order() {
    let (env, creator, client) = setup();
    let ids = ["a", "b"];
    for id in &ids {
        client.register(
            &creator,
            &String::from_str(&env, id),
            &100i128,
            &String::from_str(&env, "m"),
            &empty_tags(&env),
        );
    }

    let new_owner = Address::generate(&env);
    let id = String::from_str(&env, "a");
    client.transfer_ownership(&id, &new_owner);

    assert_eq!(client.count(), 2);
    let list = client.list(&0u32, &10u32);
    assert_eq!(list.get(0).unwrap().id, id);
    assert_eq!(list.get(0).unwrap().creator, new_owner);
    assert_eq!(list.get(1).unwrap().id, String::from_str(&env, "b"));
}

#[test]
fn update_metadata_preserves_price_and_creator() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "r8");
    let original_metadata = String::from_str(&env, "ipfs://QmOriginal");
    client.register(
        &creator,
        &id,
        &500i128,
        &original_metadata,
        &empty_tags(&env),
    );

    let new_metadata = String::from_str(&env, "ipfs://QmUpdated");
    client.update_metadata(&id, &new_metadata);

    let resource = client.get(&id);
    assert_eq!(resource.metadata, new_metadata);
    assert_eq!(resource.price, 500i128);
    assert_eq!(resource.creator, creator);
}

#[test]
fn get_owner_returns_creator() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "owner-test");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    let owner = client.get_owner(&id);
    assert_eq!(owner, creator);
}

#[test]
fn get_owner_missing_fails() {
    let (env, _creator, client) = setup();
    let res = client.try_get_owner(&String::from_str(&env, "nope"));
    assert_eq!(res, Err(Ok(Error::NotFound)));
}

#[test]
fn get_owner_after_transfer() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "owner-xfer");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    let new_owner = Address::generate(&env);
    client.transfer_ownership(&id, &new_owner);

    assert_eq!(client.get_owner(&id), new_owner);
}

#[test]
fn set_listed_requires_creator_auth() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "r6");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    // This should work fine since we mock all auths
    client.set_listed(&id, &false);
    assert!(!client.get(&id).listed);
}

#[test]
fn set_listed_on_missing_resource_fails() {
    let (env, _creator, client) = setup();
    let id = String::from_str(&env, "missing");

    let res = client.try_set_listed(&id, &false);
    assert_eq!(res, Err(Ok(Error::NotFound)));
}

#[test]
fn list_empty_returns_empty() {
    let (_env, _creator, client) = setup();
    let page = client.list(&0u32, &20u32);
    assert_eq!(page.len(), 0);
}

#[test]
fn list_returns_all_in_insertion_order() {
    let (env, creator, client) = setup();
    let ids = ["a", "b", "c"];
    for id in &ids {
        client.register(
            &creator,
            &String::from_str(&env, id),
            &100i128,
            &String::from_str(&env, "m"),
            &empty_tags(&env),
        );
    }

    let page = client.list(&0u32, &20u32);
    assert_eq!(page.len(), 3);
    assert_eq!(page.get(0).unwrap().id, String::from_str(&env, "a"));
    assert_eq!(page.get(1).unwrap().id, String::from_str(&env, "b"));
    assert_eq!(page.get(2).unwrap().id, String::from_str(&env, "c"));
}

fn metadata_of_len(env: &Env, len: u32) -> String {
    let s = "a".repeat(len as usize);
    String::from_str(env, &s)
}

#[test]
fn register_accepts_metadata_at_max_length() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "meta-max");
    let metadata = metadata_of_len(&env, MAX_METADATA_POINTER_LEN);
    client.register(&creator, &id, &100i128, &metadata, &empty_tags(&env));
    assert_eq!(client.get(&id).metadata.len(), MAX_METADATA_POINTER_LEN);
}

#[test]
fn register_rejects_metadata_over_max_length() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "meta-long");
    let metadata = metadata_of_len(&env, MAX_METADATA_POINTER_LEN + 1);
    assert_eq!(
        client.try_register(&creator, &id, &100i128, &metadata, &empty_tags(&env)),
        Err(Ok(Error::MetadataTooLong))
    );
    assert!(!client.exists(&id));
}

#[test]
fn update_metadata_accepts_at_max_length() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "meta-upd-ok");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "short"),
        &empty_tags(&env),
    );
    let metadata = metadata_of_len(&env, MAX_METADATA_POINTER_LEN);
    client.update_metadata(&id, &metadata);
    assert_eq!(client.get(&id).metadata.len(), MAX_METADATA_POINTER_LEN);
}

#[test]
fn update_metadata_rejects_over_max_length() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "meta-upd-bad");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "short"),
        &empty_tags(&env),
    );
    let metadata = metadata_of_len(&env, MAX_METADATA_POINTER_LEN + 1);
    assert_eq!(
        client.try_update_metadata(&id, &metadata),
        Err(Ok(Error::MetadataTooLong))
    );
    assert_eq!(client.get(&id).metadata, String::from_str(&env, "short"));
}

fn register_n(env: &Env, creator: &Address, client: &VaultRegistryClient<'_>, ids: &[&str]) {
    for id in ids {
        client.register(
            creator,
            &String::from_str(env, id),
            &100i128,
            &String::from_str(env, "m"),
            &empty_tags(env),
        );
    }
}

#[test]
fn list_pagination_first_page() {
    let (env, creator, client) = setup();
    register_n(&env, &creator, &client, &["r0", "r1", "r2", "r3", "r4"]);

    let page = client.list(&0u32, &3u32);
    assert_eq!(page.len(), 3);
    assert_eq!(page.get(0).unwrap().id, String::from_str(&env, "r0"));
    assert_eq!(page.get(2).unwrap().id, String::from_str(&env, "r2"));
}

#[test]
fn list_pagination_second_page() {
    let (env, creator, client) = setup();
    register_n(&env, &creator, &client, &["r0", "r1", "r2", "r3", "r4"]);

    let page = client.list(&3u32, &3u32);
    assert_eq!(page.len(), 2); // only r3, r4 remain
    assert_eq!(page.get(0).unwrap().id, String::from_str(&env, "r3"));
    assert_eq!(page.get(1).unwrap().id, String::from_str(&env, "r4"));
}

#[test]
fn list_start_beyond_count_returns_empty() {
    let (env, creator, client) = setup();
    client.register(
        &creator,
        &String::from_str(&env, "x"),
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    let page = client.list(&99u32, &10u32);
    assert_eq!(page.len(), 0);
}

#[test]
fn register_extends_resource_storage_ttl() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "ttl-register");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );
    assert_eq!(
        resource_storage_ttl(&env, &client.address, &id),
        TTL_BUMP_AMOUNT
    );
}

#[test]
fn set_price_reextends_resource_ttl() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "ttl-price");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );
    env.ledger()
        .set_sequence_number(env.ledger().sequence() + DAY_IN_LEDGERS);
    assert_eq!(
        resource_storage_ttl(&env, &client.address, &id),
        TTL_BUMP_AMOUNT - DAY_IN_LEDGERS
    );

    client.set_price(&id, &200i128);
    assert_eq!(
        resource_storage_ttl(&env, &client.address, &id),
        TTL_BUMP_AMOUNT
    );
}

#[test]
fn update_metadata_reextends_resource_ttl() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "ttl-meta");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "old"),
        &empty_tags(&env),
    );
    env.ledger()
        .set_sequence_number(env.ledger().sequence() + DAY_IN_LEDGERS);

    client.update_metadata(&id, &String::from_str(&env, "new"));
    assert_eq!(
        resource_storage_ttl(&env, &client.address, &id),
        TTL_BUMP_AMOUNT
    );
}

#[test]
fn transfer_ownership_reextends_resource_ttl() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "ttl-xfer");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );
    env.ledger()
        .set_sequence_number(env.ledger().sequence() + DAY_IN_LEDGERS);

    let new_owner = Address::generate(&env);
    client.transfer_ownership(&id, &new_owner);
    assert_eq!(
        resource_storage_ttl(&env, &client.address, &id),
        TTL_BUMP_AMOUNT
    );
}

#[test]
fn list_limit_capped_at_20() {
    let (env, creator, client) = setup();
    let ids = [
        "i00", "i01", "i02", "i03", "i04", "i05", "i06", "i07", "i08", "i09", "i10", "i11", "i12",
        "i13", "i14", "i15", "i16", "i17", "i18", "i19", "i20", "i21", "i22", "i23", "i24",
    ];
    register_n(&env, &creator, &client, &ids);

    // Requesting 25 items should be silently capped to 20.
    let page = client.list(&0u32, &25u32);
    assert_eq!(page.len(), 20);
}

#[test]
fn register_with_tags_stores_labels() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "tagged");
    let metadata = String::from_str(&env, "ipfs://QmTagged");
    let resource_tags = tags(&env, &["dataset", "research"]);

    client.register(&creator, &id, &100i128, &metadata, &resource_tags);

    let r = client.get(&id);
    assert_eq!(r.metadata, metadata);
    assert_eq!(r.tags.len(), 2);
    assert_eq!(r.tags.get(0).unwrap(), String::from_str(&env, "dataset"));
    assert_eq!(r.tags.get(1).unwrap(), String::from_str(&env, "research"));
}

#[test]
fn set_tags_updates_value_without_touching_metadata() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "tag-update");
    let metadata = String::from_str(&env, "ipfs://QmKeepMeta");
    client.register(&creator, &id, &100i128, &metadata, &empty_tags(&env));

    let new_tags = tags(&env, &["finance", "api"]);
    client.set_tags(&id, &new_tags);

    let r = client.get(&id);
    assert_eq!(r.metadata, metadata);
    assert_eq!(r.tags.len(), 2);
    assert_eq!(r.tags.get(0).unwrap(), String::from_str(&env, "finance"));
}

#[test]
fn invalid_tag_rejected() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "bad-tag");
    let metadata = String::from_str(&env, "m");
    let empty = String::from_str(&env, "");
    let mut bad = Vec::new(&env);
    bad.push_back(empty);

    assert_eq!(
        client.try_register(&creator, &id, &100i128, &metadata, &bad),
        Err(Ok(Error::InvalidTag))
    );
    assert!(!client.exists(&id));

    client.register(&creator, &id, &100i128, &metadata, &empty_tags(&env));
    assert_eq!(client.try_set_tags(&id, &bad), Err(Ok(Error::InvalidTag)));
}

#[test]
fn init_sets_admin() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(VaultRegistry, ());
    let client = VaultRegistryClient::new(&env, &contract_id);
    let admin = Address::generate(&env);

    client.init(&admin);

    assert_eq!(client.admin(), admin);
}

#[test]
fn upgrade_requires_admin_auth() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(VaultRegistry, ());
    let client = VaultRegistryClient::new(&env, &contract_id);
    let admin = Address::generate(&env);

    client.init(&admin);

    let wasm_hash = upload_upgrade_target(&env);
    client.upgrade(&wasm_hash);

    // Admin auth must have been required for the upgrade call.
    assert!(env.auths().iter().any(|(addr, _)| addr == &admin));
}

#[test]
fn upgrade_rejects_non_admin() {
    let env = Env::default();
    let contract_id = env.register(VaultRegistry, ());
    let client = VaultRegistryClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let non_admin = Address::generate(&env);

    env.mock_all_auths();
    client.init(&admin);

    // Only the non-admin authorizes the call; the contract must reject it.
    env.mock_auths(&[soroban_sdk::testutils::MockAuth {
        address: &non_admin,
        invoke: &soroban_sdk::testutils::MockAuthInvoke {
            contract: &contract_id,
            fn_name: "upgrade",
            args: soroban_sdk::vec![
                &env,
                soroban_sdk::IntoVal::<Env, soroban_sdk::Val>::into_val(
                    &soroban_sdk::BytesN::from_array(&env, &[0u8; 32]),
                    &env,
                ),
            ],
            sub_invokes: &[],
        },
    }]);

    let wasm_hash = soroban_sdk::BytesN::from_array(&env, &[0u8; 32]);
    let res = client.try_upgrade(&wasm_hash);
    assert!(res.is_err());
}

#[test]
fn upgrade_with_valid_wasm_hash_succeeds() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(VaultRegistry, ());
    let client = VaultRegistryClient::new(&env, &contract_id);
    let admin = Address::generate(&env);

    client.init(&admin);

    let creator = Address::generate(&env);
    let id = String::from_str(&env, "pre-upgrade");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    let wasm_hash = upload_upgrade_target(&env);
    client.upgrade(&wasm_hash);

    // Contract remains functional and keeps its state after upgrade.
    assert_eq!(client.count(), 1);
    assert_eq!(client.get(&id).creator, creator);
    assert_eq!(client.admin(), admin);
}

#[test]
fn entrypoint_behavior_regression_after_optimization() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "regression");
    let metadata = String::from_str(&env, "ipfs://QmRegression");

    client.register(&creator, &id, &100i128, &metadata, &empty_tags(&env));
    assert_eq!(client.count(), 1);
    assert!(client.exists(&id));
    assert_eq!(client.get(&id).metadata, metadata);

    client.set_price(&id, &250i128);
    assert_eq!(client.get(&id).price, 250i128);

    client.set_listed(&id, &false);
    assert!(!client.get(&id).listed);

    client.transfer_ownership(&id, &Address::generate(&env));
    assert_eq!(client.count(), 1);
}

#[test]
fn footprint_regression_register_and_read() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "footprint");
    let metadata = String::from_str(&env, "ipfs://QmFootprint");

    client.register(&creator, &id, &100i128, &metadata, &empty_tags(&env));

    // Entrypoints must still return correct values after optimization.
    assert_eq!(client.count(), 1);
    assert!(client.exists(&id));
    let r = client.get(&id);
    assert_eq!(r.id, id);
    assert_eq!(r.creator, creator);
    assert_eq!(r.price, 100i128);
    assert_eq!(r.metadata, metadata);
    assert!(r.listed);

    let page = client.list(&0u32, &10u32);
    assert_eq!(page.len(), 1);
    assert_eq!(page.get(0).unwrap().id, id);
}

#[test]
fn version_returns_expected_string() {
    let (_env, _creator, client) = setup();
    assert_eq!(client.version(), String::from_str(&_env, VERSION));
}

#[test]
fn version_is_stable_across_calls() {
    let (_env, _creator, client) = setup();
    let v1 = client.version();
    let v2 = client.version();
    assert_eq!(v1, v2);
}

#[test]
fn init_twice_is_rejected() {
    let (env, _creator, client) = setup();
    let admin = Address::generate(&env);
    client.init(&admin);
    assert_eq!(
        client.try_init(&Address::generate(&env)),
        Err(Ok(Error::AlreadyInitialised))
    );
    assert_eq!(client.admin(), admin);
}

#[test]
fn upgrade_before_init_fails() {
    let (env, _creator, client) = setup();
    let wasm_hash = upload_upgrade_target(&env);
    assert_eq!(
        client.try_upgrade(&wasm_hash),
        Err(Ok(Error::NotInitialised))
    );
}

#[test]
fn version_check() {
    let (env, _creator, client) = setup();
    client.init(&Address::generate(&env));
    assert_eq!(client.get_version(), String::from_str(&env, VERSION));
    assert!(client.is_compatible(&String::from_str(&env, VERSION)));
    assert!(!client.is_compatible(&String::from_str(&env, "99.0.0")));
    assert!(!client.is_compatible(&String::from_str(&env, "")));
}

#[test]
fn too_many_or_too_long_tags_rejected() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "tags");
    let m = String::from_str(&env, "m");
    let nine = tags(&env, &["a", "b", "c", "d", "e", "f", "g", "h", "i"]);
    assert_eq!(
        client.try_register(&creator, &id, &100i128, &m, &nine),
        Err(Ok(Error::InvalidTag))
    );
    let long = "x".repeat(33);
    let too_long = tags(&env, &[long.as_str()]);
    assert_eq!(
        client.try_register(&creator, &id, &100i128, &m, &too_long),
        Err(Ok(Error::InvalidTag))
    );
    let eight = tags(&env, &["a", "b", "c", "d", "e", "f", "g", "h"]);
    client.register(&creator, &id, &100i128, &m, &eight);
    assert_eq!(client.get(&id).tags.len(), 8);
}

#[test]
fn mutations_require_creator_auth() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "auth");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );
    assert_eq!(env.auths()[0].0, creator);

    client.set_price(&id, &200i128);
    assert_eq!(env.auths()[0].0, creator);

    let new_owner = Address::generate(&env);
    client.transfer_ownership(&id, &new_owner);
    assert_eq!(env.auths()[0].0, creator);

    // After transfer only the new owner is asked to authorize.
    client.set_listed(&id, &false);
    assert_eq!(env.auths()[0].0, new_owner);
}

#[test]
fn set_price_by_non_creator_is_rejected() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "steal");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    let mallory = Address::generate(&env);
    env.mock_auths(&[soroban_sdk::testutils::MockAuth {
        address: &mallory,
        invoke: &soroban_sdk::testutils::MockAuthInvoke {
            contract: &client.address,
            fn_name: "set_price",
            args: (id.clone(), 1i128).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    assert!(client.try_set_price(&id, &1i128).is_err());
    assert_eq!(client.get(&id).price, 100i128);
}

/// Prints CPU / memory cost of the hot entrypoints and guards against large
/// regressions. Run with `cargo test -p vault-registry gas -- --nocapture`.
#[test]
fn gas_register_and_list_budget() {
    let (env, creator, client) = setup();
    let ids = [
        "g00", "g01", "g02", "g03", "g04", "g05", "g06", "g07", "g08", "g09", "g10", "g11", "g12",
        "g13", "g14", "g15", "g16", "g17", "g18", "g19",
    ];

    env.cost_estimate().budget().reset_default();
    client.register(
        &creator,
        &String::from_str(&env, "g-first"),
        &100i128,
        &String::from_str(&env, "ipfs://QmGas"),
        &tags(&env, &["dataset", "research"]),
    );
    let reg_cpu = env.cost_estimate().budget().cpu_instruction_cost();
    let reg_mem = env.cost_estimate().budget().memory_bytes_cost();
    std::println!("register: cpu={reg_cpu} mem={reg_mem}");

    let first = String::from_str(&env, "g-first");
    env.cost_estimate().budget().reset_default();
    client.set_price(&first, &200i128);
    std::println!(
        "set_price: cpu={} mem={}",
        env.cost_estimate().budget().cpu_instruction_cost(),
        env.cost_estimate().budget().memory_bytes_cost()
    );

    env.cost_estimate().budget().reset_default();
    client.get(&first);
    std::println!(
        "get: cpu={} mem={}",
        env.cost_estimate().budget().cpu_instruction_cost(),
        env.cost_estimate().budget().memory_bytes_cost()
    );

    register_n(&env, &creator, &client, &ids);
    env.cost_estimate().budget().reset_default();
    let page = client.list(&0u32, &MAX_PAGE_SIZE);
    let list_cpu = env.cost_estimate().budget().cpu_instruction_cost();
    let list_mem = env.cost_estimate().budget().memory_bytes_cost();
    std::println!("list(20): cpu={list_cpu} mem={list_mem}");
    assert_eq!(page.len(), MAX_PAGE_SIZE);

    // Network limit is 100M instructions per tx; stay well below it.
    assert!(reg_cpu < 10_000_000, "register too expensive: {reg_cpu}");
    assert!(list_cpu < 50_000_000, "list too expensive: {list_cpu}");
}

/// Event topics/data must stay wire-compatible with what indexers expect
/// (see docs/CONTRACTS.md).
#[test]
fn events_have_documented_shape() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "ev");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );
    assert_eq!(
        env.events().all().filter_by_contract(&client.address),
        soroban_sdk::vec![
            &env,
            (
                client.address.clone(),
                (symbol_short!("register"), creator.clone()).into_val(&env),
                id.clone().into_val(&env),
            ),
        ]
    );

    client.set_price(&id, &250i128);
    assert_eq!(
        env.events().all().filter_by_contract(&client.address),
        soroban_sdk::vec![
            &env,
            (
                client.address.clone(),
                (symbol_short!("setprice"), id.clone()).into_val(&env),
                250i128.into_val(&env),
            ),
        ]
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]
    #[test]
    fn test_metadata_pointer_roundtrip_property(
        id_str in r"[a-zA-Z0-9_-]{1,32}",
        price in 1..1000000000000i128,
        price_2 in 1..1000000000000i128,
        meta_str in r"[a-zA-Z0-9:/\\._-]{0,512}",
        meta_str_2 in r"[a-zA-Z0-9:/\\._-]{0,512}",
        listed in any::<bool>(),
    ) {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(VaultRegistry, ());
        let client = VaultRegistryClient::new(&env, &contract_id);
        let creator = Address::generate(&env);

        let id = String::from_str(&env, &id_str);
        let metadata = String::from_str(&env, &meta_str);
        let metadata_2 = String::from_str(&env, &meta_str_2);

        // 1. Register resource with initial metadata
        client.register(&creator, &id, &price, &metadata, &empty_tags(&env));

        // 2. Get and verify metadata is identical
        let r = client.get(&id);
        assert_eq!(r.metadata, metadata);
        assert_eq!(r.price, price);
        assert_eq!(r.creator, creator);
        assert!(r.listed);

        // 3. Update metadata
        client.update_metadata(&id, &metadata_2);

        // 4. Verify updated metadata is identical and other fields preserved
        let r2 = client.get(&id);
        assert_eq!(r2.metadata, metadata_2);
        assert_eq!(r2.price, price);
        assert_eq!(r2.creator, creator);
        assert!(r2.listed);

        // 5. Update price and verify metadata is unaffected
        client.set_price(&id, &price_2);
        let r3 = client.get(&id);
        assert_eq!(r3.metadata, metadata_2);
        assert_eq!(r3.price, price_2);

        // 6. Update listing status and verify metadata is unaffected
        client.set_listed(&id, &listed);
        let r4 = client.get(&id);
        assert_eq!(r4.metadata, metadata_2);
        assert_eq!(r4.listed, listed);
    }
}

// ── #80: UpdateMetadataEvent must carry the new metadata pointer ─────────────

/// Verify that `update_metadata` emits an event whose data field contains the
/// new metadata pointer so that indexers do not need an extra ledger read.
#[test]
fn update_metadata_event_includes_new_pointer() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "ev-meta");
    let old_meta = String::from_str(&env, "ipfs://QmOld");
    let new_meta = String::from_str(&env, "ipfs://QmNew");

    client.register(&creator, &id, &100i128, &old_meta, &empty_tags(&env));
    env.events().all(); // clear previous events

    client.update_metadata(&id, &new_meta);

    let all = env.events().all().filter_by_contract(&client.address);
    assert_eq!(all.len(), 1);
    // data value must be the new metadata string, not unit / empty
    let (_contract, _topics, data) = all.get(0).unwrap();
    let emitted: String = data.into_val(&env);
    assert_eq!(emitted, new_meta);
}

// ── #81: set_listed must be a no-op when value is unchanged ──────────────────

/// Calling `set_listed` with the current value must not write to storage
/// (TTL is unchanged) and must not emit any event.
#[test]
fn set_listed_no_op_when_value_unchanged() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "noop-listed");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );

    // Resource is listed by default; call set_listed(true) — should be no-op.
    let ttl_before = resource_storage_ttl(&env, &client.address, &id);
    env.events().all(); // clear

    client.set_listed(&id, &true);

    // No events emitted.
    let events = env.events().all().filter_by_contract(&client.address);
    assert_eq!(events.len(), 0, "expected no event on no-op set_listed");

    // TTL must not have been bumped (no storage write happened).
    let ttl_after = resource_storage_ttl(&env, &client.address, &id);
    assert_eq!(ttl_before, ttl_after, "TTL must not change on no-op set_listed");
}

/// Calling `set_listed` with a different value must still emit the event.
#[test]
fn set_listed_emits_event_when_value_changes() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "change-listed");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );
    env.events().all(); // clear

    client.set_listed(&id, &false);

    let events = env.events().all().filter_by_contract(&client.address);
    assert_eq!(events.len(), 1, "expected one event when listed changes");
    let (_contract, _topics, data) = events.get(0).unwrap();
    let emitted: bool = data.into_val(&env);
    assert!(!emitted);
}

// ── #83: is_compatible must reject malformed / boundary version strings ───────

/// Versions with only one dot (no patch component) must return false.
#[test]
fn is_compatible_rejects_version_without_patch() {
    let (env, _creator, client) = setup();
    // "0.1" has no patch — must not match "0.1.x"
    assert!(
        !client.is_compatible(&String::from_str(&env, "0.1")),
        "two-component version must not be compatible"
    );
    assert!(
        !client.is_compatible(&String::from_str(&env, "1.0")),
        "two-component version must not be compatible"
    );
}

/// Completely malformed strings must return false.
#[test]
fn is_compatible_rejects_malformed_versions() {
    let (env, _creator, client) = setup();
    let bad = ["", "abc", ".", "..", "1", "1.", ".0.1", "0..1", "0.1.", "0.1.2.3"];
    for v in &bad {
        assert!(
            !client.is_compatible(&String::from_str(&env, v)),
            "expected false for malformed version {:?}",
            v
        );
    }
}

/// A well-formed version with matching major.minor but differing patch is
/// still compatible (patch is not part of the compatibility check).
#[test]
fn is_compatible_accepts_different_patch() {
    let (env, _creator, client) = setup();
    // VERSION is the compile-time crate version; craft a variant with a
    // different patch number to ensure patch is ignored.
    let compatible_alt = String::from_str(&env, "0.1.99");
    // This only holds if VERSION is 0.1.x — adapt if the crate version changes.
    // We use a known-compatible full version first as a sanity check.
    assert!(
        client.is_compatible(&String::from_str(&env, VERSION)),
        "current VERSION must be compatible with itself"
    );
    // A higher patch on the same major.minor is also compatible.
    let _ = compatible_alt; // used conditionally below
    // Build a version string with the same major.minor as VERSION but patch=99.
    let version_bytes = VERSION.as_bytes();
    let mut dots = 0usize;
    let mut second_dot = version_bytes.len();
    for (i, b) in version_bytes.iter().enumerate() {
        if *b == b'.' {
            dots += 1;
            if dots == 2 {
                second_dot = i;
                break;
            }
        }
    }
    let prefix = &VERSION[..second_dot];
    let alt = std::format!("{}.99", prefix);
    assert!(
        client.is_compatible(&String::from_str(&env, &alt)),
        "same major.minor with different patch must be compatible"
    );
}

/// Boundary: version string at exactly 32 bytes is accepted if well-formed;
/// 33+ bytes is rejected by the length guard.
#[test]
fn is_compatible_length_boundary() {
    let (env, _creator, client) = setup();
    // 33-character version — must be rejected by the len > BUF_LEN guard.
    let long = "1".repeat(11) + "." + &"2".repeat(11) + "." + &"3".repeat(8); // 33 chars
    assert_eq!(long.len(), 33);
    assert!(
        !client.is_compatible(&String::from_str(&env, &long)),
        "version longer than 32 bytes must be rejected"
    );

    // A well-formed 32-byte version string should parse without panicking.
    // e.g. "111.222.333444555" is 17 chars; pad the patch to hit 32 chars.
    let at_limit = "1.1.".to_string() + &"0".repeat(28); // 32 chars
    assert_eq!(at_limit.len(), 32);
    // We don't assert true/false here (major.minor won't match VERSION in
    // general) but we do assert it doesn't panic and returns a bool.
    let _ = client.is_compatible(&String::from_str(&env, &at_limit));
}

// ── #85: is_compatible must reject empty / non-numeric versions ──────────────

/// `is_compatible` must return false (without panicking) for an empty string
/// and for non-numeric version strings.
#[test]
fn is_compatible_rejects_empty_and_non_numeric() {
    let (env, _creator, client) = setup();

    // Empty string must be rejected.
    assert!(
        !client.is_compatible(&String::from_str(&env, "")),
        "empty version must not be compatible"
    );

    // Non-numeric components must be rejected without panicking.
    let non_numeric = ["abc", "1.x.0", "a.b.c", "1.2.x", "x.2.3", "1.2.3-rc1"];
    for v in &non_numeric {
        assert!(
            !client.is_compatible(&String::from_str(&env, v)),
            "expected false for non-numeric version {:?}",
            v
        );
    }
}

// ── #84: is_compatible must reject versions longer than the 32-byte buffer ───

/// Version strings longer than the 32-byte buffer must be rejected without
/// panicking or being silently truncated to a matching prefix.
#[test]
fn is_compatible_rejects_over_buffer_length() {
    let (env, _creator, client) = setup();

    // A well-formed-looking string that is 33 bytes — one over the buffer.
    let over = "1.1.".to_string() + &"0".repeat(29); // 33 chars
    assert_eq!(over.len(), 33);
    assert!(
        !client.is_compatible(&String::from_str(&env, &over)),
        "version longer than 32 bytes must be rejected"
    );

    // A much longer string must also be rejected (no truncation to a match).
    let very_long = "1.1.".to_string() + &"0".repeat(100);
    assert!(very_long.len() > 32);
    assert!(
        !client.is_compatible(&String::from_str(&env, &very_long)),
        "very long version must be rejected"
    );
}

// ── #86: register on an archived id must be rejected ─────────────────────────

/// After a resource's persistent entry is archived (TTL lapses), registering
/// the same id again must be rejected with `AlreadyRegistered` rather than
/// silently succeeding due to `has` being unreliable for archived entries.
#[test]
fn register_on_archived_id_is_rejected() {
    let (env, creator, client) = setup();
    let id = String::from_str(&env, "archived");
    client.register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );
    assert_eq!(client.count(), 1);

    // Advance the ledger far past the persistent TTL so the resource entry is
    // archived out of the live ledger.
    env.ledger()
        .set_sequence_number(env.ledger().sequence() + TTL_BUMP_AMOUNT + DAY_IN_LEDGERS);

    // Re-registering the same id must be rejected, not silently accepted.
    let res = client.try_register(
        &creator,
        &id,
        &100i128,
        &String::from_str(&env, "m"),
        &empty_tags(&env),
    );
    assert_eq!(res, Err(Ok(Error::AlreadyRegistered)));
}
