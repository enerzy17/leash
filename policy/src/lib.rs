//! Leash policy engine.
//!
//! Pure decision logic for "may this agent key send `amount` to `receiver` right now?".
//! It has no dependency on NEAR, so it can be unit tested with plain `cargo test`
//! on any machine. The wallet contract in `../contract` stores a [`Policy`] and a
//! [`Usage`] per agent key and calls [`evaluate`] on every payment.
//!
//! All amounts are in the smallest unit of the asset (yoctoNEAR for NEAR).
//! In JSON every amount is a decimal string, because a u128 does not fit a JSON number.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Most receivers one policy can list.
pub const MAX_RECEIVERS: usize = 32;

/// What one agent key is allowed to do.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Largest single payment the agent can make without a human.
    #[serde(with = "u128_dec")]
    pub per_tx_limit: u128,
    /// Most the agent can spend inside one window.
    #[serde(with = "u128_dec")]
    pub window_limit: u128,
    /// Window length in milliseconds. The window is fixed, not rolling:
    /// it opens at the first payment and resets once `window_ms` has passed.
    pub window_ms: u64,
    /// Most the agent can ever spend on its own. `None` means no lifetime cap.
    #[serde(default, with = "opt_u128_dec")]
    pub lifetime_limit: Option<u128>,
    /// The only accounts this agent can pay. Anything else is denied.
    pub receivers: Vec<String>,
    /// A payment above `per_tx_limit` and up to this amount is parked for the
    /// owner to approve or reject. `None` means such payments are denied.
    #[serde(default, with = "opt_u128_dec")]
    pub approval_limit: Option<u128>,
    /// Unix time in milliseconds after which the key can no longer pay.
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
    /// Denied payments that trip the key. 0 turns the tripwire off.
    /// Strikes are not cleared by later allowed payments, only by the owner.
    pub max_strikes: u32,
}

/// Running totals for one agent key.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq,
)]
pub struct Usage {
    pub window_start_ms: u64,
    #[serde(with = "u128_dec")]
    pub window_spent: u128,
    #[serde(with = "u128_dec")]
    pub lifetime_spent: u128,
    /// Total paid through owner approvals. Not counted against the limits.
    #[serde(with = "u128_dec")]
    pub approved_spent: u128,
    /// Denied payments that count against the key since the owner last cleared them.
    /// An allowed payment does not clear them: otherwise a thief could keep the
    /// count low by mixing tiny allowed payments in between the denied ones.
    pub strikes: u32,
    pub allowed_count: u64,
    pub denied_count: u64,
}

/// Facts about the moment a payment is evaluated.
#[derive(Clone, Copy, Debug)]
pub struct Context {
    pub now_ms: u64,
    pub wallet_frozen: bool,
    pub agent_frozen: bool,
    /// What the wallet can pay out right now without touching its reserve.
    pub available: u128,
}

/// Why a payment was not simply allowed.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    WalletFrozen,
    AgentFrozen,
    KeyExpired,
    ZeroAmount,
    ReceiverNotAllowed,
    OverPerTxLimit,
    OverApprovalLimit,
    OverWindowLimit,
    OverLifetimeLimit,
    InsufficientFunds,
    TooManyPending,
}

