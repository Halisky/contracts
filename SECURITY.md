# Security Policy

Please **do not** open a public issue for vulnerabilities.

Report privately via [GitHub Security Advisories](https://github.com/SynapsVault/contracts/security/advisories/new).
Include the affected contract, entrypoint, and a reproduction (a failing unit
test is ideal). We aim to acknowledge reports within 3 business days.

Past findings and their resolutions are tracked in [`SECURITY_AUDIT.md`](SECURITY_AUDIT.md).

## Admin Key Threat Model and Required Deployment Setup

### Threat model

The contracts in this repository (`vault-registry`, `access-lease` and
`subscription`) store a single `admin` address that is configured at
deployment time. The admin is authorized to perform privileged operations
such as upgrading contract code, changing configuration, pausing or
unpausing the contracts, and managing roles.

The contracts enforce only that the caller of an admin-only entrypoint is
the configured admin address. They do **not** enforce any on-chain
multisig or threshold requirement. Consequently:

- A single compromised admin key grants an attacker full control over the
  contract, including the ability to upgrade it to arbitrary code.
- A single lost admin key can permanently lock out administrative
  operations, since there is no on-chain recovery path.
- The security of the deployment therefore depends entirely on how the
  admin account is controlled off-chain.

This is a deliberate design decision: implementing threshold approvals
on-chain would add significant complexity and gas cost to every admin
operation. The mitigation is to require that the admin address is itself a
multisig/threshold-controlled account, as described below. This is a
**deployment requirement**, not a contract-enforced guarantee.

### Required deployment setup

The `admin` address passed to each contract's initialization/constructor
MUST be a multisig or threshold-controlled account, never a single
signing key. On Stellar this is typically a Stellar account configured
with multiple signers and a medium/high threshold, for example:

- A minimum of 3 signers held by distinct individuals or independent
  infrastructure, with a threshold of at least 2-of-3 for admin
  operations.
- Signer keys stored in hardware wallets or an equivalent hardened key
  management system; no signer key should exist only in plaintext on a
  general-purpose machine.
- The account's `high` threshold set so that changing the signer set or
  thresholds itself requires the full threshold, preventing a single
  signer from unilaterally reconfiguring the account.

Deployments that configure a single-key admin are considered
non-compliant with this policy and should not be used in production.

### Key rotation and signer management

- **Routine rotation:** Rotate signer keys on a defined schedule and
  whenever a signer leaves the organization or their key may have been
  exposed. Rotation is performed on the admin account's signer set, not
  on the contracts; the contract `admin` address does not change.
- **Changing the admin address:** If the admin address itself must
  change, use the contract's admin-transfer entrypoint (where available)
  and execute the change through the multisig so that it is approved by
  the required threshold. Verify the new address is itself a compliant
  multisig account before transferring control.
- **Removing a signer:** Remove a signer's key from the admin account as
  soon as their access is revoked. Do not rely on the signer simply not
  using their key.
- **Adding a signer:** Add new signers only after their key material has
  been generated and stored according to the requirements above, and
  confirm the resulting threshold still meets the minimum.
- **Lost key:** If a signer key is lost, rotate it out of the signer set
  using the remaining signers. Ensure the threshold is still satisfiable
  at all times; never reduce the signer set below the minimum required
  threshold.

### Operational procedures

- Maintain an up-to-date record of the current signer set, thresholds and
  the individuals responsible for each signer.
- Require out-of-band verification (e.g. a second communication channel)
  of the exact transaction contents before each signer approves an admin
  operation, especially upgrades.
- Test admin operations, including signer rotation and admin transfer, on
  a testnet deployment before performing them on mainnet.
- Monitor admin-account activity and alert on any unexpected signer
  changes or admin operations.
- Document and rehearse the recovery procedure for a lost signer and for
  a suspected key compromise, including pausing the contracts if
  supported.

### Scope

This section documents the operational mitigation for the single-key
admin design. It does not change contract runtime behavior; the contracts
continue to authorize admin operations based solely on the configured
admin address.