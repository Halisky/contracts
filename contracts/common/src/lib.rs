#![no_std]

use soroban_sdk::{contracterror, Address, Env};

/// Number of seconds in a day.
pub const DAY: u64 = 86_400;

/// Canonical bump amount used for persistent storage entries.
pub const BUMP: u32 = 100_000;

/// Canonical bump threshold used for persistent storage entries.
pub const BUMP_THRESH: u32 = 1_000_000;

/// Canonical contract version reported by `get_version`.
pub const VERSION: u32 = 1;

/// Errors returned by the shared helpers.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum CommonError {
    /// The caller is not the stored admin.
    Unauthorized = 1,
}

/// Require that `admin` has authorized the current invocation.
///
/// Panics with [`CommonError::Unauthorized`] when the stored admin has not
/// authorized the call. Callers are expected to have already resolved the
/// stored admin address and pass it in.
pub fn require_admin(env: &Env, admin: &Address) {
    admin.require_auth();
    let _ = env;
}

/// Return the canonical contract version.
pub fn get_version(_env: &Env) -> u32 {
    VERSION
}

/// Return whether `version` is compatible with the canonical version.
///
/// A version is compatible when it is greater than zero and no newer than
/// the canonical [`VERSION`].
pub fn is_compatible(version: u32) -> bool {
    version > 0 && version <= VERSION
}