impl Reason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Reason::WalletFrozen => "wallet_frozen",
            Reason::AgentFrozen => "agent_frozen",
            Reason::KeyExpired => "key_expired",
            Reason::ZeroAmount => "zero_amount",
            Reason::ReceiverNotAllowed => "receiver_not_allowed",
            Reason::OverPerTxLimit => "over_per_tx_limit",
            Reason::OverApprovalLimit => "over_approval_limit",
            Reason::OverWindowLimit => "over_window_limit",
            Reason::OverLifetimeLimit => "over_lifetime_limit",
            Reason::InsufficientFunds => "insufficient_funds",
            Reason::TooManyPending => "too_many_pending",
        }
    }

    /// A strike means the agent asked for something its policy forbids.
    /// A frozen wallet or an empty wallet is not the agent's doing, so it is not a strike.
    pub fn is_strike(&self) -> bool {
        !matches!(
            self,
            Reason::WalletFrozen | Reason::AgentFrozen | Reason::InsufficientFunds
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Pay now.
    Allow,
    /// Park the payment and ask the owner.
    Escalate(Reason),
    /// Refuse.
    Deny(Reason),
}

impl Policy {
    /// Rejects policies that cannot mean what the owner intended.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.per_tx_limit == 0 {
            return Err("per_tx_limit must be above zero");
        }
        if self.window_limit < self.per_tx_limit {
            return Err("window_limit must be at least per_tx_limit");
        }
        if self.window_ms == 0 {
            return Err("window_ms must be above zero");
        }
        if let Some(lifetime) = self.lifetime_limit {
            if lifetime < self.per_tx_limit {
                return Err("lifetime_limit must be at least per_tx_limit");
            }
        }
        if let Some(approval) = self.approval_limit {
            if approval <= self.per_tx_limit {
                return Err("approval_limit must be above per_tx_limit");
            }
        }
        if self.receivers.is_empty() {
            return Err("receivers must list at least one account");
        }
        if self.receivers.len() > MAX_RECEIVERS {
            return Err("too many receivers");
        }
        for (i, r) in self.receivers.iter().enumerate() {
            if r.is_empty() {
                return Err("receiver must not be empty");
            }
            if self.receivers[..i].contains(r) {
                return Err("receivers must not repeat");
            }
        }
        Ok(())
    }

    pub fn allows_receiver(&self, receiver: &str) -> bool {
        self.receivers.iter().any(|r| r == receiver)
    }
}

impl Usage {
    /// What has been spent in the window that is open at `now_ms`.
    pub fn spent_in_window(&self, policy: &Policy, now_ms: u64) -> u128 {
        if self.window_open(policy, now_ms) {
            self.window_spent
        } else {
            0
        }
    }

    fn window_open(&self, policy: &Policy, now_ms: u64) -> bool {
        self.window_spent > 0 && now_ms < self.window_start_ms.saturating_add(policy.window_ms)
    }

    /// Room left in the current window, also capped by the lifetime limit.
    pub fn remaining(&self, policy: &Policy, now_ms: u64) -> u128 {
        let window_left = policy
            .window_limit
            .saturating_sub(self.spent_in_window(policy, now_ms));
        match policy.lifetime_limit {
            Some(lifetime) => window_left.min(lifetime.saturating_sub(self.lifetime_spent)),
            None => window_left,
        }
    }

    /// Book an allowed payment.
    pub fn record_allow(&mut self, policy: &Policy, now_ms: u64, amount: u128) {
        if !self.window_open(policy, now_ms) {
            self.window_start_ms = now_ms;
            self.window_spent = 0;
        }
        self.window_spent = self.window_spent.saturating_add(amount);
        self.lifetime_spent = self.lifetime_spent.saturating_add(amount);
        self.allowed_count += 1;
    }

    /// Book a denied payment. Returns true when this denial trips the key.
    pub fn record_deny(&mut self, policy: &Policy, reason: Reason) -> bool {
        self.denied_count += 1;
        if !reason.is_strike() {
            return false;
        }
        self.strikes = self.strikes.saturating_add(1);
        policy.max_strikes > 0 && self.strikes >= policy.max_strikes
    }

    /// Undo an allowed payment whose transfer failed on chain.
    /// `window_start_ms` is the window the payment was booked in. If a new window
    /// has opened since, only the lifetime total is given back: the old window is
    /// gone, and giving its budget to the new one would let a failed payment be
    /// used to carry budget across the boundary.
    pub fn refund(&mut self, amount: u128, window_start_ms: u64) {
        if self.window_start_ms == window_start_ms {
            self.window_spent = self.window_spent.saturating_sub(amount);
        }
        self.lifetime_spent = self.lifetime_spent.saturating_sub(amount);
    }
}

