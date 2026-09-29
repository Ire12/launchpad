#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    BytesN, Env, Map, Vec,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Typed contract errors for the vesting contract.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum VestingError {
    /// `initialize` was called on a contract that is already initialized.
    AlreadyInitialized = 1,
    /// Operation attempted before `initialize` was called.
    NotInitialized = 2,
    /// The vesting contract is paused.
    Paused = 3,
    /// Amount is zero or negative where a positive value is required.
    InvalidAmount = 4,
    /// `end_ledger` is not strictly after `cliff_ledger`.
    InvalidLedgerRange = 5,
    /// `accept_admin` was called with no pending proposal.
    NoPendingAdmin = 6,
    /// Operation attempted on a revoked vesting schedule.
    ScheduleRevoked = 7,
    /// Schedule has already been revoked.
    AlreadyRevoked = 8,
    /// `release` was called but no vested tokens are available.
    NothingToRelease = 9,
    /// No schedule found for recipient.
    ScheduleNotFound = 10,
    /// Schedule index is out of bounds for recipient.
    ScheduleIndexOutOfBounds = 11,
    /// Batch schedules list is empty.
    BatchEmpty = 12,
    /// Batch schedules size exceeds maximum of 50.
    BatchTooLarge = 13,
    /// `extend_cliff` called after the cliff ledger has passed.
    CliffPassed = 14,
    /// New cliff ledger is not strictly later than the current cliff ledger.
    CliffNotExtended = 15,
    /// New cliff ledger is not strictly before the end ledger.
    CliffAfterEnd = 16,
    /// `prune_recipient` called for a recipient that is not tracked.
    RecipientNotTracked = 17,
    /// A recipient already holds the maximum number of stored schedules.
    TooManySchedules = 18,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Largest schedule amount that can be linearly interpolated without an
/// `i128` overflow for any valid pair of `u32` ledger sequence numbers.
///
/// `_vested_amount` multiplies this amount by the elapsed ledger count. A
/// schedule can span up to `u32::MAX` ledgers, so keeping the amount at or
/// below this ceiling makes that intermediate multiplication safe.
const MAX_VESTING_AMOUNT: i128 = i128::MAX / u32::MAX as i128;

/// Desired lifetime for the ledger entries this contract keeps alive:
/// about a year, assuming Stellar's ~5s ledger close time.
///
/// 365 days * 24h * 60m * 60s / 5s-per-ledger = 6,307,200 ledgers.
///
/// This is only a *request*. The effective window is whatever the network
/// allows — `env.storage().max_ttl()`, read at call time — and every
/// `extend_ttl` site clamps to it. On testnet and mainnet today
/// `max_entry_ttl` is 3,110,400 ledgers, so entries actually live **about
/// 180 days, not a year**. Holders who need their balance to outlive that
/// must interact with the contract at least once per window.
///
/// Deliberately not compared against a hardcoded ceiling: the previous
/// constant here (6,312,000) was the soroban-sdk *test harness* default
/// (`soroban-sdk/src/env.rs`), not a network value, so the clamp it fed
/// could never fire. The test environment still reports 6,312,000, which is
/// why no test in this file asserts a specific network figure.
const TTL_LEDGERS: u32 = 365 * 24 * 60 * 60 / 5;

/// Largest page a caller may request from a paginated getter.
///
/// `limit` is caller-supplied, so with no ceiling a single read can ask the
/// host to build and serialise an arbitrarily large vector — burning the
/// invocation's compute budget and resource fee on a read that returns
/// nothing the caller did not already have (issue #469). Stellar's guidance
/// is that a function which reads storage in a loop must bound its
/// iterations, and that a caller-chosen page size is clamped in-contract
/// rather than trusted.
///
/// 100 is comfortably above the page size the launchpad frontend actually
/// asks for (20, see `useVestingDashboard`), so the clamp is invisible to it
/// while leaving other clients a generous window.
const MAX_PAGE: u32 = 100;

/// Most schedules a single recipient may hold at once.
///
/// `total_vested`, `total_released`, `total_releasable`, `get_all_schedules`
/// and `release_all` each walk a recipient's entire schedule range, and that
/// range is bounded only by how many times the admin is willing to call
/// `create_schedule` / `create_schedules_batch`. Past a few hundred entries
/// those getters exceed the compute budget and become permanently uncallable
/// — and they break *before* `release` does, so `total_releasable` and
/// `release_all` go down together: a holder can neither claim nor find out
/// what they are owed. A single well-meaning second grant for a recipient who
/// already has fifty is enough to start down that path (issue #466).
///
/// Enforcing the bound at the writer is what makes those five loops safe by
/// construction, rather than each reader defending itself separately and any
/// one of them being missed.
///
/// 50 matches `create_schedules_batch`'s own per-call cap, so a recipient can
/// be brought to the ceiling by one batch and then not exceeded, and it sits
/// ~2 orders of magnitude below where the aggregate loops start failing.
///
/// Note this is a lifetime cap, not a concurrent one: `prune_recipient` only
/// clears the recipient's *enumeration* slot and never decrements
/// `ScheduleCount`, so it does not free capacity. Revoking a schedule frees
/// the tokens but not the slot.
const MAX_SCHEDULES_PER_RECIPIENT: u32 = 50;

// ---------------------------------------------------------------------------
// Storage types
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[contracttype]
pub enum DataKey {
    Admin,
    PendingAdmin,
    Locked,
    TokenContract,
    IsPaused,
    TotalCommitted,
    Schedule(Address, u32),
    ScheduleCount(Address),
    RecipientCount,
    RecipientAt(u32),
}

#[derive(Clone, Debug)]
#[contracttype]
pub struct VestingSchedule {
    pub recipient: Address,
    pub total_amount: i128,
    pub cliff_ledger: u32,
    pub end_ledger: u32,
    pub released: i128,
    pub revoked: bool,
}

#[derive(Clone, Debug)]
#[contracttype]
pub struct ScheduleInput {
    pub recipient: Address,
    pub total_amount: i128,
    pub cliff_ledger: u32,
    pub end_ledger: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[contracttype]
pub struct Solvency {
    pub token_balance: i128,
    pub total_committed: i128,
    pub solvent: bool,
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

/// Vesting Contract — cliff + linear unlock schedules.
///
/// Contributor issues layered on top:
/// - #3  revoke() — admin reclaims unvested tokens
/// - #5  structured events audit
/// - #149 pause/unpause circuit breaker
#[contract]
pub struct VestingContract;

#[contractimpl]
impl VestingContract {
    // ── Initialization ──────────────────────────────────────────────────

    /// Set the admin and the token contract this vesting module manages.
    pub fn initialize(env: Env, admin: Address, token_contract: Address) {
        admin.require_auth();
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, VestingError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::TokenContract, &token_contract);

        env.events()
            .publish((symbol_short!("init"),), (admin, token_contract));
    }

    // ── Admin actions ───────────────────────────────────────────────────

    /// Propose a new admin. Must be called by the current admin.
    /// The new admin must call `accept_admin` to finalize the transfer.
    pub fn propose_admin(env: Env, new_admin: Address) {
        Self::_require_admin(&env);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        env.events()
            .publish((symbol_short!("prop_adm"),), new_admin);
    }

    /// Accept the admin role. Must be called by the pending admin.
    pub fn accept_admin(env: Env) {
        Self::_require_not_locked(&env);
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or_else(|| panic_with_error!(&env, VestingError::NoPendingAdmin));
        pending.require_auth();
        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events().publish((symbol_short!("acc_adm"),), pending);
    }

    /// Cancel a proposed admin transfer. Must be called by the current admin.
    pub fn cancel_admin_proposal(env: Env) {
        Self::_require_admin(&env);
        env.storage().instance().remove(&DataKey::PendingAdmin);
    }

    /// Create a cliff + linear vesting schedule for `recipient`.
    ///
    /// `cliff_ledger` — ledger number when tokens start unlocking.
    /// `end_ledger`   — ledger number when 100 % is vested.
    ///
    /// This function atomically transfers `total_amount` tokens from the admin
    /// to this contract's address using transfer, ensuring the contract
    /// is properly funded in the same transaction.
    ///
    /// **Maximum schedules per recipient: `MAX_SCHEDULES_PER_RECIPIENT` (50).**
    /// Exceeding it fails with `TooManySchedules` rather than being stored,
    /// so the aggregate getters that walk a recipient's full schedule range
    /// stay inside the compute budget (issue #466).
    pub fn create_schedule(
        env: Env,
        recipient: Address,
        total_amount: i128,
        cliff_ledger: u32,
        end_ledger: u32,
    ) {
        Self::_check_paused(&env);
        let admin = Self::_require_admin(&env);

        Self::_validate_total_amount(total_amount);
        assert!(
            cliff_ledger >= env.ledger().sequence(),
            "cliff_ledger must not be in the past"
        );
        assert!(
            end_ledger > cliff_ledger,
            "end_ledger must be after cliff_ledger"
        );

        let schedule_index = Self::_schedule_count(&env, &recipient);
        if schedule_index >= MAX_SCHEDULES_PER_RECIPIENT {
            panic_with_error!(&env, VestingError::TooManySchedules);
        }
        let key = Self::_schedule_key(&recipient, schedule_index);

        // Get the token contract address
        let token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::TokenContract)
            .unwrap_or_else(|| panic_with_error!(&env, VestingError::NotInitialized));

        // Atomically transfer tokens from admin to this contract
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        token_client.transfer(&admin, &env.current_contract_address(), &total_amount);
        Self::_increase_total_committed(&env, total_amount);
        Self::_assert_solvent(&env, &token_addr);

        let schedule = VestingSchedule {
            recipient: recipient.clone(),
            total_amount,
            cliff_ledger,
            end_ledger,
            released: 0,
            revoked: false,
        };

        env.storage().persistent().set(&key, &schedule);

        // Extend TTL for the schedule to prevent archiving during vesting period
        let ttl_ledgers = Self::_ttl_ledgers(&env, end_ledger);
        Self::_extend_persistent_ttl(&env, &key, ttl_ledgers);
        Self::_set_schedule_count(&env, &recipient, schedule_index + 1, ttl_ledgers);

        // Track the recipient in the recipients list (only if new to this contract)
        if schedule_index == 0 {
            Self::_add_recipient(&env, &recipient);
        }

        env.events()
            .publish((symbol_short!("create"), recipient), total_amount);
    }

    /// Create multiple vesting schedules in a single transaction.
    ///
    /// Atomically transfers the sum of all `total_amount` values from the admin
    /// to this contract (Phase 2), then writes each schedule (Phase 3). If any
    /// step panics the entire transaction rolls back, including the token transfer.
    ///
    /// **Maximum batch size: 50 recipients.** Larger batches risk exceeding
    /// Soroban's per-transaction compute budget and will be rejected up front
    /// with a clear error rather than an opaque resource failure.
    ///
    /// **Maximum schedules per recipient: `MAX_SCHEDULES_PER_RECIPIENT` (50)**
    /// — the same invariant `create_schedule` enforces, and for the same
    /// reason (issue #466). The check below counts this batch's own earlier
    /// entries too, so a batch can bring a recipient *to* the ceiling but
    /// never past it. A batch that would push any recipient over fails
    /// entirely, like every other validation error here.
    pub fn create_schedules_batch(env: Env, schedules: Vec<ScheduleInput>) -> u32 {
        Self::_check_paused(&env);
        let admin = Self::_require_admin(&env);

        if schedules.is_empty() {
            panic_with_error!(&env, VestingError::BatchEmpty);
        }
        if schedules.len() > 50 {
            panic_with_error!(&env, VestingError::BatchTooLarge);
        }

        // Get the token contract address
        let token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::TokenContract)
            .unwrap_or_else(|| panic_with_error!(&env, VestingError::NotInitialized));

