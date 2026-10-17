<div align="center">
  <h1>⬡ SynapsVault — Contracts</h1>
  <p><strong>Soroban smart contracts on the Stellar network</strong></p>
  <p>
    <a href="https://github.com/SynapsVault/contracts/actions"><img src="https://github.com/SynapsVault/contracts/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
    <img src="https://img.shields.io/badge/soroban--sdk-v28-7D00FF" alt="soroban-sdk v28">
    <img src="https://img.shields.io/badge/Rust-1.91%2B-orange" alt="Rust">
    <img src="https://img.shields.io/badge/network-Stellar-blue" alt="Stellar">
    <img src="https://img.shields.io/badge/license-MIT-green" alt="MIT">
  </p>
</div>

---

## Contracts

### `vault-registry`

The on-chain registry for SynapsVault resources. Stores creator address, price (in USDC stroops), metadata pointer (IPFS CID / content hash), tags, and listing status.

Only the registered creator can mutate their resource (`require_auth`). Ownership can be transferred. Supports paginated listing and metadata updates.

**Functions**

| Function | Auth | Description |
|---|---|---|
| `init(admin)` | — (once) | Set upgrade admin |
| `register(creator, id, price, metadata, tags)` | Creator | Register a resource |
| `set_price` / `update_metadata` / `set_tags` | Creator | Mutate a resource |
| `set_listed` / `delist` | Creator | Toggle discoverability |
| `propose_transfer(id, new_creator)` | Creator | Propose ownership transfer (step 1 of 2) |
| `accept_transfer(id, new_creator)` | New creator | Accept proposed transfer (step 2 of 2) |
| `cancel_transfer(id)` | Creator | Cancel a pending transfer |
| `get` / `get_owner` / `exists` / `count` | None | Reads |
| `list(start, limit)` | None | Paginated, max 20 per page |
| `upgrade(new_wasm_hash)` | Admin | Upgrade WASM |