/// Decide one payment. Never changes state.
pub fn evaluate(policy: &Policy, usage: &Usage, ctx: &Context, receiver: &str, amount: u128) -> Verdict {
    if ctx.wallet_frozen {
        return Verdict::Deny(Reason::WalletFrozen);
    }
    if ctx.agent_frozen {
        return Verdict::Deny(Reason::AgentFrozen);
    }
    if let Some(expiry) = policy.expires_at_ms {
        if ctx.now_ms >= expiry {
            return Verdict::Deny(Reason::KeyExpired);
        }
    }
    if amount == 0 {
        return Verdict::Deny(Reason::ZeroAmount);
    }
    if !policy.allows_receiver(receiver) {
        return Verdict::Deny(Reason::ReceiverNotAllowed);
    }
    if amount > policy.per_tx_limit {
        return match policy.approval_limit {
            Some(ceiling) if amount <= ceiling => {
                if amount > ctx.available {
                    Verdict::Deny(Reason::InsufficientFunds)
                } else {
                    Verdict::Escalate(Reason::OverPerTxLimit)
                }
            }
            Some(_) => Verdict::Deny(Reason::OverApprovalLimit),
            None => Verdict::Deny(Reason::OverPerTxLimit),
        };
    }
    let spent = usage.spent_in_window(policy, ctx.now_ms);
    if amount > policy.window_limit.saturating_sub(spent) {
        return Verdict::Deny(Reason::OverWindowLimit);
    }
    if let Some(lifetime) = policy.lifetime_limit {
        if amount > lifetime.saturating_sub(usage.lifetime_spent) {
            return Verdict::Deny(Reason::OverLifetimeLimit);
        }
    }
    if amount > ctx.available {
        return Verdict::Deny(Reason::InsufficientFunds);
    }
    Verdict::Allow
}

/// u128 as a decimal string in JSON.
pub mod u128_dec {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let text = String::deserialize(d)?;
        text.parse::<u128>().map_err(serde::de::Error::custom)
    }
}

