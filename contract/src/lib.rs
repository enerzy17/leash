//! Leash wallet contract.
//!
//! Deploy it on an account that holds the money. The owner is a different account.
//! Each AI agent gets its own key pair. `add_agent` stores a spending policy for the
//! public key and adds it to this account as a NEAR function-call access key that can
//! call exactly one method, `pay`, on exactly this contract. The protocol itself then
//! refuses anything else that key signs (transfers, deploys, key changes, other methods),
//! and `pay` enforces the policy for what is left.
//!
//! Who can call what:
//! - owner account (`owner_id`): everything that changes configuration or moves money freely
//! - agent key (function-call key, method `pay`): `pay` only
//! - guardian key (function-call key, method `freeze`): `freeze` only
//! - anyone: view methods

use leash_policy::{evaluate, Context, Policy, Reason, Usage, Verdict};
use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::serde_json::{json, Value};
use near_sdk::store::{IterableMap, IterableSet};
use near_sdk::{
    env, is_promise_success, near, require, AccountId, Allowance, BorshStorageKey, Gas, NearToken,
    PanicOnDefault, Promise, PublicKey,
};

const VERSION: &str = "0.1.0";
const MAX_AGENTS: u32 = 32;
const MAX_GUARDIANS: u32 = 8;
const MAX_PENDING_PER_AGENT: usize = 4;
const MAX_LABEL: usize = 64;
const MAX_MEMO: usize = 128;
const PENDING_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// Kept back so function-call keys can still pay for gas after the wallet is spent down.
const GAS_RESERVE: u128 = 500_000_000_000_000_000_000_000; // 0.5 NEAR
const DEFAULT_AGENT_GAS_ALLOWANCE: u128 = 100_000_000_000_000_000_000_000; // 0.1 NEAR
const GUARDIAN_GAS_ALLOWANCE: u128 = 50_000_000_000_000_000_000_000; // 0.05 NEAR
const CALLBACK_GAS: Gas = Gas::from_tgas(5);
/// `pay` refuses to run with less gas than this. Without the floor a caller could
/// attach just enough gas for a plain denial but too little for the denial that
/// trips the key, so the tripping call would run out of gas and leave no trace.
const MIN_PAY_GAS: Gas = Gas::from_tgas(30);

#[derive(BorshSerialize, BorshStorageKey)]
#[borsh(crate = "near_sdk::borsh")]
enum StorageKey {
    Agents,
    Guardians,
    Pending,
}

#[derive(BorshSerialize, BorshDeserialize, Clone)]
#[borsh(crate = "near_sdk::borsh")]
pub struct Agent {
    label: String,
    policy: Policy,
    usage: Usage,
    frozen: bool,
    /// False once the contract has removed the access key from the account.
    key_active: bool,
    created_at_ms: u64,
}