        let mut total_amount: i128 = 0;
        let mut assigned_indexes = Vec::new(&env);
        let mut next_indexes = Map::new(&env);

        // Phase 1: Validate all schedules and calculate total amount
        for i in 0..schedules.len() {
            let input = schedules.get(i).expect("index out of bounds");

            Self::_validate_total_amount(input.total_amount);
            assert!(
                input.cliff_ledger >= env.ledger().sequence(),
                "cliff_ledger must not be in the past"
            );
            assert!(
                input.end_ledger > input.cliff_ledger,
                "end_ledger must be after cliff_ledger"
            );

            let schedule_index = next_indexes
                .get(input.recipient.clone())
                .unwrap_or(Self::_schedule_count(&env, &input.recipient));
            // Bound the writer so the aggregate readers stay affordable. The
            // index is this recipient's *next* slot, so a recipient already
            // at the ceiling and a recipient who reaches it part-way through
            // this batch are rejected by the same comparison.
            if schedule_index >= MAX_SCHEDULES_PER_RECIPIENT {
                panic_with_error!(&env, VestingError::TooManySchedules);
            }
            next_indexes.set(input.recipient.clone(), schedule_index + 1);
            assigned_indexes.push_back(schedule_index);

            total_amount = total_amount
                .checked_add(input.total_amount)
                .expect("total amount overflow");
        }

        // Phase 2: Transfer total amount from admin to contract in one transaction
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        token_client.transfer(&admin, &env.current_contract_address(), &total_amount);
        Self::_increase_total_committed(&env, total_amount);
        Self::_assert_solvent(&env, &token_addr);

        // Phase 3: Create all schedules
        let mut created_count: u32 = 0;
        for i in 0..schedules.len() {
            let input = schedules.get(i).expect("index out of bounds");
            let schedule_index = assigned_indexes.get(i).expect("index out of bounds");

            let schedule = VestingSchedule {
                recipient: input.recipient.clone(),
                total_amount: input.total_amount,
                cliff_ledger: input.cliff_ledger,
                end_ledger: input.end_ledger,
                released: 0,
                revoked: false,
            };

            let key = Self::_schedule_key(&input.recipient, schedule_index);
            env.storage().persistent().set(&key, &schedule);

            // Extend TTL for the schedule (clamped to the network maximum, see
            // `_ttl_ledgers`)
            let ttl_ledgers = Self::_ttl_ledgers(&env, input.end_ledger);
            Self::_extend_persistent_ttl(&env, &key, ttl_ledgers);
            Self::_set_schedule_count(&env, &input.recipient, schedule_index + 1, ttl_ledgers);

            // Track the recipient in the recipients list (only if new to this contract)
            if schedule_index == 0 {
                Self::_add_recipient(&env, &input.recipient);
            }

            env.events().publish(
                (symbol_short!("create"), input.recipient.clone()),
                input.total_amount,
            );

            created_count += 1;
        }

        // Publish batch event
        env.events()
            .publish((symbol_short!("batch"),), (created_count, total_amount));