/// Optional u128 as a decimal string or null in JSON.
pub mod opt_u128_dec {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<u128>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(x) => s.collect_str(x),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u128>, D::Error> {
        let text = Option::<String>::deserialize(d)?;
        text.map(|t| t.parse::<u128>().map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEAR: u128 = 1_000_000_000_000_000_000_000_000;
    const HOUR: u64 = 3_600_000;

    fn policy() -> Policy {
        Policy {
            per_tx_limit: NEAR,
            window_limit: 3 * NEAR,
            window_ms: 24 * HOUR,
            lifetime_limit: Some(10 * NEAR),
            receivers: vec!["shop.testnet".into(), "api.testnet".into()],
            approval_limit: Some(5 * NEAR),
            expires_at_ms: Some(1_000 * HOUR),
            max_strikes: 3,
        }
    }

    fn ctx(now_ms: u64) -> Context {
        Context {
            now_ms,
            wallet_frozen: false,
            agent_frozen: false,
            available: 100 * NEAR,
        }
    }

    #[test]
    fn allows_a_payment_inside_every_limit() {
        let v = evaluate(&policy(), &Usage::default(), &ctx(HOUR), "shop.testnet", NEAR);
        assert_eq!(v, Verdict::Allow);
    }

    #[test]
    fn allows_exactly_the_per_tx_limit_and_escalates_one_yocto_more() {
        let p = policy();
        let u = Usage::default();
        assert_eq!(evaluate(&p, &u, &ctx(HOUR), "shop.testnet", NEAR), Verdict::Allow);
        assert_eq!(
            evaluate(&p, &u, &ctx(HOUR), "shop.testnet", NEAR + 1),
            Verdict::Escalate(Reason::OverPerTxLimit)
        );
    }

    #[test]
    fn denies_a_receiver_that_is_not_listed() {
        let v = evaluate(&policy(), &Usage::default(), &ctx(HOUR), "attacker.testnet", 1);
        assert_eq!(v, Verdict::Deny(Reason::ReceiverNotAllowed));
    }

    #[test]
    fn receiver_match_is_exact_not_a_suffix_or_prefix() {
        let p = policy();
        let u = Usage::default();
        for r in ["evil.shop.testnet", "shop.testnet.evil", "SHOP.testnet", "shop.testne", ""] {
            assert_eq!(
                evaluate(&p, &u, &ctx(HOUR), r, 1),
                Verdict::Deny(Reason::ReceiverNotAllowed),
                "{r}"
            );
        }
    }

    #[test]
    fn unlisted_receiver_is_denied_even_at_an_amount_that_would_escalate() {
        let v = evaluate(&policy(), &Usage::default(), &ctx(HOUR), "attacker.testnet", 2 * NEAR);
        assert_eq!(v, Verdict::Deny(Reason::ReceiverNotAllowed));
    }

    #[test]
    fn over_per_tx_without_an_approval_limit_is_denied() {
        let mut p = policy();
        p.approval_limit = None;
        let v = evaluate(&p, &Usage::default(), &ctx(HOUR), "shop.testnet", 2 * NEAR);
        assert_eq!(v, Verdict::Deny(Reason::OverPerTxLimit));
    }

    #[test]
    fn above_the_approval_limit_is_denied_not_escalated() {
        let p = policy();
        let u = Usage::default();
        assert_eq!(
            evaluate(&p, &u, &ctx(HOUR), "shop.testnet", 5 * NEAR),
            Verdict::Escalate(Reason::OverPerTxLimit)
        );
        assert_eq!(
            evaluate(&p, &u, &ctx(HOUR), "shop.testnet", 5 * NEAR + 1),
            Verdict::Deny(Reason::OverApprovalLimit)
        );
    }

    #[test]
    fn window_limit_holds_across_several_payments() {
        let p = policy();
        let mut u = Usage::default();
        for i in 0..3 {
            let now = HOUR + i;
            assert_eq!(evaluate(&p, &u, &ctx(now), "shop.testnet", NEAR), Verdict::Allow);
            u.record_allow(&p, now, NEAR);
        }
        assert_eq!(u.remaining(&p, HOUR + 3), 0);
        assert_eq!(
            evaluate(&p, &u, &ctx(HOUR + 3), "shop.testnet", 1),
            Verdict::Deny(Reason::OverWindowLimit)
        );
    }

    #[test]
    fn window_resets_after_window_ms_and_not_one_millisecond_before() {
        let p = policy();
        let mut u = Usage::default();
        let start = 5 * HOUR;
        for _ in 0..3 {
            u.record_allow(&p, start, NEAR);
        }
        let last_ms_of_window = start + p.window_ms - 1;
        assert_eq!(
            evaluate(&p, &u, &ctx(last_ms_of_window), "shop.testnet", NEAR),
            Verdict::Deny(Reason::OverWindowLimit)
        );
        let first_ms_after = start + p.window_ms;
        assert_eq!(evaluate(&p, &u, &ctx(first_ms_after), "shop.testnet", NEAR), Verdict::Allow);
        u.record_allow(&p, first_ms_after, NEAR);
        assert_eq!(u.window_start_ms, first_ms_after);
        assert_eq!(u.window_spent, NEAR);
        assert_eq!(u.lifetime_spent, 4 * NEAR);
    }

    #[test]
    fn lifetime_limit_holds_across_windows() {
        let mut p = policy();
        p.expires_at_ms = None;
        let mut u = Usage::default();
        let mut now = HOUR;
        let mut paid = 0u128;
        loop {
            match evaluate(&p, &u, &ctx(now), "shop.testnet", NEAR) {
                Verdict::Allow => {
                    u.record_allow(&p, now, NEAR);
                    paid += NEAR;
                }
                Verdict::Deny(Reason::OverWindowLimit) => now += p.window_ms,
                Verdict::Deny(Reason::OverLifetimeLimit) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(paid, 10 * NEAR);
        assert_eq!(u.remaining(&p, now), 0);
    }

    #[test]
    fn expired_key_is_denied_from_the_expiry_millisecond_on() {
        let p = policy();
        let u = Usage::default();
        let expiry = p.expires_at_ms.unwrap();
        assert_eq!(evaluate(&p, &u, &ctx(expiry - 1), "shop.testnet", 1), Verdict::Allow);
        assert_eq!(
            evaluate(&p, &u, &ctx(expiry), "shop.testnet", 1),
            Verdict::Deny(Reason::KeyExpired)
        );
    }

    #[test]
    fn frozen_wallet_and_frozen_agent_deny_everything() {
        let p = policy();
        let u = Usage::default();
        let mut c = ctx(HOUR);
        c.wallet_frozen = true;
        assert_eq!(
            evaluate(&p, &u, &c, "shop.testnet", 1),
            Verdict::Deny(Reason::WalletFrozen)
        );
        let mut c = ctx(HOUR);
        c.agent_frozen = true;
        assert_eq!(
            evaluate(&p, &u, &c, "shop.testnet", 1),
            Verdict::Deny(Reason::AgentFrozen)
        );
    }

    #[test]
    fn zero_amount_is_denied() {
        let v = evaluate(&policy(), &Usage::default(), &ctx(HOUR), "shop.testnet", 0);
        assert_eq!(v, Verdict::Deny(Reason::ZeroAmount));
    }

    #[test]
    fn empty_wallet_denies_without_a_strike() {
        let p = policy();
        let mut u = Usage::default();
        let mut c = ctx(HOUR);
        c.available = NEAR - 1;
        let v = evaluate(&p, &u, &c, "shop.testnet", NEAR);
        assert_eq!(v, Verdict::Deny(Reason::InsufficientFunds));
        assert!(!u.record_deny(&p, Reason::InsufficientFunds));
        assert_eq!(u.strikes, 0);
        assert_eq!(u.denied_count, 1);
        assert_eq!(
            evaluate(&p, &u, &c, "shop.testnet", 2 * NEAR),
            Verdict::Deny(Reason::InsufficientFunds)
        );
    }

    #[test]
    fn third_strike_trips_and_allowed_payments_in_between_do_not_clear_strikes() {
        let p = policy();
        let mut u = Usage::default();
        assert!(!u.record_deny(&p, Reason::ReceiverNotAllowed));
        u.record_allow(&p, HOUR, 1);
        assert_eq!(u.strikes, 1, "an allowed payment does not buy a strike back");
        assert!(!u.record_deny(&p, Reason::OverWindowLimit));
        u.record_allow(&p, HOUR, 1);
        u.record_allow(&p, HOUR, 1);
        assert_eq!(u.strikes, 2);
        assert!(u.record_deny(&p, Reason::ReceiverNotAllowed));
        assert_eq!(u.strikes, 3);
        assert_eq!(u.denied_count, 3);
        assert_eq!(u.allowed_count, 3);
    }

    #[test]
    fn tripwire_off_never_trips() {
        let mut p = policy();
        p.max_strikes = 0;
        let mut u = Usage::default();
        for _ in 0..50 {
            assert!(!u.record_deny(&p, Reason::ReceiverNotAllowed));
        }
        assert_eq!(u.strikes, 50);
    }

    #[test]
    fn refund_restores_the_budget_after_a_failed_transfer() {
        let p = policy();
        let mut u = Usage::default();
        u.record_allow(&p, HOUR, NEAR);
        u.record_allow(&p, HOUR, NEAR);
        u.refund(NEAR, HOUR);
        assert_eq!(u.window_spent, NEAR);
        assert_eq!(u.lifetime_spent, NEAR);
        assert_eq!(u.remaining(&p, HOUR), 2 * NEAR);
        u.refund(50 * NEAR, HOUR);
        assert_eq!(u.window_spent, 0);
        assert_eq!(u.lifetime_spent, 0);
    }

    #[test]
    fn a_late_refund_from_an_old_window_does_not_add_budget_to_the_new_window() {
        let p = policy();
        let mut u = Usage::default();
        let old_window = HOUR;
        u.record_allow(&p, old_window, NEAR); // this transfer will fail later
        let new_window = old_window + p.window_ms;
        for _ in 0..3 {
            u.record_allow(&p, new_window, NEAR);
        }
        assert_eq!(u.window_start_ms, new_window);
        u.refund(NEAR, old_window);
        assert_eq!(u.window_spent, 3 * NEAR, "the new window is untouched");
        assert_eq!(u.lifetime_spent, 3 * NEAR, "the lifetime total is corrected");
        assert_eq!(
            evaluate(&p, &u, &ctx(new_window + 1), "shop.testnet", 1),
            Verdict::Deny(Reason::OverWindowLimit)
        );
    }

    #[test]
    fn huge_amounts_do_not_overflow() {
        let p = policy();
        let mut u = Usage::default();
        u.record_allow(&p, HOUR, NEAR);
        let v = evaluate(&p, &u, &ctx(HOUR), "shop.testnet", u128::MAX);
        assert_eq!(v, Verdict::Deny(Reason::OverApprovalLimit));
        let mut p2 = policy();
        p2.per_tx_limit = u128::MAX;
        p2.window_limit = u128::MAX;
        p2.lifetime_limit = None;
        p2.approval_limit = None;
        let mut c = ctx(HOUR);
        c.available = u128::MAX;
        assert_eq!(
            evaluate(&p2, &u, &c, "shop.testnet", u128::MAX),
            Verdict::Deny(Reason::OverWindowLimit)
        );
    }

    #[test]
    fn validate_rejects_policies_that_make_no_sense() {
        let ok = policy();
        assert_eq!(ok.validate(), Ok(()));
        let cases: Vec<(&str, Box<dyn Fn(&mut Policy)>)> = vec![
            ("zero per tx", Box::new(|p| p.per_tx_limit = 0)),
            ("window below per tx", Box::new(|p| p.window_limit = p.per_tx_limit - 1)),
            ("zero window", Box::new(|p| p.window_ms = 0)),
            ("lifetime below per tx", Box::new(|p| p.lifetime_limit = Some(1))),
            ("approval not above per tx", Box::new(|p| p.approval_limit = Some(p.per_tx_limit))),
            ("no receivers", Box::new(|p| p.receivers.clear())),
            ("empty receiver", Box::new(|p| p.receivers.push(String::new()))),
            ("repeated receiver", Box::new(|p| p.receivers.push("shop.testnet".into()))),
            (
                "too many receivers",
                Box::new(|p| p.receivers = (0..=MAX_RECEIVERS).map(|i| format!("r{i}.testnet")).collect()),
            ),
        ];
        for (name, change) in cases {
            let mut p = policy();
            change(&mut p);
            assert!(p.validate().is_err(), "{name} should be rejected");
        }
    }

    #[test]
    fn json_uses_decimal_strings_for_amounts_and_optional_fields_may_be_left_out() {
        let p: Policy = serde_json::from_str(
            r#"{"per_tx_limit":"1000000000000000000000000","window_limit":"3000000000000000000000000",
                "window_ms":86400000,"receivers":["shop.testnet"],"max_strikes":3}"#,
        )
        .unwrap();
        assert_eq!(p.per_tx_limit, NEAR);
        assert_eq!(p.lifetime_limit, None);
        assert_eq!(p.approval_limit, None);
        assert_eq!(p.expires_at_ms, None);
        let text = serde_json::to_string(&policy()).unwrap();
        assert!(text.contains(r#""per_tx_limit":"1000000000000000000000000""#), "{text}");
        assert!(text.contains(r#""approval_limit":"5000000000000000000000000""#), "{text}");
        let back: Policy = serde_json::from_str(&text).unwrap();
        assert_eq!(back, policy());
        assert!(serde_json::from_str::<Policy>(r#"{"per_tx_limit":1}"#).is_err());
    }

    #[test]
    fn borsh_round_trip_keeps_policy_and_usage() {
        let p = policy();
        let bytes = borsh::to_vec(&p).unwrap();
        assert_eq!(Policy::try_from_slice(&bytes).unwrap(), p);
        let mut u = Usage::default();
        u.record_allow(&p, HOUR, NEAR);
        let bytes = borsh::to_vec(&u).unwrap();
        assert_eq!(Usage::try_from_slice(&bytes).unwrap(), u);
    }

    #[test]
    fn reason_strings_match_their_json_form() {
        for r in [
            Reason::WalletFrozen,
            Reason::AgentFrozen,
            Reason::KeyExpired,
            Reason::ZeroAmount,
            Reason::ReceiverNotAllowed,
            Reason::OverPerTxLimit,
            Reason::OverApprovalLimit,
            Reason::OverWindowLimit,
            Reason::OverLifetimeLimit,
            Reason::InsufficientFunds,
            Reason::TooManyPending,
        ] {
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{}\"", r.as_str()));
        }
    }
}