See [`docs/CONTRACTS.md`](docs/CONTRACTS.md#vault-registry) for the full interface reference.

### `access-lease` ⭐

Time-limited on-chain access grants. The backend issues a `Lease` struct with a specific `expires_at` ledger sequence. Any party can verify access with a single read — no need to trust the backend.

**Functions**

| Function | Auth | Description |
|---|---|---|
| `init(admin)` | — (once) | Set admin at deploy |
| `set_admin(new_admin)` | Admin | Rotate admin |
| `grant_lease(resource_id, buyer, duration_ledgers)` | Admin | Issue a timed lease |
| `extend_lease(resource_id, buyer, extra_ledgers)` | Admin | Extend lease |
| `is_valid(resource_id, buyer)` | None | Check active status |
| `get_lease(resource_id, buyer)` | None | Full lease struct |
| `revoke_lease(resource_id, buyer)` | Admin | Revoke on refund / ToS |

See [`docs/CONTRACTS.md`](docs/CONTRACTS.md#access-lease) for the full interface reference.

### `subscription` ⭐

Recurring 30-day subscription plans. Publishers define plans; the backend subscribes buyers and renews each cycle. Subscribers can self-cancel with access through the period end.

**Functions**

| Function | Auth | Description |
|---|---|---|
| `init(admin)` | — (once) | Set admin at deploy |
| `set_admin(new_admin)` | Admin | Rotate admin |
| `create_plan(publisher, plan_id, price_per_cycle)` | Publisher | Define a plan (ids are unique) |
| `get_plan(plan_id)` | None | Read a plan |
| `deactivate_plan(plan_id)` | Publisher | Close plan to new subs |
| `subscribe(plan_id, subscriber)` | Admin | Start subscription |
| `renew(plan_id, subscriber)` | Admin | Add one billing cycle |
| `cancel(plan_id, subscriber)` | Subscriber | Self-cancel (access runs to period end) |
| `is_active(plan_id, subscriber)` | None | Check active status |
| `get_subscription(plan_id, subscriber)` | None | Full sub struct |

See [`docs/CONTRACTS.md`](docs/CONTRACTS.md#subscription) for the full interface reference.

---

## Documentation

Detailed contract documentation lives in [`docs/CONTRACTS.md`](docs/CONTRACTS.md). It covers the full public interface, data structures, storage layout, and error codes for each contract.

> **Note:** Keep documentation in sync with code. When you change a contract's public interface, storage layout, or error codes, update `docs/CONTRACTS.md` in the same pull request.

---

## Quick start

```bash
# Prerequisites: Rust 1.91+ (rust-toolchain.toml installs the wasm32v1-none target)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Stellar CLI v25.2+ (required to build contracts) — see https://developers.stellar.org/docs/tools/cli
cargo install --locked stellar-cli

# Clone
git clone https://github.com/SynapsVault/contracts SynapsVault-contracts
cd SynapsVault-contracts

# Run tests, lints and docs (same as CI)
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Build WASM
stellar contract build
# Output: target/wasm32v1-none/release/*.wasm
```

> `soroban-sdk` 28 only builds contract WASM through `stellar contract build` (stellar-cli v25.2+), which targets `wasm32v1-none` and strips unused `contract-spec` entries. Plain `cargo build --target ...` fails on purpose.

## Deploy to Stellar testnet

```bash
stellar network add testnet \
  --rpc-url https://soroban-testnet.stellar.org \
  --network-passphrase "Test SDF Network ; September 2015"

stellar keys generate --global deployer
stellar keys fund deployer --network testnet

for PKG in vault_registry access_lease subscription; do
  ID=$(stellar contract deploy \
    --wasm target/wasm32v1-none/release/$PKG.wasm \
    --source-account deployer --network testnet)
  # init is one-time and cannot be repeated — run it right after deploy.
  stellar contract invoke --id "$ID" --source-account deployer --network testnet \
    -- init --admin <PLATFORM_ADMIN_ADDRESS>
  echo "$PKG=$ID"
done
```

## Deploy to Stellar mainnet

Mainnet deploys use the **Mainnet Deploy** workflow (manual dispatch, requires typing `deploy-mainnet` and approval on the `mainnet` environment). It runs the test suite, builds, deploys and initialises all three contracts and commits the IDs to `deployed/mainnet-contract-ids.env`. Never reuse the testnet `deployer` key on mainnet.

### Mainnet checklist

- [ ] All tests green on `main` and the release tag is signed.
- [ ] WASM hashes are recorded (see [Upgrades](#upgrades)).
- [ ] Contract IDs and admin address are written to the backend's mainnet config.
- [ ] `init` is called exactly once per contract — a second call fails with `AlreadyInitialised`.
- [ ] A post-deploy smoke test verifies `is_valid` / `is_active` reads.

## Upgrades

Contracts are upgradeable through an admin-gated `upgrade` entrypoint. To deploy a new WASM without losing storage, you must upload the new WASM to get its hash, and invoke the `upgrade` function on the existing contract rather than deploying a brand new contract instance. Storage is preserved, so existing resources, leases, plans and subscriptions remain readable.

```bash
# 1. Build
stellar contract build

# 2. Upload the WASM blob and capture its hash
HASH=$(stellar contract upload \
  --wasm target/wasm32v1-none/release/access_lease.wasm \
  --source-account mainnet-deployer --network mainnet)

# 3. Point the live contract at the new hash (admin auth required)
stellar contract invoke --id <C_LEASE_ID> \
  --source-account <ADMIN> --network mainnet \
  -- upgrade --new_wasm_hash "$HASH"
```

### Migration notes

- `upgrade` does not run data migrations. A change to a stored struct's layout needs a follow-up admin migration.
- Always upgrade on testnet first, verify reads against existing data, then repeat the exact same hash on mainnet.
- Keep the previous WASM hash recorded so a rollback is a single `upgrade` call.

## CI/CD

```
PR / push to dev, main ──► fmt + clippy (-D warnings) + rustdoc (-D warnings)
                           tests (all contracts) + gas report
                           WASM build (stellar contract build) + 64 KiB size check
Merge to main          ──► build + deploy + init on Stellar testnet
Manual dispatch        ──► mainnet deploy (confirmation + environment approval)
```

**GitHub secrets:**

| Secret | Used by |
|---|---|
| `DEPLOYER_SECRET` | Testnet deploy (deploy is skipped if unset) |
| `BACKEND_PUBLIC` | Testnet admin address passed to `init` |
| `PUBLISHER1_PUBLIC`, `PUBLISHER2_PUBLIC` | Funded via friendbot on testnet |
| `MAINNET_DEPLOYER_SECRET` | Mainnet deploy (`mainnet` environment) |
| `MAINNET_ADMIN_PUBLIC` | Mainnet admin address passed to `init` |

## Architecture

```
Stellar Ledger
│
├── vault-registry     (C_REGISTRY_ID)
│   └── Resource { id, creator, price, metadata, tags, listed }
│
├── access-lease       (C_LEASE_ID)
│   └── Lease { resource_id, buyer, granted_at, expires_at, duration_ledgers }
│
└── subscription       (C_SUB_ID)
    ├── Plan { plan_id, publisher, price_per_cycle, active }
    └── Subscription { plan_id, subscriber, started_at, current_period_end, cancelled, total_renewals }
```

## Storage TTLs

Entries are TTL-bumped on every write so actively managed data is never archived:

| Contract | Persistent bump |
|---|---|
| `vault-registry` | 30 days |
| `access-lease` | 90 days |
| `subscription` | 365 days |

## Security

- Every mutating call is gated by `require_auth()` on the appropriate authority.
- `init` is one-time on every contract; the admin can only be changed by the current admin via `set_admin`.
- The admin role is held by the backend's platform wallet — not a user wallet.
- `is_valid` and `is_active` are fully permissionless — any frontend or smart contract can verify access without relying on SynapsVault's API.
- All ledger-sequence arithmetic is checked; overflow returns an error instead of wrapping.
- Every state change emits an event for off-chain indexing and auditing.

See [`SECURITY_AUDIT.md`](SECURITY_AUDIT.md) for findings and their status.

## Repo siblings

| Repo | Description |
|---|---|
| `SynapsVault-frontend` | React UI |
| `SynapsVault-backend` | Express API |

## License

MIT © 2025 Busiii-adetiba

## Handoff notes

- #42: `[access-lease]` `set_admin` is single-step; a mistyped address permanently locks admin — propose two-step rotation
- #44: `[access-lease]` `is_compatible` uses exact string match here but major/minor match in `vault-registry` — unify semantics
- #50: `[subscription]` Publishers cannot change a plan's price after creation
- #51: `[subscription]` Plan ownership cannot be transferred (`vault-registry` supports this)
- #53: `[subscription]` Resubscribing a cancelled-but-paid subscription adds a full cycle on top of the remaining time; confirm thi...