        created_count
    }

    /// Release all currently vested (but unreleased) tokens to the recipient.
    /// Can be called by anyone.
    pub fn release(env: Env, recipient: Address, index: Option<u32>) {
        Self::_check_paused(&env);
        let (key, mut schedule) = Self::_load_schedule(&env, &recipient, index);

        if schedule.revoked {
            panic_with_error!(&env, VestingError::ScheduleRevoked);
        }

        let vested = Self::_vested_amount(&env, &schedule);
        let releasable = vested - schedule.released;
        if releasable <= 0 {
            panic_with_error!(&env, VestingError::NothingToRelease);
        }

        schedule.released += releasable;
        env.storage().persistent().set(&key, &schedule);
        Self::_decrease_total_committed(&env, releasable);

        // Extend TTL for the schedule to prevent archiving
        // saturating_sub prevents u32 underflow when the schedule has fully vested (current_ledger > end_ledger)
        let remaining_ledgers = schedule
            .end_ledger
            .saturating_sub(env.ledger().sequence())
            .min(env.storage().max_ttl());
        if remaining_ledgers > 0 {
            env.storage()
                .persistent()
                .extend_ttl(&key, remaining_ledgers, remaining_ledgers);
        }

        // Transfer tokens from the vesting contract to the recipient via
        // the token contract's transfer function.
        let token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::TokenContract)
            .unwrap_or_else(|| panic_with_error!(&env, VestingError::NotInitialized));

        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        token_client.transfer(&env.current_contract_address(), &recipient, &releasable);

        env.events()
            .publish((symbol_short!("release"), recipient), releasable);
    }

    /// Refresh a schedule's storage TTL without releasing tokens.
    ///
    /// Schedules whose remaining duration exceeds the network's maximum
    /// entry TTL (roughly 180 days) have their storage TTL clamped at
    /// creation time (see `_ttl_ledgers`). For such long-dated grants,
    /// call this at least once per TTL window to keep the entry from
    /// being archived between claims. Can be called by anyone.
    pub fn keep_alive(env: Env, recipient: Address, index: Option<u32>) {
        Self::_check_paused(&env);
        let (key, schedule) = Self::_load_schedule(&env, &recipient, index);

        let remaining_ledgers = schedule
            .end_ledger
            .saturating_sub(env.ledger().sequence())
            .min(env.storage().max_ttl());
        if remaining_ledgers > 0 {
            env.storage()
                .persistent()
                .extend_ttl(&key, remaining_ledgers, remaining_ledgers);
        }
    }

    /// Admin-only: revoke a schedule, send vested portion to recipient,
    /// return unvested remainder to admin.
    pub fn revoke(env: Env, recipient: Address, index: Option<u32>) {
        Self::_check_paused(&env);
        Self::_require_admin(&env);

        let (key, mut schedule) = Self::_load_schedule(&env, &recipient, index);

        if schedule.revoked {
            panic_with_error!(&env, VestingError::AlreadyRevoked);
        }

        let vested = Self::_vested_amount(&env, &schedule);
        let releasable = vested - schedule.released;
        let unvested = schedule.total_amount - vested;
        let committed = schedule.total_amount - schedule.released;

        // Update schedule state
        schedule.revoked = true;
        schedule.released = vested; // All vested tokens are now accounted for as released (or being released)
        env.storage().persistent().set(&key, &schedule);
        Self::_decrease_total_committed(&env, committed);

        let token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::TokenContract)
            .unwrap_or_else(|| panic_with_error!(&env, VestingError::NotInitialized));

        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);

        // 1. Transfer releasable vested tokens to recipient
        if releasable > 0 {
            token_client.transfer(&env.current_contract_address(), &recipient, &releasable);
        }

        // 2. Transfer unvested tokens back to admin
        if unvested > 0 {
            let admin: Address = env
                .storage()
                .instance()
                .get(&DataKey::Admin)
                .unwrap_or_else(|| panic_with_error!(&env, VestingError::NotInitialized));
            token_client.transfer(&env.current_contract_address(), &admin, &unvested);
        }

        env.events()
            .publish((symbol_short!("revoke"), recipient), (releasable, unvested));
    }

    /// Admin-only: extend the cliff ledger of an existing (non-revoked) schedule.
    ///
    /// Also shifts `end_ledger` by the same delta so that the total vesting
    /// duration is preserved — the per-ledger unlock rate remains unchanged.
    ///
    /// Rules enforced:
    /// - `new_cliff` must be strictly greater than the current `cliff_ledger`
    ///   (extension only — reduction is never allowed).
    /// - The current ledger must still be before the cliff (once the cliff has
    ///   already passed there is nothing left to delay).
    /// - `new_cliff` must remain strictly less than the shifted `end_ledger`.
    pub fn extend_cliff(env: Env, recipient: Address, new_cliff: u32, index: Option<u32>) {
        Self::_check_paused(&env);
        Self::_require_admin(&env);

        let (key, mut schedule) = Self::_load_schedule(&env, &recipient, index);

        assert!(!schedule.revoked, "schedule has been revoked");
        assert!(
            env.ledger().sequence() < schedule.cliff_ledger,
            "cliff has already passed"
        );
        assert!(
            new_cliff > schedule.cliff_ledger,
            "new_cliff must be later than current cliff"
        );

        // Shift end_ledger by the same delta so vesting duration is preserved
        let delta = new_cliff - schedule.cliff_ledger;
        let old_cliff = schedule.cliff_ledger;
        let old_end = schedule.end_ledger;
        let new_end = schedule.end_ledger + delta;

        assert!(
            new_cliff < new_end,
            "new_cliff must be before the shifted end_ledger"
        );

        schedule.cliff_ledger = new_cliff;
        schedule.end_ledger = new_end;
        env.storage().persistent().set(&key, &schedule);

        // Emit event with old/new cliff and old/new end
        env.events().publish(
            (symbol_short!("clf_ext"), recipient),
            (old_cliff, new_cliff, old_end, new_end),
        );
    }

    // ── Read-only queries ───────────────────────────────────────────────

    /// Total amount vested so far (may or may not have been released).
    pub fn vested_amount(env: Env, recipient: Address, index: Option<u32>) -> i128 {
        let (_, schedule) = Self::_load_schedule(&env, &recipient, index);
        Self::_vested_amount(&env, &schedule)
    }

    /// Amount already released to the recipient.
    pub fn released_amount(env: Env, recipient: Address, index: Option<u32>) -> i128 {
        let (_, schedule) = Self::_load_schedule(&env, &recipient, index);
        schedule.released
    }

    /// Total tokens still committed to active vesting schedules.
    pub fn total_committed(env: Env) -> i128 {
        Self::_total_committed(&env)
    }

    /// Compare the vesting contract's live token balance to outstanding grants.
    pub fn solvency(env: Env) -> Solvency {
        let token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::TokenContract)
            .expect("not initialized");
        let total_committed = Self::_total_committed(&env);
        let token_balance = Self::_token_balance(&env, &token_addr);

        Solvency {
            token_balance,
            total_committed,
            solvent: token_balance >= total_committed,
        }
    }

    /// Returns `true` if the contract is currently paused.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::IsPaused)
            .unwrap_or(false)
    }

    /// Pause the vesting contract. Admin only.
    pub fn pause(env: Env) {
        Self::_require_admin(&env);
        env.storage().instance().set(&DataKey::IsPaused, &true);
        env.events().publish((symbol_short!("pause"),), ());
    }

    /// Unpause the vesting contract. Admin only.
    pub fn unpause(env: Env) {
        Self::_require_admin(&env);
        env.storage().instance().remove(&DataKey::IsPaused);
        env.events().publish((symbol_short!("unpause"),), ());
    }

    /// Upgrade this contract's WASM code hash in place. Admin only.
    ///
    /// Security note: this preserves existing storage and contract state, so
    /// new WASM must remain storage-compatible with previous deployments.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        Self::_require_admin(&env);
        assert!(
            new_wasm_hash != BytesN::from_array(&env, &[0; 32]),
            "invalid wasm hash"
        );
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        env.events()
            .publish((symbol_short!("upgrade"),), new_wasm_hash);
    }

    /// Permanently revoke the admin role and lock the contract.
    ///
    /// After this call:
    /// - No further `create_schedule`, `revoke`, `extend_cliff`,
    ///   `prune_recipient`, `propose_admin`, `accept_admin`,
    ///   `upgrade`, `pause`, or `unpause` operation can ever succeed.
    /// - The Admin storage entry is removed and a `Locked` flag is set.
    /// - `is_locked()` returns `true` from then on.
    ///
    /// Holders can still `release` and `keep_alive`. The contract
    /// becomes effectively immutable.
    ///
    /// **This action is irreversible.**
    pub fn revoke_admin(env: Env) {
        Self::_require_admin(&env);
        env.storage().instance().set(&DataKey::Locked, &true);
        env.storage().instance().remove(&DataKey::Admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        env.events().publish((symbol_short!("revoked"),), true);
    }

    /// Returns `true` once `revoke_admin` has been called. Once locked, no
    /// admin operation can ever succeed again.
    pub fn is_locked(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Locked)
            .unwrap_or(false)
    }

    /// Return the number of schedules stored for a recipient.
    pub fn get_schedule_count(env: Env, recipient: Address) -> u32 {
        Self::_schedule_count(&env, &recipient)
    }

    /// Return the full schedule struct for a recipient.
    pub fn get_schedule(env: Env, recipient: Address, index: Option<u32>) -> VestingSchedule {
        let (_, schedule) = Self::_load_schedule(&env, &recipient, index);
        schedule
    }

    /// Returns the admin address of this vesting contract.
    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, VestingError::NotInitialized))
    }

    /// Returns the address proposed via `propose_admin` that has not yet
    /// accepted the role, or `None` when no transfer is in progress.
    pub fn pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    /// Returns the token contract address managed by this vesting contract.
    pub fn get_token_contract(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::TokenContract)
            .unwrap_or_else(|| panic_with_error!(&env, VestingError::NotInitialized))
    }

    /// Return the number of recipients tracked (including any pruned slots).
    pub fn get_recipient_count(env: Env) -> u32 {
        Self::_recipient_count(&env)
    }

    /// Return paginated list of recipients with vesting schedules.
    ///
    /// `start` — zero-based offset into the recipients list.
    /// `limit` — maximum number of recipients to return, **clamped to
    /// `MAX_PAGE` (100)**. A larger request is served as a 100-entry page
    /// rather than rejected, so an over-eager client still makes progress
    /// instead of getting nothing back (#469).
    ///
    /// Pruned slots (see `prune_recipient`) are omitted from the result, so
    /// a page may contain fewer than `limit` entries even if more remain —
    /// which is also why a short page is not, on its own, proof that the
    /// list is exhausted. Callers should keep paging while the returned
    /// page is non-empty, as `useVestingDashboard` does.
    pub fn get_recipients_paginated(env: Env, start: u32, limit: u32) -> Vec<Address> {
        let total = Self::_recipient_count(&env);

        if start >= total {
            return Vec::new(&env);
        }

        let end = start.saturating_add(limit.min(MAX_PAGE)).min(total);

        let mut paginated = Vec::new(&env);
        let mut i = start;
        while i < end {
            if let Some(recipient) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&DataKey::RecipientAt(i))
            {
                paginated.push_back(recipient);
            }
            i += 1;
        }
        paginated
    }

    /// Admin-only: remove a fully-settled recipient from the enumeration
    /// index. Does not touch the recipient's schedules — it only prunes the
    /// enumeration slot(s) so `get_recipients_paginated` stops listing them.
    pub fn prune_recipient(env: Env, recipient: Address) {
        Self::_require_admin(&env);

        let total = Self::_recipient_count(&env);
        let mut i = 0u32;
        let mut pruned = false;
        while i < total {
            let key = DataKey::RecipientAt(i);
            if let Some(stored) = env.storage().persistent().get::<DataKey, Address>(&key) {
                if stored == recipient {
                    env.storage().persistent().remove(&key);
                    pruned = true;
                }
            }
            i += 1;
        }

        if !pruned {
            panic_with_error!(&env, VestingError::RecipientNotTracked);
        }
        env.events().publish((symbol_short!("prune"),), recipient);
    }

    /// Sum vested amount across all non-revoked schedules for a recipient.
    ///
    /// Bounded by `MAX_SCHEDULES_PER_RECIPIENT`: the writer refuses to store
    /// a 51st schedule, so this loop cannot outgrow the compute budget
    /// (issue #466).
    pub fn total_vested(env: Env, recipient: Address) -> i128 {
        let count = Self::_schedule_count(&env, &recipient);
        let mut total: i128 = 0;
        for i in 0..count {
            let key = Self::_schedule_key(&recipient, i);
            if let Some(schedule) = env
                .storage()
                .persistent()
                .get::<DataKey, VestingSchedule>(&key)
            {
                if !schedule.revoked {
                    total += Self::_vested_amount(&env, &schedule);
                }
            }
        }
        total
    }

    /// Sum released amount across all schedules for a recipient.
    ///
    /// Bounded by `MAX_SCHEDULES_PER_RECIPIENT` (see `total_vested`).
    pub fn total_released(env: Env, recipient: Address) -> i128 {
        let count = Self::_schedule_count(&env, &recipient);
        let mut total: i128 = 0;
        for i in 0..count {
            let key = Self::_schedule_key(&recipient, i);
            if let Some(schedule) = env
                .storage()
                .persistent()
                .get::<DataKey, VestingSchedule>(&key)
            {
                total += schedule.released;
            }
        }
        total
    }

    /// Sum releasable (vested minus released) across all non-revoked schedules.
    ///
    /// Bounded by `MAX_SCHEDULES_PER_RECIPIENT` (see `total_vested`).
    pub fn total_releasable(env: Env, recipient: Address) -> i128 {
        let count = Self::_schedule_count(&env, &recipient);
        let mut total: i128 = 0;
        for i in 0..count {
            let key = Self::_schedule_key(&recipient, i);
            if let Some(schedule) = env
                .storage()
                .persistent()
                .get::<DataKey, VestingSchedule>(&key)
            {
                if !schedule.revoked {
                    let vested = Self::_vested_amount(&env, &schedule);
                    total += vested - schedule.released;
                }
            }
        }
        total
    }

    /// Return all schedule objects for a recipient in a single call.
    ///
    /// Safe to call for any recipient the cap admits, because that is what
    /// bounds the loop: at most `MAX_SCHEDULES_PER_RECIPIENT` entries are ever
    /// stored (issue #466). `get_schedules_paginated` is the incremental
    /// alternative for a client that would rather bound its own work.
    pub fn get_all_schedules(env: Env, recipient: Address) -> Vec<VestingSchedule> {
        let count = Self::_schedule_count(&env, &recipient);
        let mut schedules: Vec<VestingSchedule> = Vec::new(&env);
        for i in 0..count {
            let key = Self::_schedule_key(&recipient, i);
            if let Some(schedule) = env
                .storage()
                .persistent()
                .get::<DataKey, VestingSchedule>(&key)
            {
                schedules.push_back(schedule);
            }
        }
        schedules
    }

    /// Return one page of a recipient's schedules.
    ///
    /// `start` — zero-based index into the recipient's schedule range.
    /// `limit` — maximum number of schedules to return, **clamped to
    /// `MAX_PAGE` (100)**; a larger request is served as a 100-entry page
    /// rather than rejected (#469).
    ///
    /// The same `get_recipients_paginated` contract applies: a page may come
    /// back short even when more entries remain, so a caller paging through
    /// everything should stop on an empty page rather than on a short one.
    /// `get_schedule_count` reports how many slots exist in total.
    ///
    /// Added alongside `get_all_schedules`, not in place of it: the cap is
    /// what makes the single-call form safe, and this lets a client render
    /// grants incrementally without paying for the whole set up front
    /// (issue #466).
    pub fn get_schedules_paginated(
        env: Env,
        recipient: Address,
        start: u32,
        limit: u32,
    ) -> Vec<VestingSchedule> {
        let count = Self::_schedule_count(&env, &recipient);
        let mut schedules: Vec<VestingSchedule> = Vec::new(&env);

        if start >= count {
            return schedules;
        }

        let end = start.saturating_add(limit.min(MAX_PAGE)).min(count);
        let mut i = start;
        while i < end {
            let key = Self::_schedule_key(&recipient, i);
            if let Some(schedule) = env
                .storage()
                .persistent()
                .get::<DataKey, VestingSchedule>(&key)
            {
                schedules.push_back(schedule);
            }
            i += 1;
        }
        schedules
    }

    /// Release all releasable tokens across all non-revoked schedules
    /// in a single token transfer.
    ///
    /// Bounded by `MAX_SCHEDULES_PER_RECIPIENT` (see `total_vested`): this
    /// loop used to be the one place a holder could be left unable to claim
    /// at all, because it walks the same uncapped range as the getters.
    pub fn release_all(env: Env, recipient: Address) {
        Self::_check_paused(&env);
        recipient.require_auth();

        let count = Self::_schedule_count(&env, &recipient);
        let mut total_releasable: i128 = 0;

        for i in 0..count {
            let key = Self::_schedule_key(&recipient, i);
            if let Some(mut schedule) = env
                .storage()
                .persistent()
                .get::<DataKey, VestingSchedule>(&key)
            {
                if schedule.revoked {
                    continue;
                }
                let vested = Self::_vested_amount(&env, &schedule);
                let releasable = vested - schedule.released;
                if releasable > 0 {
                    schedule.released += releasable;
                    env.storage().persistent().set(&key, &schedule);

                    let remaining_ledgers = schedule
                        .end_ledger
                        .saturating_sub(env.ledger().sequence())
                        .min(env.storage().max_ttl());
                    if remaining_ledgers > 0 {
                        env.storage().persistent().extend_ttl(
                            &key,
                            remaining_ledgers,
                            remaining_ledgers,
                        );
                    }

                    total_releasable += releasable;
                }
            }
        }

        assert!(total_releasable > 0, "nothing to release");

        let token_addr: Address = env
            .storage()
            .instance()
            .get(&DataKey::TokenContract)
            .expect("not initialized");

        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        token_client.transfer(
            &env.current_contract_address(),
            &recipient,
            &total_releasable,
        );

        env.events()
            .publish((symbol_short!("release"), recipient), total_releasable);
    }

    // ── Internals ───────────────────────────────────────────────────────

    /// Authorises the current admin and returns it, so callers that move
    /// tokens *from* the admin can bind it without re-reading storage.
    fn _require_admin(env: &Env) -> Address {
        Self::_require_not_locked(env);
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(env, VestingError::NotInitialized));
        admin.require_auth();
        admin
    }

    fn _require_not_locked(env: &Env) {
        let locked: bool = env
            .storage()
            .instance()
            .get(&DataKey::Locked)
            .unwrap_or(false);
        if locked {
            panic!("admin revoked: contract is locked");
        }
    }

    fn _check_paused(env: &Env) {
        if env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::IsPaused)
            .unwrap_or(false)
        {
            panic_with_error!(env, VestingError::Paused);
        }
    }

    fn _validate_total_amount(total_amount: i128) {
        assert!(total_amount > 0, "total_amount must be positive");
        assert!(
            total_amount <= MAX_VESTING_AMOUNT,
            "total_amount exceeds vesting limit"
        );
    }

    fn _schedule_key(recipient: &Address, index: u32) -> DataKey {
        DataKey::Schedule(recipient.clone(), index)
    }

    fn _schedule_count(env: &Env, recipient: &Address) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::ScheduleCount(recipient.clone()))
            .unwrap_or(0)
    }

    fn _set_schedule_count(env: &Env, recipient: &Address, count: u32, ttl_ledgers: u32) {
        let key = DataKey::ScheduleCount(recipient.clone());
        env.storage().persistent().set(&key, &count);
        Self::_extend_persistent_ttl(env, &key, ttl_ledgers);
    }

    fn _total_committed(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::TotalCommitted)
            .unwrap_or(0)
    }

    fn _set_total_committed(env: &Env, amount: i128) {
        env.storage()
            .instance()
            .set(&DataKey::TotalCommitted, &amount);
    }

    fn _increase_total_committed(env: &Env, amount: i128) {
        let total = Self::_total_committed(env)
            .checked_add(amount)
            .expect("total committed overflow");
        Self::_set_total_committed(env, total);
    }

    fn _decrease_total_committed(env: &Env, amount: i128) {
        Self::_set_total_committed(env, Self::_total_committed(env).saturating_sub(amount));
    }

    fn _token_balance(env: &Env, token_addr: &Address) -> i128 {
        let token_client = soroban_sdk::token::Client::new(env, token_addr);
        token_client.balance(&env.current_contract_address())
    }

    fn _assert_solvent(env: &Env, token_addr: &Address) {
        assert!(
            Self::_token_balance(env, token_addr) >= Self::_total_committed(env),
            "vesting contract underfunded"
        );
    }

    fn _resolve_schedule_index(env: &Env, recipient: &Address, index: Option<u32>) -> u32 {
        let count = Self::_schedule_count(env, recipient);
        let resolved = match index {
            Some(index) => index,
            None => {
                if count == 0 {
                    panic_with_error!(env, VestingError::ScheduleNotFound);
                }
                count - 1
            }
        };
        if resolved >= count {
            panic_with_error!(env, VestingError::ScheduleIndexOutOfBounds);
        }
        resolved
    }

    fn _load_schedule(
        env: &Env,
        recipient: &Address,
        index: Option<u32>,
    ) -> (DataKey, VestingSchedule) {
        let resolved_index = Self::_resolve_schedule_index(env, recipient, index);
        let key = Self::_schedule_key(recipient, resolved_index);
        let schedule: VestingSchedule = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(env, VestingError::ScheduleNotFound));
        (key, schedule)
    }

    fn _ttl_ledgers(env: &Env, end_ledger: u32) -> u32 {
        let current_ledger = env.ledger().sequence();
        let desired = if end_ledger > current_ledger {
            end_ledger - current_ledger
        } else {
            // Default TTL if end_ledger is in the past
            TTL_LEDGERS
        };
        // Soroban rejects extend_to above the network's max entry TTL. Schedules
        // whose end is further out than one TTL window still need a keep-alive
        // (via `release` or `keep_alive`) at least once per window to stay live.
        desired.min(env.storage().max_ttl())
    }

    fn _extend_persistent_ttl(env: &Env, key: &DataKey, ttl_ledgers: u32) {
        env.storage()
            .persistent()
            .extend_ttl(key, ttl_ledgers, ttl_ledgers);
    }

    /// Cliff + linear vesting formula.
    ///
    /// - Before cliff → 0
    /// - Between cliff and end → proportional
    /// - After end → total_amount
    fn _vested_amount(env: &Env, schedule: &VestingSchedule) -> i128 {
        let current = env.ledger().sequence();

        if current < schedule.cliff_ledger {
            return 0;
        }
        if current >= schedule.end_ledger {
            return schedule.total_amount;
        }

        // Linear interpolation between cliff and end
        let elapsed = (current - schedule.cliff_ledger) as i128;
        let duration = (schedule.end_ledger - schedule.cliff_ledger) as i128;
        schedule
            .total_amount
            .checked_mul(elapsed)
            .expect("vesting amount multiplication overflow")
            / duration
    }

    fn _recipient_count(env: &Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::RecipientCount)
            .unwrap_or(0)
    }

    fn _add_recipient(env: &Env, recipient: &Address) {
        // `_add_recipient` is only called when `schedule_index == 0`, i.e. the
        // caller has already established this is the recipient's first
        // schedule with this contract, so no linear scan is needed here.
        let count = Self::_recipient_count(env);
        let key = DataKey::RecipientAt(count);
        env.storage().persistent().set(&key, recipient);

        let ttl_ledgers = TTL_LEDGERS.min(env.storage().max_ttl());
        Self::_extend_persistent_ttl(env, &key, ttl_ledgers);

        let count_key = DataKey::RecipientCount;
        env.storage().persistent().set(&count_key, &(count + 1));
        Self::_extend_persistent_ttl(env, &count_key, ttl_ledgers);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::IntoVal;
    use soroban_sdk::{testutils::Address as _, testutils::Events as _, testutils::Ledger, Env};

    // ── Event topic fixture ─────────────────────────────────────────────
    //
    // The checked-in, single source of truth for every event topic-0 name
    // this contract emits. `docs/events.md` is generated from
    // `docs/events.json`, which must list exactly this set — see issue
    // #340, where the doc drifted from the contract (documented 3 of the
    // ~10 events this contract actually emits) and a frontend indexer was
    // built against the stale doc instead of the contract, dropping whole
    // categories of activity. `scripts/generate_events_doc.py --check`
    // re-derives this same set directly from source and fails CI if it
    // and `docs/events.json` disagree.
    const EXPECTED_TOPICS: [&str; 13] = [
        "init", "prop_adm", "acc_adm", "create", "batch", "release", "revoke", "clf_ext", "pause",
        "unpause", "prune", "upgrade", "revoked",
    ];

    /// Asserts the set of `symbol_short!("...")` topic-0 literals used in
    /// this file's production code (everything before the test module)
    /// exactly matches `EXPECTED_TOPICS`. Static rather than live because
    /// scanning every `.publish(...)` call site covers events regardless
    /// of how hard they are to trigger in a live scenario.
    #[test]
    fn test_emitted_topics_match_checked_in_fixture() {
        const SOURCE: &str = include_str!("lib.rs");
        let (production_source, _) = SOURCE
            .split_once("#[cfg(test)]\nmod test {")
            .expect("could not locate test module boundary in lib.rs");

        const NEEDLE: &str = "symbol_short!(\"";

        // Every expected topic must actually appear as a symbol_short! literal.
        for topic in EXPECTED_TOPICS {
            let mut rest = production_source;
            let mut found = false;
            while let Some(pos) = rest.find(NEEDLE) {
                let after = &rest[pos + NEEDLE.len()..];
                if after.len() > topic.len()
                    && after.starts_with(topic)
                    && after.as_bytes()[topic.len()] == b'"'
                {
                    found = true;
                    break;
                }
                rest = &after[1..];
            }
            assert!(
                found,
                "topic {topic:?} is listed in EXPECTED_TOPICS but no \
                 symbol_short!(\"{topic}\") literal was found in the contract"
            );
        }
        // No symbol_short! literal exists outside the expected set — i.e.
        // nothing new was added without updating the fixture (and
        // docs/events.json / docs/events.md alongside it).

        let mut rest = production_source;
        while let Some(pos) = rest.find(NEEDLE) {
            let after = &rest[pos + NEEDLE.len()..];
            let end = after.find('"').expect("unterminated symbol_short! literal");
            let name = &after[..end];
            assert!(
                EXPECTED_TOPICS.contains(&name),
                "topic {name:?} is emitted by the contract but missing from \
                 EXPECTED_TOPICS (and likely docs/events.json / docs/events.md)"
            );
            rest = &after[end..];
        }
    }

    #[test]
    #[should_panic(expected = "cliff_ledger must not be in the past")]
    fn test_create_schedule_past_cliff_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        client.initialize(&admin, &token_addr);

        // Advance current ledger sequence to 100
        env.ledger().set_sequence_number(100);

        // Attempt to pass a cliff_ledger (50) that is before current ledger (100)
        client.create_schedule(&recipient, &1000, &50, &200);
    }

    // ── TTL constant tests ──────────────────────────────────────────────

    #[test]
    fn test_ttl_requests_are_clamped_to_the_network_ceiling() {
        // See the token contract's equivalent test for why this asserts a
        // relationship rather than a network figure (#398).
        let env = Env::default();
        let contract_id = env.register_contract(None, VestingContract);

        env.as_contract(&contract_id, || {
            let network_max = env.storage().max_ttl();

            // A schedule ending far beyond the ceiling is capped at it.
            assert_eq!(
                VestingContract::_ttl_ledgers(&env, u32::MAX),
                network_max,
                "a far-future schedule must be capped at the network ceiling"
            );

            // One already in the past falls back to the request, itself clamped.
            assert_eq!(
                VestingContract::_ttl_ledgers(&env, 0),
                TTL_LEDGERS.min(network_max),
                "the fallback TTL must also be clamped"
            );

            // Whatever the schedule, the result never exceeds the ceiling.
            for end_ledger in [0u32, 1, 1_000, TTL_LEDGERS, u32::MAX] {
                assert!(VestingContract::_ttl_ledgers(&env, end_ledger) <= network_max);
            }
        });
    }

    #[test]
    fn test_ttl_ledgers_encodes_a_one_year_request() {
        // Documents intent only; the effective window is whatever the network
        // allows, currently about 180 days.
        let days = (TTL_LEDGERS as u64 * 5) / (24 * 60 * 60);
        assert_eq!(days, 365);
    }

    fn latest_index() -> Option<u32> {
        None
    }

    fn index(value: u32) -> Option<u32> {
        Some(value)
    }

    fn get_schedule_latest(client: &VestingContractClient, recipient: &Address) -> VestingSchedule {
        client.get_schedule(recipient, &latest_index())
    }

    fn get_schedule_at(
        client: &VestingContractClient,
        recipient: &Address,
        schedule_index: u32,
    ) -> VestingSchedule {
        client.get_schedule(recipient, &index(schedule_index))
    }

    fn vested_amount_latest(client: &VestingContractClient, recipient: &Address) -> i128 {
        client.vested_amount(recipient, &latest_index())
    }

    fn vested_amount_at(
        client: &VestingContractClient,
        recipient: &Address,
        schedule_index: u32,
    ) -> i128 {
        client.vested_amount(recipient, &index(schedule_index))
    }

    fn released_amount_latest(client: &VestingContractClient, recipient: &Address) -> i128 {
        client.released_amount(recipient, &latest_index())
    }

    fn released_amount_at(
        client: &VestingContractClient,
        recipient: &Address,
        schedule_index: u32,
    ) -> i128 {
        client.released_amount(recipient, &index(schedule_index))
    }

    fn release_latest(client: &VestingContractClient, recipient: &Address) {
        client.release(recipient, &latest_index());
    }

    fn release_at(client: &VestingContractClient, recipient: &Address, schedule_index: u32) {
        client.release(recipient, &index(schedule_index));
    }

    fn revoke_latest(client: &VestingContractClient, recipient: &Address) {
        client.revoke(recipient, &latest_index());
    }

    fn revoke_at(client: &VestingContractClient, recipient: &Address, schedule_index: u32) {
        client.revoke(recipient, &index(schedule_index));
    }

    fn extend_cliff_latest(client: &VestingContractClient, recipient: &Address, new_cliff: u32) {
        client.extend_cliff(recipient, &new_cliff, &latest_index());
    }

    fn setup_schedule(env: &Env, client: &VestingContractClient) -> (Address, Address) {
        let admin = Address::generate(env);
        let recipient = Address::generate(env);

        // Register a mock token contract
        let token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::StellarAssetClient::new(env, &token);

        // Mint tokens to the admin
        token_client.mint(&admin, &1_000_000i128);

        client.initialize(&admin, &token);

        // cliff at ledger 100, fully vested at ledger 200
        // The admin will transfer tokens directly in create_schedule
        client.create_schedule(&recipient, &1_000i128, &100u32, &200u32);

        (admin, recipient)
    }

    #[test]
    fn test_initialize() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
    }

    #[test]
    fn test_double_init() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
        assert_eq!(
            client.try_initialize(&admin, &token),
            Err(Ok(VestingError::AlreadyInitialized.into()))
        );
    }

    #[test]
    fn test_create_schedule_and_getters() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        let schedule = get_schedule_latest(&client, &recipient);
        assert_eq!(schedule.total_amount, 1_000);
        assert_eq!(schedule.cliff_ledger, 100);
        assert_eq!(schedule.end_ledger, 200);
        assert_eq!(schedule.released, 0);
        assert!(!schedule.revoked);
    }

    #[test]
    fn test_pending_admin_getter_and_cancel() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let proposed = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&admin, &token);

        assert_eq!(client.pending_admin(), None);

        client.propose_admin(&proposed);
        assert_eq!(client.pending_admin(), Some(proposed.clone()));

        client.cancel_admin_proposal();
        assert_eq!(client.pending_admin(), None);

        client.propose_admin(&proposed);
        client.accept_admin();
        assert_eq!(client.pending_admin(), None);
    }

    #[test]
    fn test_total_committed_tracks_schedule_lifecycle() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let other = Address::generate(&env);
        let token_addr = env.register_stellar_asset_contract(admin.clone());
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &5_000);

        assert_eq!(client.total_committed(), 0);

        client.create_schedule(&recipient, &1_000, &100, &200);
        assert_eq!(client.total_committed(), 1_000);
        assert_eq!(token_client.balance(&contract_id), 1_000);
        assert_eq!(
            client.solvency(),
            Solvency {
                token_balance: 1_000,
                total_committed: 1_000,
                solvent: true,
            }
        );

        env.ledger().set_sequence_number(150);
        release_latest(&client, &recipient);
        assert_eq!(client.total_committed(), 500);
        assert_eq!(token_client.balance(&contract_id), 500);

        client.create_schedule(&other, &300, &200, &300);
        assert_eq!(client.total_committed(), 800);
        assert_eq!(token_client.balance(&contract_id), 800);

        revoke_latest(&client, &recipient);
        assert_eq!(client.total_committed(), 300);
        assert_eq!(token_client.balance(&contract_id), 300);
    }

    #[test]
    fn test_solvency_reports_external_token_drain() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env.register_stellar_asset_contract(admin.clone());
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &1_000);
        client.create_schedule(&recipient, &1_000, &100, &200);

        // Models any token-side admin action that drains already committed funds
        // from the vesting contract, such as clawback on a clawbackable token.
        token_client.transfer(&contract_id, &admin, &400);

        assert_eq!(
            client.solvency(),
            Solvency {
                token_balance: 600,
                total_committed: 1_000,
                solvent: false,
            }
        );
    }

    #[test]
    #[should_panic(expected = "vesting contract underfunded")]
    fn test_create_schedule_rejects_when_existing_commitments_are_underfunded() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let other = Address::generate(&env);
        let token_addr = env.register_stellar_asset_contract(admin.clone());
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &2_000);
        client.create_schedule(&recipient, &1_000, &100, &200);
        token_client.transfer(&contract_id, &admin, &400);

        client.create_schedule(&other, &100, &100, &200);
    }

    #[test]
    fn test_vested_before_cliff() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        env.ledger().set_sequence_number(50);
        assert_eq!(vested_amount_latest(&client, &recipient), 0);
    }

    #[test]
    fn test_vested_midway() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        env.ledger().set_sequence_number(150);
        assert_eq!(vested_amount_latest(&client, &recipient), 500);
    }

    #[test]
    fn test_vested_amount_is_safe_for_large_amounts_and_long_durations() {
        let env = Env::default();
        let recipient = Address::generate(&env);
        let total_amount = MAX_VESTING_AMOUNT;

        // Exercise the largest valid amount over several long ledger spans.
        // Each result must remain within the schedule allocation, proving the
        // interpolation intermediate does not wrap.
        for end_ledger in [1_000_000u32, u32::MAX - 1, u32::MAX] {
            let schedule = VestingSchedule {
                recipient: recipient.clone(),
                total_amount,
                cliff_ledger: 0,
                end_ledger,
                released: 0,
                revoked: false,
            };
            env.ledger().set_sequence_number(end_ledger - 1);
            let vested = VestingContract::_vested_amount(&env, &schedule);
            assert!(vested > 0);
            assert!(vested <= total_amount);
        }
    }

    #[test]
    #[should_panic(expected = "total_amount exceeds vesting limit")]
    fn test_total_amount_above_vesting_limit_is_rejected() {
        VestingContract::_validate_total_amount(MAX_VESTING_AMOUNT + 1);
    }

    #[test]
    fn test_release_incremental() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        env.ledger().set_sequence_number(125);
        release_latest(&client, &recipient);
        assert_eq!(released_amount_latest(&client, &recipient), 250);

        env.ledger().set_sequence_number(150);
        release_latest(&client, &recipient);
        assert_eq!(released_amount_latest(&client, &recipient), 500);

        env.ledger().set_sequence_number(200);
        release_latest(&client, &recipient);
        assert_eq!(released_amount_latest(&client, &recipient), 1000);
    }

    #[test]
    fn test_double_release_same_ledger_fails() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        env.ledger().set_sequence_number(150);
        release_latest(&client, &recipient);
        assert_eq!(
            client.try_release(&recipient, &latest_index()),
            Err(Ok(VestingError::NothingToRelease.into()))
        );
    }

    // ── Regression tests for issue #215: u32 underflow in release() ────

    #[test]
    fn test_release_after_full_vesting() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &1000);

        // Create schedule: cliff at 100, end at 200
        client.create_schedule(&recipient, &1000, &100, &200);

        // Advance past end_ledger to simulate fully vested schedule
        env.ledger().set_sequence_number(250);

        // This should succeed without u32 underflow
        release_latest(&client, &recipient);

        // Verify full amount was released
        assert_eq!(released_amount_latest(&client, &recipient), 1000);
        assert_eq!(token_client.balance(&recipient), 1000);
    }

    #[test]
    fn test_release_at_exact_end_ledger() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &1000);

        client.create_schedule(&recipient, &1000, &100, &200);

        // Advance to exactly end_ledger
        env.ledger().set_sequence_number(200);

        release_latest(&client, &recipient);

        // Verify full amount was released
        assert_eq!(released_amount_latest(&client, &recipient), 1000);
        assert_eq!(token_client.balance(&recipient), 1000);
    }

    #[test]
    fn test_release_one_ledger_before_end() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &1000);

        client.create_schedule(&recipient, &1000, &100, &200);

        // Advance to one ledger before end
        env.ledger().set_sequence_number(199);

        release_latest(&client, &recipient);

        // Verify nearly full amount was released (990 out of 1000)
        assert_eq!(released_amount_latest(&client, &recipient), 990);
        assert_eq!(token_client.balance(&recipient), 990);
    }

    #[test]
    fn test_release_far_past_end_ledger() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &1000);

        client.create_schedule(&recipient, &1000, &100, &200);

        // Advance well past end_ledger (simulating a schedule left unclaimed for some time)
        // Using 500 to avoid test environment archiving issues while still testing the fix
        env.ledger().set_sequence_number(500);

        // This should succeed without u32 underflow
        release_latest(&client, &recipient);

        assert_eq!(released_amount_latest(&client, &recipient), 1000);
        assert_eq!(token_client.balance(&recipient), 1000);
    }

    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn test_revoke_transfers_correctly() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &1000);

        client.create_schedule(&recipient, &1000, &100, &200);

        env.ledger().set_sequence_number(150);
        revoke_latest(&client, &recipient);

        assert_eq!(token_client.balance(&recipient), 500);
        assert_eq!(token_client.balance(&admin), 500);
    }

    #[test]
    fn test_revoke_after_partial_release() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &1000);

        client.create_schedule(&recipient, &1000, &100, &200);

        env.ledger().set_sequence_number(125);
        release_latest(&client, &recipient);
        assert_eq!(token_client.balance(&recipient), 250);

        env.ledger().set_sequence_number(175);
        revoke_latest(&client, &recipient);

        assert_eq!(token_client.balance(&recipient), 750);
        assert_eq!(token_client.balance(&admin), 250);
    }

    #[test]
    fn test_release_after_revoke_fails() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        env.ledger().set_sequence_number(150);
        revoke_latest(&client, &recipient);

        env.ledger().set_sequence_number(200);
        assert_eq!(
            client.try_release(&recipient, &latest_index()),
            Err(Ok(VestingError::ScheduleRevoked.into()))
        );
    }

    #[test]
    fn test_double_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        revoke_latest(&client, &recipient);
        assert_eq!(
            client.try_revoke(&recipient, &latest_index()),
            Err(Ok(VestingError::AlreadyRevoked.into()))
        );
    }

    #[test]
    fn test_revoke_non_admin_panics() {
        let env = Env::default();
        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
        assert!(client.try_revoke(&recipient, &latest_index()).is_err());
    }

    // ── extend_cliff tests ─────────────────────────────────────────────

    #[test]
    fn test_extend_cliff_success() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        // Cliff is at 100; extend it to 150 while current ledger is still 0
        extend_cliff_latest(&client, &recipient, 150u32);

        let schedule = get_schedule_latest(&client, &recipient);
        assert_eq!(schedule.cliff_ledger, 150);
        assert_eq!(schedule.end_ledger, 250); // shifted by same delta (+50)

        // Verify event emission contains (old_cliff, new_cliff, old_end, new_end)
        use soroban_sdk::IntoVal;
        let events = env.events().all();
        let last_event = events.slice(events.len() - 1..);
        assert_eq!(
            last_event,
            soroban_sdk::vec![
                &env,
                (
                    contract_id,
                    (symbol_short!("clf_ext"), recipient).into_val(&env),
                    (100u32, 150u32, 200u32, 250u32).into_val(&env)
                )
            ]
        );
    }

    #[test]
    fn test_extend_cliff_preserves_unlock_rate() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        // Original: cliff=100, end=200, duration=100 ledgers.
        // At ledger 150 (midpoint), 500 out of 1000 should be vested.
        env.ledger().set_sequence_number(150);
        let vested_before = vested_amount_latest(&client, &recipient);

        // Now extend cliff from 100 to 120 (delta = +20).
        // With the fix, end should also shift by +20 to 220, preserving the
        // 100-ledger vesting duration. At ledger 170 (midpoint of 120→220),
        // the same 500 out of 1000 should be vested.
        env.ledger().set_sequence_number(50); // must be before cliff
        extend_cliff_latest(&client, &recipient, 120u32);

        let schedule = get_schedule_latest(&client, &recipient);
        assert_eq!(schedule.cliff_ledger, 120);
        assert_eq!(schedule.end_ledger, 220);
        assert_eq!(schedule.end_ledger - schedule.cliff_ledger, 100); // duration preserved

        // At the new midpoint (170) the vested amount should be the same
        env.ledger().set_sequence_number(170);
        let vested_after = vested_amount_latest(&client, &recipient);
        assert_eq!(vested_before, vested_after);

        // At the original end_ledger (200), tokens should NOT be fully
        // vested anymore — only 80% should be vested (80/100 elapsed)
        env.ledger().set_sequence_number(200);
        assert_eq!(vested_amount_latest(&client, &recipient), 800);

        // At the new end_ledger (220), tokens should be fully vested
        env.ledger().set_sequence_number(220);
        assert_eq!(vested_amount_latest(&client, &recipient), 1000);
    }

    #[test]
    #[should_panic(expected = "new_cliff must be later than current cliff")]
    fn test_extend_cliff_cannot_reduce() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        // cliff is 100; trying to set it to 50 must panic
        assert_eq!(
            client.try_extend_cliff(&recipient, &50u32, &latest_index()),
            Err(Ok(VestingError::CliffNotExtended.into()))
        );
    }

    #[test]
    fn test_extend_cliff_after_cliff_passed() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        // Jump past the cliff
        env.ledger().set_sequence_number(120);
        assert_eq!(
            client.try_extend_cliff(&recipient, &150u32, &latest_index()),
            Err(Ok(VestingError::CliffPassed.into()))
        );
    }

    #[test]
    fn test_extend_cliff_non_admin_panics() {
        let env = Env::default();
        // Do NOT mock all auths — only mock nothing so admin auth fails
        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);

        // Use mock_all_auths only for setup
        env.mock_all_auths();
        client.initialize(&admin, &token);
        asset_client.mint(&admin, &1_000_000i128);
        let recipient = Address::generate(&env);
        client.create_schedule(&recipient, &1_000i128, &100u32, &200u32);

        // Clear auths so the next call fails
        env.set_auths(&[]);
        assert!(client
            .try_extend_cliff(&recipient, &150u32, &latest_index())
            .is_err());
    }

    #[test]
    fn test_extend_cliff_blocked_when_paused() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        client.pause();
        assert_eq!(
            client.try_extend_cliff(&recipient, &150u32, &latest_index()),
            Err(Ok(VestingError::Paused.into()))
        );
    }

    #[test]
    fn test_create_schedule_invalid_amount() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, admin, recipient) = (
            VestingContractClient::new(&env, &env.register_contract(None, VestingContract)),
            Address::generate(&env),
            Address::generate(&env),
        );
        client.initialize(&admin, &Address::generate(&env));
        assert_eq!(
            client.try_create_schedule(&recipient, &0, &100, &200),
            Err(Ok(VestingError::InvalidAmount.into()))
        );
    }

    #[test]
    fn test_create_schedules_batch_basic() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);

        // Mint enough tokens for all schedules
        asset_client.mint(&admin, &5000);

        // Create batch of 3 schedules
        let recipient1 = Address::generate(&env);
        let recipient2 = Address::generate(&env);
        let recipient3 = Address::generate(&env);

        let mut schedules = Vec::new(&env);
        schedules.push_back(ScheduleInput {
            recipient: recipient1.clone(),
            total_amount: 1000,
            cliff_ledger: 100,
            end_ledger: 200,
        });
        schedules.push_back(ScheduleInput {
            recipient: recipient2.clone(),
            total_amount: 2000,
            cliff_ledger: 150,
            end_ledger: 250,
        });
        schedules.push_back(ScheduleInput {
            recipient: recipient3.clone(),
            total_amount: 1500,
            cliff_ledger: 120,
            end_ledger: 220,
        });

        let count = client.create_schedules_batch(&schedules);
        assert_eq!(count, 3);

        // Verify all schedules were created correctly
        let schedule1 = get_schedule_latest(&client, &recipient1);
        assert_eq!(schedule1.total_amount, 1000);
        assert_eq!(schedule1.cliff_ledger, 100);
        assert_eq!(schedule1.end_ledger, 200);

        let schedule2 = get_schedule_latest(&client, &recipient2);
        assert_eq!(schedule2.total_amount, 2000);
        assert_eq!(schedule2.cliff_ledger, 150);
        assert_eq!(schedule2.end_ledger, 250);

        let schedule3 = get_schedule_latest(&client, &recipient3);
        assert_eq!(schedule3.total_amount, 1500);
        assert_eq!(schedule3.cliff_ledger, 120);
        assert_eq!(schedule3.end_ledger, 220);
    }

    #[test]
    fn test_create_schedules_batch_large() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);

        // Mint enough tokens for 50 schedules
        asset_client.mint(&admin, &50_000);

        // Create batch of 50 schedules (simulating staff/investor distribution)
        let mut schedules = Vec::new(&env);
        for _ in 0..50 {
            let recipient = Address::generate(&env);
            schedules.push_back(ScheduleInput {
                recipient,
                total_amount: 1000,
                cliff_ledger: 100,
                end_ledger: 200,
            });
        }

        let count = client.create_schedules_batch(&schedules);
        assert_eq!(count, 50);
    }

    #[test]
    fn test_create_schedules_batch_too_large() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);

        // Ensure funding checks do not mask the batch-size assertion.
        asset_client.mint(&admin, &100_000);

        let mut schedules = Vec::new(&env);
        for _ in 0..51 {
            schedules.push_back(ScheduleInput {
                recipient: Address::generate(&env),
                total_amount: 1000,
                cliff_ledger: 100,
                end_ledger: 200,
            });
        }

        assert_eq!(
            client.try_create_schedules_batch(&schedules),
            Err(Ok(VestingError::BatchTooLarge.into()))
        );
    }

    #[test]
    fn test_create_schedules_batch_empty() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        client.initialize(&admin, &token_addr);

        let schedules = Vec::new(&env);
        assert_eq!(
            client.try_create_schedules_batch(&schedules),
            Err(Ok(VestingError::BatchEmpty.into()))
        );
    }

    #[test]
    fn test_create_schedules_batch_invalid_amount() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        client.initialize(&admin, &token_addr);

        let recipient = Address::generate(&env);
        let mut schedules = Vec::new(&env);
        schedules.push_back(ScheduleInput {
            recipient,
            total_amount: 0,
            cliff_ledger: 100,
            end_ledger: 200,
        });

        assert_eq!(
            client.try_create_schedules_batch(&schedules),
            Err(Ok(VestingError::InvalidAmount.into()))
        );
    }

    #[test]
    fn test_create_schedules_batch_invalid_ledgers() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        client.initialize(&admin, &token_addr);

        let recipient = Address::generate(&env);
        let mut schedules = Vec::new(&env);
        schedules.push_back(ScheduleInput {
            recipient,
            total_amount: 1000,
            cliff_ledger: 200,
            end_ledger: 100,
        });

        assert_eq!(
            client.try_create_schedules_batch(&schedules),
            Err(Ok(VestingError::InvalidLedgerRange.into()))
        );
    }

    #[test]
    fn test_create_schedules_batch_duplicate_recipient_assigns_indexes() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &5000);

        let recipient = Address::generate(&env);
        let mut schedules = Vec::new(&env);
        schedules.push_back(ScheduleInput {
            recipient: recipient.clone(),
            total_amount: 1000,
            cliff_ledger: 100,
            end_ledger: 200,
        });
        schedules.push_back(ScheduleInput {
            recipient: recipient.clone(),
            total_amount: 2000,
            cliff_ledger: 150,
            end_ledger: 250,
        });

        let count = client.create_schedules_batch(&schedules);
        assert_eq!(count, 2);
        assert_eq!(client.get_schedule_count(&recipient), 2);

        let first_schedule = get_schedule_at(&client, &recipient, 0);
        assert_eq!(first_schedule.total_amount, 1000);
        assert_eq!(first_schedule.end_ledger, 200);

        let latest_schedule = get_schedule_latest(&client, &recipient);
        assert_eq!(latest_schedule.total_amount, 2000);
        assert_eq!(latest_schedule.end_ledger, 250);
    }

    #[test]
    fn test_create_schedules_batch_existing_schedule_appends_new_index() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &5000);

        let recipient = Address::generate(&env);

        // Create a schedule first
        client.create_schedule(&recipient, &1000, &100, &200);

        // Try to create batch with same recipient
        let mut schedules = Vec::new(&env);
        schedules.push_back(ScheduleInput {
            recipient: recipient.clone(),
            total_amount: 2000,
            cliff_ledger: 150,
            end_ledger: 250,
        });

        client.create_schedules_batch(&schedules);

        assert_eq!(client.get_schedule_count(&recipient), 2);
        assert_eq!(get_schedule_at(&client, &recipient, 0).total_amount, 1000);
        assert_eq!(get_schedule_at(&client, &recipient, 1).total_amount, 2000);
    }

    #[test]
    fn test_create_schedules_batch_release_works() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &3000);

        let recipient1 = Address::generate(&env);
        let recipient2 = Address::generate(&env);

        let mut schedules = Vec::new(&env);
        schedules.push_back(ScheduleInput {
            recipient: recipient1.clone(),
            total_amount: 1000,
            cliff_ledger: 100,
            end_ledger: 200,
        });
        schedules.push_back(ScheduleInput {
            recipient: recipient2.clone(),
            total_amount: 2000,
            cliff_ledger: 100,
            end_ledger: 200,
        });

        client.create_schedules_batch(&schedules);

        // Test release for both recipients
        env.ledger().set_sequence_number(150);

        release_latest(&client, &recipient1);
        assert_eq!(token_client.balance(&recipient1), 500);

        release_latest(&client, &recipient2);
        assert_eq!(token_client.balance(&recipient2), 1000);
    }

    #[test]
    fn test_multiple_schedules_for_same_recipient_can_be_released_independently() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &3000);

        client.create_schedule(&recipient, &1000, &100, &200);
        client.create_schedule(&recipient, &2000, &150, &250);

        assert_eq!(client.get_schedule_count(&recipient), 2);
        assert_eq!(get_schedule_latest(&client, &recipient).total_amount, 2000);
        assert_eq!(vested_amount_latest(&client, &recipient), 0);

        env.ledger().set_sequence_number(200);

        assert_eq!(vested_amount_at(&client, &recipient, 0), 1000);
        assert_eq!(vested_amount_at(&client, &recipient, 1), 1000);

        release_at(&client, &recipient, 0);
        assert_eq!(released_amount_at(&client, &recipient, 0), 1000);
        assert_eq!(released_amount_latest(&client, &recipient), 0);

        release_latest(&client, &recipient);
        assert_eq!(released_amount_latest(&client, &recipient), 1000);
        assert_eq!(token_client.balance(&recipient), 2000);
    }

    #[test]
    fn test_revoke_can_target_older_schedule_by_index() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &3000);

        client.create_schedule(&recipient, &1000, &100, &200);
        client.create_schedule(&recipient, &2000, &150, &250);

        env.ledger().set_sequence_number(175);
        revoke_at(&client, &recipient, 0);

        assert!(get_schedule_at(&client, &recipient, 0).revoked);
        assert!(!get_schedule_latest(&client, &recipient).revoked);
        assert_eq!(token_client.balance(&recipient), 750);
        assert_eq!(token_client.balance(&admin), 250);
    }

    // ── #359: initialize auth guard ───────────────────────────────────

    #[test]
    #[should_panic]
    fn test_initialize_unauthorized() {
        let env = Env::default();
        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
    }

    #[test]
    fn test_initialize_authorized() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
    }

    // ── Regression tests for issue #324: TTL clamp for long schedules ──
    // ── Upgrade tests ───────────────────────────────────────────
    #[test]
    fn test_upgrade_rejects_zero_hash() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);

        let zero_hash = BytesN::from_array(&env, &[0; 32]);
        client.upgrade(&zero_hash);
    }

    #[test]
    fn test_upgrade_success() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);

        let non_zero_hash = BytesN::from_array(&env, &[1; 32]);
        client.upgrade(&non_zero_hash);

        // Verify the contract is still functional after upgrade
        assert_eq!(client.get_admin(), admin);
    }

    #[test]
    #[should_panic]
    fn test_non_admin_cannot_upgrade() {
        let env = Env::default();
        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);

        let non_zero_hash = BytesN::from_array(&env, &[1; 32]);
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &user,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &contract_id,
                fn_name: "upgrade",
                args: (non_zero_hash.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);

        client.upgrade(&non_zero_hash);
    }

    // ── Lock / revoke_admin tests ──────────────────────────────

    #[test]
    fn test_revoke_admin_sets_locked_flag() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);

        assert!(!client.is_locked());
        client.revoke_admin();
        assert!(client.is_locked());
    }

    #[test]
    #[should_panic(expected = "admin revoked")]
    fn test_admin_getter_after_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
        client.revoke_admin();
        let _ = client.get_admin();
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_create_schedule_after_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
        client.revoke_admin();

        let recipient = Address::generate(&env);
        client.create_schedule(&recipient, &1000, &100, &200);
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_revoke_after_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
        client.revoke_admin();
        client.revoke_admin();
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_upgrade_after_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
        client.revoke_admin();

        let non_zero_hash = BytesN::from_array(&env, &[1; 32]);
        client.upgrade(&non_zero_hash);
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_extend_cliff_after_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        client.revoke_admin();
        extend_cliff_latest(&client, &recipient, 150u32);
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_prune_recipient_after_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        client.revoke_admin();
        client.prune_recipient(&recipient);
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_pause_after_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
        client.revoke_admin();
        client.pause();
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_unpause_after_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
        client.revoke_admin();
        client.unpause();
    }

    #[test]
    #[should_panic(expected = "admin revoked: contract is locked")]
    fn test_propose_admin_after_revoke_panics() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);
        client.revoke_admin();

        let other = Address::generate(&env);
        client.propose_admin(&other);
    }

    #[test]
    fn test_holder_release_still_works_after_revoke() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env.register_stellar_asset_contract(admin.clone());
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &1000);

        client.create_schedule(&recipient, &1000, &100, &200);
        client.revoke_admin();

        // Release should still work - holders can claim vested tokens
        env.ledger().set_sequence_number(150);
        release_latest(&client, &recipient);
        assert_eq!(token_client.balance(&recipient), 500);
    }

    #[test]
    fn test_keep_alive_still_works_after_revoke() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);
        let (_, recipient) = setup_schedule(&env, &client);

        client.revoke_admin();

        // keep_alive should still work after revoke
        env.ledger().set_sequence_number(50);
        client.keep_alive(&recipient, &latest_index());
    }

    // ── Upgrade event tests ─────────────────────────────────────

    #[test]
    fn test_upgrade_emits_event_with_new_hash() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);

        let non_zero_hash = BytesN::from_array(&env, &[0xAB; 32]);
        client.upgrade(&non_zero_hash);

        let events = env.events().all();
        let last_event = events.slice(events.len() - 1..);
        assert_eq!(
            last_event,
            soroban_sdk::vec![
                &env,
                (
                    contract_id,
                    (symbol_short!("upgrade"),).into_val(&env),
                    non_zero_hash.into_val(&env)
                )
            ]
        );
    }

    #[test]
    fn test_revoke_admin_emits_event() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);

        client.revoke_admin();

        let events = env.events().all();
        let last_event = events.slice(events.len() - 1..);
        assert_eq!(
            last_event,
            soroban_sdk::vec![
                &env,
                (
                    contract_id,
                    (symbol_short!("revoked"),).into_val(&env),
                    true.into_val(&env)
                )
            ]
        );
    }
    // ── Regression: existing vesting functionality unchanged ─────

    #[test]
    fn test_initialize_still_works() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);

        assert_eq!(client.get_admin(), admin);
        assert_eq!(client.get_token_contract(), token);
        assert!(!client.is_locked());
    }

    #[test]
    fn test_is_locked_default_is_false() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let token = Address::generate(&env);
        client.initialize(&admin, &token);

        assert!(!client.is_locked());
    }

    // ── #360: aggregate getters and release_all ────────────────────────

    #[test]
    fn test_get_all_schedules_returns_all() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &6000);

        client.create_schedule(&recipient, &1000, &100, &200);
        client.create_schedule(&recipient, &2000, &150, &250);
        client.create_schedule(&recipient, &3000, &200, &300);

        let all = client.get_all_schedules(&recipient);
        assert_eq!(all.len(), 3);
        assert_eq!(all.get(0).unwrap().total_amount, 1000);
        assert_eq!(all.get(1).unwrap().total_amount, 2000);
        assert_eq!(all.get(2).unwrap().total_amount, 3000);
    }

    #[test]
    fn test_total_vested_sums_across_schedules() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &6000);

        client.create_schedule(&recipient, &1000, &100, &200);
        client.create_schedule(&recipient, &2000, &150, &250);

        env.ledger().set_sequence_number(175);
        // Schedule 0: (175-100)/(200-100) = 75% of 1000 = 750
        // Schedule 1: (175-150)/(250-150) = 25% of 2000 = 500
        // Total: 1250
        assert_eq!(client.total_vested(&recipient), 1250);
    }

    #[test]
    fn test_total_released_sums_across_schedules() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &6000);

        client.create_schedule(&recipient, &1000, &100, &200);
        client.create_schedule(&recipient, &2000, &150, &250);

        env.ledger().set_sequence_number(175);
        assert_eq!(client.total_released(&recipient), 0);

        release_at(&client, &recipient, 0);
        // Schedule 0 released: 750
        assert_eq!(client.total_released(&recipient), 750);

        release_latest(&client, &recipient);
        // Schedule 1 also released: 500
        assert_eq!(client.total_released(&recipient), 1250);
    }

    #[test]
    fn test_total_releasable_returns_unlocked_balance() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &6000);

        client.create_schedule(&recipient, &1000, &100, &200);
        client.create_schedule(&recipient, &2000, &150, &250);

        env.ledger().set_sequence_number(175);
        // Before any release: releasable == vested
        assert_eq!(client.total_releasable(&recipient), 1250);

        release_at(&client, &recipient, 0);
        // After releasing schedule 0 (750): releasable = 1250 - 750 = 500
        assert_eq!(client.total_releasable(&recipient), 500);
    }

    #[test]
    fn test_release_all_transfers_combined_releasable() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &6000);

        client.create_schedule(&recipient, &1000, &100, &200);
        client.create_schedule(&recipient, &2000, &150, &250);

        env.ledger().set_sequence_number(175);

        client.release_all(&recipient);

        assert_eq!(token_client.balance(&recipient), 1250);
        assert_eq!(released_amount_at(&client, &recipient, 0), 750);
        assert_eq!(released_amount_at(&client, &recipient, 1), 500);
    }

    #[test]
    fn test_release_all_skips_revoked_schedules() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::Client::new(&env, &token_addr);
        let asset_client = soroban_sdk::token::StellarAssetClient::new(&env, &token_addr);

        client.initialize(&admin, &token_addr);
        asset_client.mint(&admin, &6000);

        client.create_schedule(&recipient, &1000, &100, &200);
        client.create_schedule(&recipient, &2000, &150, &250);
        client.create_schedule(&recipient, &3000, &200, &300);

        env.ledger().set_sequence_number(175);

        let balance_before = token_client.balance(&recipient);

        // Revoke schedule at index 1 — recipient gets 500 vested, admin gets 1500 unvested
        revoke_at(&client, &recipient, 1);
        let balance_after_revoke = token_client.balance(&recipient);
        assert_eq!(balance_after_revoke - balance_before, 500);

        client.release_all(&recipient);

        // release_all should only process non-revoked schedules:
        //   Schedule 0 (non-revoked): (175-100)/(200-100) * 1000 = 750
        //   Schedule 1 (revoked): skipped
        //   Schedule 2 (non-revoked, before cliff): 0
        // Total new tokens: 750
        assert_eq!(token_client.balance(&recipient) - balance_after_revoke, 750);
    }

    #[test]
    fn test_total_vested_zero_when_no_schedules() {
        let env = Env::default();
        env.mock_all_auths();

        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(&env, &contract_id);

        let admin = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = Address::generate(&env);

        client.initialize(&admin, &token);

        assert_eq!(client.total_vested(&recipient), 0);
        assert_eq!(client.total_released(&recipient), 0);
        assert_eq!(client.total_releasable(&recipient), 0);

        let all = client.get_all_schedules(&recipient);
        assert_eq!(all.len(), 0);
    }

    // ── Per-recipient schedule cap (#466) ─────────────────────────────────
    //
    // `total_vested`, `total_released`, `total_releasable`, `get_all_schedules`
    // and `release_all` each walk a recipient's whole schedule range. Unbounded,
    // a well-meaning second grant eventually pushes that walk past the compute
    // budget and the aggregate getters become permanently uncallable — so a
    // holder can neither see what they are owed nor claim it. The invariant is
    // therefore enforced where the entries are written, and these tests hold
    // both writers to it.

    /// A vested-and-funded contract, with a fixed generous float so tests can
    /// park a few large schedules without the *token* contract — not the code
    /// under test — being the thing that runs out.
    fn setup_for_cap(env: &Env) -> VestingContractClient<'static> {
        let contract_id = env.register_contract(None, VestingContract);
        let client = VestingContractClient::new(env, &contract_id);

        let admin = Address::generate(env);
        let token_addr = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_client = soroban_sdk::token::StellarAssetClient::new(env, &token_addr);
        token_client.mint(&admin, &100_000_000i128);

        client.initialize(&admin, &token_addr);
        client
    }

    /// A batch of `count` single-token schedules for one recipient.
    fn single_recipient_batch(env: &Env, recipient: &Address, count: u32) -> Vec<ScheduleInput> {
        let mut batch = Vec::new(env);
        for _ in 0..count {
            batch.push_back(ScheduleInput {
                recipient: recipient.clone(),
                total_amount: 1,
                cliff_ledger: 100,
                end_ledger: 200,
            });
        }
        batch
    }

    #[test]
    fn test_create_schedule_rejects_beyond_the_per_recipient_cap() {
        let env = Env::default();
        env.mock_all_auths();

        let client = setup_for_cap(&env);
        let recipient = Address::generate(&env);

        // Fill to exactly the cap, one call at a time, so this also proves the
        // boundary is inclusive: the 50th schedule is stored.
        for _ in 0..MAX_SCHEDULES_PER_RECIPIENT {
            client.create_schedule(&recipient, &1, &100, &200);
        }
        assert_eq!(
            client.get_schedule_count(&recipient),
            MAX_SCHEDULES_PER_RECIPIENT
        );
        let committed = client.total_committed();
        assert!(committed > 0);

        // The 51st is refused rather than stored, and refusing it rolls the
        // whole call back — no schedule, no committed bump, no token moved.
        assert_eq!(
            client.try_create_schedule(&recipient, &1i128, &100u32, &200u32),
            Err(Ok(VestingError::TooManySchedules.into()))
        );
        assert_eq!(
            client.get_schedule_count(&recipient),
            MAX_SCHEDULES_PER_RECIPIENT
        );
        assert_eq!(client.total_committed(), committed);
        assert_eq!(client.get_all_schedules(&recipient).len(), 50);

        // The cap is per recipient, not global: someone else is unaffected.
        let other = Address::generate(&env);
        client.create_schedule(&other, &1, &100, &200);
        assert_eq!(client.get_schedule_count(&other), 1);

        // ...and the aggregates that depend on the cap all still answer at
        // the ceiling, which is the whole point of holding the writer to it.
        let _ = client.total_vested(&recipient);
        let _ = client.total_released(&recipient);
        let _ = client.total_releasable(&recipient);
    }

    #[test]
    fn test_create_schedules_batch_rejects_beyond_the_per_recipient_cap() {
        let env = Env::default();
        env.mock_all_auths();

        let client = setup_for_cap(&env);
        let recipient = Address::generate(&env);

        // 49 by one, leaving exactly one slot, then a batch of two: the batch
        // is what pushes past the ceiling, and it must be rejected whole even
        // though its first entry would have fitted.
        for _ in 0..MAX_SCHEDULES_PER_RECIPIENT - 1 {
            client.create_schedule(&recipient, &1, &100, &200);
        }
        assert_eq!(
            client.get_schedule_count(&recipient),
            MAX_SCHEDULES_PER_RECIPIENT - 1
        );

        let mut batch = Vec::new(&env);
        for _ in 0..2 {
            batch.push_back(ScheduleInput {
                recipient: recipient.clone(),
                total_amount: 1,
                cliff_ledger: 100,
                end_ledger: 200,
            });
        }

        assert_eq!(
            client.try_create_schedules_batch(&batch),
            Err(Ok(VestingError::TooManySchedules.into()))
        );
        // Rolled back entirely: the slot that *could* have been used is still
        // free, and no committed amount leaked.
        assert_eq!(
            client.get_schedule_count(&recipient),
            MAX_SCHEDULES_PER_RECIPIENT - 1
        );
        assert_eq!(client.total_committed(), 49);

        // A single-entry batch that fits is accepted, so the boundary is
        // inclusive from both sides.
        let mut fits = Vec::new(&env);
        fits.push_back(ScheduleInput {
            recipient: recipient.clone(),
            total_amount: 1,
            cliff_ledger: 100,
            end_ledger: 200,
        });
        assert_eq!(client.create_schedules_batch(&fits), 1);
        assert_eq!(
            client.get_schedule_count(&recipient),
            MAX_SCHEDULES_PER_RECIPIENT
        );
    }

    #[test]
    fn test_create_schedules_batch_cannot_use_a_full_batch_to_bypass_the_cap() {
        let env = Env::default();
        env.mock_all_auths();

        // This is the case the cap is easy to get wrong: the batch size limit
        // and the per-recipient cap are both 50, so a recipient who already
        // has even one schedule can be handed 50 more in a single call and
        // quietly end up over the ceiling. Each entry is checked against the
        // index the batch has itself already used up, so entry 50 is the one
        // that trips — not entry 51, which can never exist.
        let client = setup_for_cap(&env);
        let recipient = Address::generate(&env);
        client.create_schedule(&recipient, &1, &100, &200);

        let batch = single_recipient_batch(&env, &recipient, MAX_SCHEDULES_PER_RECIPIENT);
        assert_eq!(
            client.try_create_schedules_batch(&batch),
            Err(Ok(VestingError::TooManySchedules.into()))
        );

        // Nothing from the rejected batch landed: not the 49 entries that
        // would individually have fitted, and no committed amount.
        assert_eq!(client.get_schedule_count(&recipient), 1);
        assert_eq!(client.total_committed(), 1);
        assert_eq!(client.get_all_schedules(&recipient).len(), 1);

        // A fresh recipient with a full 50-entry batch is still accepted, so
        // the cap is exactly `MAX_SCHEDULES_PER_RECIPIENT` and not one short.
        let other = Address::generate(&env);
        let full = single_recipient_batch(&env, &other, MAX_SCHEDULES_PER_RECIPIENT);
        assert_eq!(client.create_schedules_batch(&full), 50);
        assert_eq!(
            client.get_schedule_count(&other),
            MAX_SCHEDULES_PER_RECIPIENT
        );

        // And 49 more would be refused, with the single remaining slot unused.
        let more = single_recipient_batch(&env, &other, 1);
        assert_eq!(
            client.try_create_schedules_batch(&more),
            Err(Ok(VestingError::TooManySchedules.into()))
        );
        assert_eq!(
            client.get_schedule_count(&other),
            MAX_SCHEDULES_PER_RECIPIENT
        );
    }

    // ── get_schedules_paginated (#466) ───────────────────────────────────

    #[test]
    fn test_get_schedules_paginated_walks_the_whole_range() {
        let env = Env::default();
        env.mock_all_auths();

        let client = setup_for_cap(&env);
        let recipient = Address::generate(&env);
        client.create_schedule(&recipient, &1000, &100, &200);
        client.create_schedule(&recipient, &2000, &150, &250);
        client.create_schedule(&recipient, &3000, &200, &300);

        // Page size 2, three entries: a short page, then a full one, then an
        // empty one. Matches how a client should walk it.
        let page0 = client.get_schedules_paginated(&recipient, &0u32, &2u32);
        assert_eq!(page0.len(), 2);
        assert_eq!(page0.get(0).unwrap().total_amount, 1000);
        assert_eq!(page0.get(1).unwrap().total_amount, 2000);

        let page1 = client.get_schedules_paginated(&recipient, &2u32, &2u32);
        assert_eq!(page1.len(), 1);
        assert_eq!(page1.get(0).unwrap().total_amount, 3000);

        // Past the end is empty, not an error.
        assert_eq!(
            client
                .get_schedules_paginated(&recipient, &3u32, &2u32)
                .len(),
            0
        );
        assert_eq!(
            client
                .get_schedules_paginated(&recipient, &u32::MAX, &2u32)
                .len(),
            0
        );

        // Overlapping reads are consistent with the single-call getter.
        let all = client.get_all_schedules(&recipient);
        let mut paged: u32 = 0;
        let mut seen = 0;
        while paged < 3 {
            seen += client
                .get_schedules_paginated(&recipient, &paged, &1u32)
                .len();
            paged += 1;
        }
        assert_eq!(seen, all.len());
    }

    #[test]
    fn test_get_schedules_paginated_clamps_an_oversized_limit() {
        let env = Env::default();
        env.mock_all_auths();

        let client = setup_for_cap(&env);
        let recipient = Address::generate(&env);
        for i in 0..3i128 {
            let amount = 1000 + i;
            client.create_schedule(&recipient, &amount, &100, &200);
        }

        // A caller-supplied limit must not be able to size the host's vector
        // (#469). The clamp cannot show up here because the recipient has only
        // three schedules, so this asserts the limit is harmless; the
        // `MAX_PAGE` ceiling itself is asserted on the recipients getter below,
        // where a large enough population exists to hit it.
        let page = client.get_schedules_paginated(&recipient, &0u32, &4_000_000_000u32);
        assert_eq!(page.len(), 3);
    }

    // ── MAX_PAGE on get_recipients_paginated (#469) ──────────────────────

    /// Register `count` distinct recipients, one single-token schedule each,
    /// returning them in registration order.
    ///
    /// Batched rather than looped: 110 sequential `create_schedule` calls blow
    /// the test budget, which says nothing useful about the getter under test.
    fn register_recipients(env: &Env, client: &VestingContractClient, count: u32) -> Vec<Address> {
        let mut recipients: Vec<Address> = Vec::new(env);
        let mut remaining = count;
        while remaining > 0 {
            let size = remaining.min(50);
            let mut batch: Vec<ScheduleInput> = Vec::new(env);
            for _ in 0..size {
                let recipient = Address::generate(env);
                recipients.push_back(recipient.clone());
                batch.push_back(ScheduleInput {
                    recipient,
                    total_amount: 1,
                    cliff_ledger: 100,
                    end_ledger: 200,
                });
            }
            client.create_schedules_batch(&batch);
            remaining -= size;
        }
        recipients
    }

    #[test]
    fn test_get_recipients_paginated_clamps_an_oversized_limit() {
        let env = Env::default();
        env.mock_all_auths();

        let client = setup_for_cap(&env);

        // More recipients than one page can hold, so the clamp is observable
        // rather than masked by a short list.
        let count = MAX_PAGE + 10;
        let recipients = register_recipients(&env, &client, count);
        assert_eq!(client.get_recipient_count(), count);

        // The oversized request is served as a MAX_PAGE-sized page rather than
        // rejected or honoured — this is the fix: `limit` is the caller's, and
        // it must not decide how much the host allocates.
        let clamped = client.get_recipients_paginated(&0u32, &4_000_000_000u32);
        assert_eq!(clamped.len(), MAX_PAGE);
        for i in 0..MAX_PAGE {
            assert_eq!(clamped.get(i).unwrap(), recipients.get(i).unwrap());
        }

        // An explicit `MAX_PAGE` request behaves identically — the clamp is not
        // a special case, it is the same value.
        assert_eq!(
            client.get_recipients_paginated(&0u32, &MAX_PAGE).len(),
            MAX_PAGE
        );

        // And the remainder is still reachable by paging, so the clamp costs a
        // caller nothing but one extra call. The two pages together account for
        // every recipient, so nothing is dropped by clamping.
        let rest = client.get_recipients_paginated(&MAX_PAGE, &MAX_PAGE);
        assert_eq!(rest.len(), 10);
        assert_eq!(rest.get(0).unwrap(), recipients.get(MAX_PAGE).unwrap());

        let all: u32 = (0..=1)
            .map(|p| {
                client
                    .get_recipients_paginated(&(p * MAX_PAGE), &u32::MAX)
                    .len()
            })
            .sum();
        assert_eq!(all, count);

        // A limit below the ceiling is honoured as-is, so the clamp only ever
        // lowers the page size, never raises it.
        assert_eq!(client.get_recipients_paginated(&0u32, &7u32).len(), 7);
    }
}