#[derive(BorshSerialize, BorshDeserialize, Clone)]
#[borsh(crate = "near_sdk::borsh")]
pub struct Pending {
    agent: PublicKey,
    receiver_id: AccountId,
    amount: u128,
    memo: Option<String>,
    created_at_ms: u64,
    expires_at_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct AgentView {
    pub public_key: PublicKey,
    pub label: String,
    pub policy: Policy,
    pub usage: Usage,
    pub frozen: bool,
    pub key_active: bool,
    pub created_at_ms: u64,
    /// Room left right now for payments that need no approval.
    pub remaining: U128,
    pub pending_requests: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct PendingView {
    pub request_id: u64,
    pub agent: PublicKey,
    pub receiver_id: AccountId,
    pub amount: U128,
    pub memo: Option<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub expired: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct Info {
    pub version: String,
    pub owner_id: AccountId,
    pub frozen: bool,
    pub agents: u32,
    pub guardians: u32,
    pub pending: u32,
    /// What the wallet can pay out right now without touching its reserve.
    pub available: U128,
}

/// The answer to one `pay` or `check_payment` call.
#[derive(Serialize, Deserialize)]
#[serde(crate = "near_sdk::serde")]
pub struct Decision {
    /// "allowed", "needs_approval" or "denied".
    pub outcome: String,
    pub reason: Option<String>,
    /// Set when the payment was parked for the owner.
    pub request_id: Option<u64>,
    /// Denied payments counted against this key, after this call.
    pub strikes: u32,
    /// True when this call froze the key and removed it from the account.
    pub tripped: bool,
    /// Room left for payments that need no approval, after this call.
    pub remaining: U128,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Leash {
    owner_id: AccountId,
    frozen: bool,
    agents: IterableMap<PublicKey, Agent>,
    guardians: IterableSet<PublicKey>,
    pending: IterableMap<u64, Pending>,
    next_request_id: u64,
}

#[near]
impl Leash {
    #[init]
    pub fn new(owner_id: AccountId) -> Self {
        // The owner must be another account. Agent keys sign as this account,
        // so if this account were the owner an agent would look like the owner.
        require!(
            owner_id != env::current_account_id(),
            "owner must be a different account than the wallet"
        );
        Self {
            owner_id,
            frozen: false,
            agents: IterableMap::new(StorageKey::Agents),
            guardians: IterableSet::new(StorageKey::Guardians),
            pending: IterableMap::new(StorageKey::Pending),
            next_request_id: 1,
        }
    }

    // ---------------------------------------------------------------- agent

    /// The only method an agent key can call. Never panics on a policy refusal:
    /// it returns the decision, so every refusal is recorded on chain and counted.
    pub fn pay(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>) -> Decision {
        let me = env::current_account_id();
        require!(
            env::predecessor_account_id() == me && env::signer_account_id() == me,
            "pay must be signed with an agent key of this wallet"
        );
        require!(env::prepaid_gas() >= MIN_PAY_GAS, "attach at least 30 Tgas to pay");
        if let Some(m) = &memo {
            require!(m.len() <= MAX_MEMO, "memo is too long");
        }
        let key = env::signer_account_pk();
        let mut agent = match self.agents.get(&key) {
            Some(a) => a.clone(),
            None => env::panic_str("this key is not a registered agent"),
        };
        let now_ms = env::block_timestamp_ms();
        let ctx = Context {
            now_ms,
            wallet_frozen: self.frozen,
            agent_frozen: agent.frozen,
            available: self.available(),
        };
        let amount = amount.0;
        let mut verdict = evaluate(&agent.policy, &agent.usage, &ctx, receiver_id.as_str(), amount);
        if let Verdict::Escalate(_) = verdict {
            self.drop_expired_requests(&key, now_ms);
            if self.open_requests(&key) >= MAX_PENDING_PER_AGENT {
                verdict = Verdict::Deny(Reason::TooManyPending);
            }
        }

        let decision = match verdict {
            Verdict::Allow => {
                agent.usage.record_allow(&agent.policy, now_ms, amount);
                self.send(&key, &agent, &receiver_id, amount, false).detach();
                emit(
                    "payment_allowed",
                    json!({ "agent": key, "receiver_id": receiver_id, "amount": amount.to_string(), "memo": memo }),
                );
                decision("allowed", None, None, &agent, now_ms, false)
            }
            Verdict::Escalate(reason) => {
                let request_id = self.next_request_id;
                self.next_request_id += 1;
                self.pending.insert(
                    request_id,
                    Pending {
                        agent: key.clone(),
                        receiver_id: receiver_id.clone(),
                        amount,
                        memo: memo.clone(),
                        created_at_ms: now_ms,
                        expires_at_ms: now_ms + PENDING_TTL_MS,
                    },
                );
                emit(
                    "payment_needs_approval",
                    json!({ "agent": key, "receiver_id": receiver_id, "amount": amount.to_string(),
                            "memo": memo, "request_id": request_id, "reason": reason.as_str() }),
                );
                decision("needs_approval", Some(reason), Some(request_id), &agent, now_ms, false)
            }
            Verdict::Deny(reason) => {
                let tripped = agent.usage.record_deny(&agent.policy, reason) && !agent.frozen;
                emit(
                    "payment_denied",
                    json!({ "agent": key, "receiver_id": receiver_id, "amount": amount.to_string(),
                            "memo": memo, "reason": reason.as_str(), "strikes": agent.usage.strikes }),
                );
                if tripped {
                    agent.frozen = true;
                    if agent.key_active {
                        agent.key_active = false;
                        Promise::new(me).delete_key(key.clone()).detach();
                    }
                    emit(
                        "agent_tripped",
                        json!({ "agent": key, "strikes": agent.usage.strikes, "key_deleted": true }),
                    );
                }
                decision("denied", Some(reason), None, &agent, now_ms, tripped)
            }
        };
        self.agents.insert(key, agent);
        decision
    }

    // ---------------------------------------------------------------- owner

    /// Register an agent key with its policy and add it to this account as a
    /// function-call access key limited to `pay` on this contract.
    /// `gas_allowance` is the most NEAR the key can burn on gas (default 0.1 NEAR).
    pub fn add_agent(
        &mut self,
        public_key: PublicKey,
        label: String,
        policy: Policy,
        gas_allowance: Option<U128>,
    ) -> Promise {
        self.assert_owner();
        require!(label.len() <= MAX_LABEL, "label is too long");
        require!(self.agents.len() < MAX_AGENTS, "too many agents");
        require!(!self.agents.contains_key(&public_key), "this key is already an agent");
        require!(!self.guardians.contains(&public_key), "this key is already a guardian");
        self.check_policy(&policy);
        let allowance = gas_allowance.map(|a| a.0).unwrap_or(DEFAULT_AGENT_GAS_ALLOWANCE);
        require!(allowance > 0, "gas_allowance must be above zero");
        self.agents.insert(
            public_key.clone(),
            Agent {
                label: label.clone(),
                policy,
                usage: Usage::default(),
                frozen: false,
                key_active: true,
                created_at_ms: env::block_timestamp_ms(),
            },
        );
        emit("agent_added", json!({ "agent": public_key, "label": label }));
        let me = env::current_account_id();
        Promise::new(me.clone())
            .add_access_key_allowance(
                public_key.clone(),
                Allowance::limited(NearToken::from_yoctonear(allowance)).unwrap(),
                me.clone(),
                "pay".to_string(),
            )
            .then(
                Self::ext(me)
                    .with_static_gas(CALLBACK_GAS)
                    .on_agent_key_added(public_key),
            )
    }

    /// Replace an agent's policy. Spending totals are kept, strikes are cleared.
    pub fn update_policy(&mut self, public_key: PublicKey, policy: Policy) {
        self.assert_owner();
        self.check_policy(&policy);
        let agent = self
            .agents
            .get_mut(&public_key)
            .unwrap_or_else(|| env::panic_str("no such agent"));
        agent.policy = policy;
        agent.usage.strikes = 0;
        emit("policy_updated", json!({ "agent": public_key }));
    }

    /// Remove an agent: its policy, its parked requests and its access key.
    pub fn revoke_agent(&mut self, public_key: PublicKey) {
        self.assert_owner();
        let agent = self
            .agents
            .remove(&public_key)
            .unwrap_or_else(|| env::panic_str("no such agent"));
        self.drop_requests_of(&public_key);
        if agent.key_active {
            Promise::new(env::current_account_id())
                .delete_key(public_key.clone())
                .detach();
        }
        emit("agent_revoked", json!({ "agent": public_key }));
    }

    /// Pay a parked request. The owner's approval is the authority here, so the
    /// agent's own limits are not applied; the amount is tracked in `approved_spent`.
    pub fn approve(&mut self, request_id: u64) -> Promise {
        self.assert_owner();
        require!(!self.frozen, "wallet is frozen");
        let request = self
            .pending
            .remove(&request_id)
            .unwrap_or_else(|| env::panic_str("no such request"));
        require!(
            env::block_timestamp_ms() < request.expires_at_ms,
            "request has expired"
        );
        require!(request.amount <= self.available(), "not enough funds");
        let agent = self
            .agents
            .get_mut(&request.agent)
            .unwrap_or_else(|| env::panic_str("agent no longer exists"));
        require!(!agent.frozen, "agent is frozen");
        // The policy may have changed since the request was parked.
        require!(
            agent.policy.allows_receiver(request.receiver_id.as_str()),
            "receiver is no longer on the list of this agent"
        );
        if let Some(expiry) = agent.policy.expires_at_ms {
            require!(env::block_timestamp_ms() < expiry, "agent key has expired");
        }
        agent.usage.approved_spent = agent.usage.approved_spent.saturating_add(request.amount);
        let agent = agent.clone();
        emit(
            "payment_approved",
            json!({ "agent": request.agent, "receiver_id": request.receiver_id,
                    "amount": request.amount.to_string(), "request_id": request_id }),
        );
        self.send(&request.agent, &agent, &request.receiver_id, request.amount, true)
    }

    pub fn reject(&mut self, request_id: u64) {
        self.assert_owner();
        let request = self
            .pending
            .remove(&request_id)
            .unwrap_or_else(|| env::panic_str("no such request"));
        emit(
            "payment_rejected",
            json!({ "agent": request.agent, "receiver_id": request.receiver_id,
                    "amount": request.amount.to_string(), "request_id": request_id }),
        );
    }

    /// Add a key that can call `freeze` and nothing else. Meant for a monitor
    /// that should be able to stop all agent spending but never move money.
    pub fn add_guardian(&mut self, public_key: PublicKey) -> Promise {
        self.assert_owner();
        require!(self.guardians.len() < MAX_GUARDIANS, "too many guardians");
        require!(!self.guardians.contains(&public_key), "this key is already a guardian");
        require!(!self.agents.contains_key(&public_key), "this key is already an agent");
        self.guardians.insert(public_key.clone());
        emit("guardian_added", json!({ "guardian": public_key }));
        let me = env::current_account_id();
        Promise::new(me.clone())
            .add_access_key_allowance(
                public_key.clone(),
                Allowance::limited(NearToken::from_yoctonear(GUARDIAN_GAS_ALLOWANCE)).unwrap(),
                me.clone(),
                "freeze".to_string(),
            )
            .then(
                Self::ext(me)
                    .with_static_gas(CALLBACK_GAS)
                    .on_guardian_key_added(public_key),
            )
    }

    pub fn remove_guardian(&mut self, public_key: PublicKey) {
        self.assert_owner();
        require!(self.guardians.remove(&public_key), "no such guardian");
        Promise::new(env::current_account_id())
            .delete_key(public_key.clone())
            .detach();
        emit("guardian_removed", json!({ "guardian": public_key }));
    }

    /// Stop every agent payment. Callable by the owner or by a guardian key.
    pub fn freeze(&mut self) {
        let me = env::current_account_id();
        let caller = env::predecessor_account_id();
        let by_guardian = caller == me
            && env::signer_account_id() == me
            && self.guardians.contains(&env::signer_account_pk());
        require!(
            caller == self.owner_id || by_guardian,
            "only the owner or a guardian key can freeze"
        );
        self.frozen = true;
        emit(
            "wallet_frozen",
            json!({ "by": if by_guardian { "guardian" } else { "owner" } }),
        );
    }

    pub fn unfreeze(&mut self) {
        self.assert_owner();
        self.frozen = false;
        emit("wallet_unfrozen", json!({}));
    }

    /// Move NEAR from the wallet back to the owner.
    pub fn withdraw(&mut self, amount: U128) -> Promise {
        self.assert_owner();
        require!(amount.0 > 0, "amount must be above zero");
        require!(amount.0 <= self.available(), "not enough funds above the reserve");
        emit("withdrawn", json!({ "amount": amount.0.to_string() }));
        Promise::new(self.owner_id.clone()).transfer(NearToken::from_yoctonear(amount.0))
    }

    // ------------------------------------------------------------ callbacks
    // `#[private]` means "predecessor is this account". Agent and guardian keys
    // also sign as this account, but the protocol only lets them call the one
    // method named in their access key, so they cannot reach these.

    #[private]
    pub fn on_paid(
        &mut self,
        public_key: PublicKey,
        receiver_id: AccountId,
        amount: U128,
        approved: bool,
        window_start_ms: u64,
        created_at_ms: u64,
    ) -> bool {
        let ok = is_promise_success();
        if !ok {
            // Only give the budget back to the same registration of this key. If the
            // owner revoked the key and added it again meanwhile, the totals are new.
            if let Some(agent) = self
                .agents
                .get_mut(&public_key)
                .filter(|a| a.created_at_ms == created_at_ms)
            {
                if approved {
                    agent.usage.approved_spent = agent.usage.approved_spent.saturating_sub(amount.0);
                } else {
                    agent.usage.refund(amount.0, window_start_ms);
                }
            }
            emit(
                "payment_failed",
                json!({ "agent": public_key, "receiver_id": receiver_id, "amount": amount.0.to_string() }),
            );
        }
        ok
    }

    #[private]
    pub fn on_agent_key_added(&mut self, public_key: PublicKey) -> bool {
        let ok = is_promise_success();
        if !ok {
            self.agents.remove(&public_key);
            emit("agent_key_rejected", json!({ "agent": public_key }));
        }
        ok
    }

    #[private]
    pub fn on_guardian_key_added(&mut self, public_key: PublicKey) -> bool {
        let ok = is_promise_success();
        if !ok {
            self.guardians.remove(&public_key);
            emit("guardian_key_rejected", json!({ "guardian": public_key }));
        }
        ok
    }

    // ---------------------------------------------------------------- views

    pub fn get_info(&self) -> Info {
        Info {
            version: VERSION.to_string(),
            owner_id: self.owner_id.clone(),
            frozen: self.frozen,
            agents: self.agents.len(),
            guardians: self.guardians.len(),
            pending: self.pending.len(),
            available: U128(self.available()),
        }
    }

    pub fn get_agent(&self, public_key: PublicKey) -> Option<AgentView> {
        self.agents.get(&public_key).map(|a| self.view(&public_key, a))
    }

    pub fn list_agents(&self) -> Vec<AgentView> {
        self.agents.iter().map(|(k, a)| self.view(k, a)).collect()
    }

    pub fn list_guardians(&self) -> Vec<PublicKey> {
        self.guardians.iter().cloned().collect()
    }

    pub fn list_pending(&self) -> Vec<PendingView> {
        let now_ms = env::block_timestamp_ms();
        self.pending
            .iter()
            .map(|(id, p)| PendingView {
                request_id: *id,
                agent: p.agent.clone(),
                receiver_id: p.receiver_id.clone(),
                amount: U128(p.amount),
                memo: p.memo.clone(),
                created_at_ms: p.created_at_ms,
                expires_at_ms: p.expires_at_ms,
                expired: now_ms >= p.expires_at_ms,
            })
            .collect()
    }

    /// What `pay` would answer right now, without paying or recording anything.
    /// Agents call this first so a well-behaved agent never earns a strike.
    pub fn check_payment(&self, public_key: PublicKey, receiver_id: AccountId, amount: U128) -> Decision {
        let agent = match self.agents.get(&public_key) {
            Some(a) => a,
            None => env::panic_str("this key is not a registered agent"),
        };
        let now_ms = env::block_timestamp_ms();
        let ctx = Context {
            now_ms,
            wallet_frozen: self.frozen,
            agent_frozen: agent.frozen,
            available: self.available(),
        };
        let mut verdict = evaluate(&agent.policy, &agent.usage, &ctx, receiver_id.as_str(), amount.0);
        if let Verdict::Escalate(_) = verdict {
            let open = self
                .pending
                .iter()
                .filter(|(_, p)| p.agent == public_key && now_ms < p.expires_at_ms)
                .count();
            if open >= MAX_PENDING_PER_AGENT {
                verdict = Verdict::Deny(Reason::TooManyPending);
            }
        }
        match verdict {
            Verdict::Allow => decision("allowed", None, None, agent, now_ms, false),
            Verdict::Escalate(r) => decision("needs_approval", Some(r), None, agent, now_ms, false),
            Verdict::Deny(r) => decision("denied", Some(r), None, agent, now_ms, false),
        }
    }
}

impl Leash {
    fn assert_owner(&self) {
        require!(
            env::predecessor_account_id() == self.owner_id,
            "only the owner can call this"
        );
    }

    fn check_policy(&self, policy: &Policy) {
        if let Err(problem) = policy.validate() {
            env::panic_str(problem);
        }
        let me = env::current_account_id();
        for r in &policy.receivers {
            let parsed: Result<AccountId, _> = r.parse();
            match parsed {
                Ok(id) => require!(id != me, "the wallet cannot be its own receiver"),
                Err(_) => env::panic_str("receiver is not a valid account id"),
            }
        }
    }

    /// Balance minus what the account must keep for storage and for gas.
    fn available(&self) -> u128 {
        let storage = env::storage_byte_cost()
            .as_yoctonear()
            .saturating_mul(env::storage_usage() as u128);
        env::account_balance()
            .as_yoctonear()
            .saturating_sub(storage.saturating_add(GAS_RESERVE))
    }

    fn send(&self, key: &PublicKey, agent: &Agent, receiver_id: &AccountId, amount: u128, approved: bool) -> Promise {
        Promise::new(receiver_id.clone())
            .transfer(NearToken::from_yoctonear(amount))
            .then(
                Self::ext(env::current_account_id())
                    .with_static_gas(CALLBACK_GAS)
                    .on_paid(
                        key.clone(),
                        receiver_id.clone(),
                        U128(amount),
                        approved,
                        agent.usage.window_start_ms,
                        agent.created_at_ms,
                    ),
            )
    }

    fn open_requests(&self, key: &PublicKey) -> usize {
        self.pending.iter().filter(|(_, p)| &p.agent == key).count()
    }

    fn drop_expired_requests(&mut self, key: &PublicKey, now_ms: u64) {
        let expired: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| &p.agent == key && now_ms >= p.expires_at_ms)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.pending.remove(&id);
        }
    }

    fn drop_requests_of(&mut self, key: &PublicKey) {
        let ids: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| &p.agent == key)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.pending.remove(&id);
        }
    }

    fn view(&self, key: &PublicKey, a: &Agent) -> AgentView {
        let now_ms = env::block_timestamp_ms();
        AgentView {
            public_key: key.clone(),
            label: a.label.clone(),
            policy: a.policy.clone(),
            usage: a.usage.clone(),
            frozen: a.frozen,
            key_active: a.key_active,
            created_at_ms: a.created_at_ms,
            remaining: U128(a.usage.remaining(&a.policy, now_ms)),
            pending_requests: self.open_requests(key) as u32,
        }
    }
}

fn decision(
    outcome: &str,
    reason: Option<Reason>,
    request_id: Option<u64>,
    agent: &Agent,
    now_ms: u64,
    tripped: bool,
) -> Decision {
    Decision {
        outcome: outcome.to_string(),
        reason: reason.map(|r| r.as_str().to_string()),
        request_id,
        strikes: agent.usage.strikes,
        tripped,
        remaining: U128(agent.usage.remaining(&agent.policy, now_ms)),
    }
}

/// NEP-297 event log.
fn emit(event: &str, data: Value) {
    env::log_str(&format!(
        "EVENT_JSON:{}",
        json!({ "standard": "leash", "version": "1.0.0", "event": event, "data": [data] })
    ));
}
