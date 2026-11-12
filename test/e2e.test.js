// End-to-end tests against NEAR testnet. Every test signs real transactions.
//
//   npm test
//
// A fresh funded account comes from the public testnet faucet, or from
// NEAR_ACCOUNT_ID / NEAR_PRIVATE_KEY when both are set. Tests run in order
// and share one wallet, because each deployment locks about 2.4 NEAR of storage.
// One run needs about 5.2 NEAR.

import assert from 'node:assert/strict';
import { before, describe, test } from 'node:test';
import { Account, actions, nearToYocto, teraToGas } from 'near-api-js';
import {
  LeashAgent,
  LeashOwner,
  LeashRefusal,
  deployWallet,
  freezeAsGuardian,
  fundedAccount,
  lockWallet,
  newKeyPair,
  runNames,
  sleep,
  testnetProvider,
} from '../client/src/index.js';

const N = (near) => nearToYocto(near).toString();
const provider = testnetProvider();

let owner; // Account
let ownerId;
let walletId;
let setupSecretKey;
let setupPublicKey;
let leash; // LeashOwner
let shop;
let ghost; // listed in policies but never created
let attacker; // never created, never listed
let stranger; // Account with its own full-access key, not the owner
let strangerSecretKey;

const keys = {
  shopper: newKeyPair(),
  windowed: newKeyPair(),
  tripwire: newKeyPair(),
  expiring: newKeyPair(),
  queue: newKeyPair(),
  guardian: newKeyPair(),
};

function policy(overrides = {}) {
  return {
    per_tx_limit: N('0.1'),
    window_limit: N('0.3'),
    window_ms: 3_600_000,
    lifetime_limit: null,
    receivers: [shop, ghost],
    approval_limit: N('1'),
    expires_at_ms: null,
    max_strikes: 3,
    ...overrides,
  };
}

function agentFor(name) {
  return new LeashAgent({ provider, walletId, secretKey: keys[name].secretKey });
}

async function balanceOf(accountId) {
  const view = await provider.viewAccount({ accountId, finality: 'final' });
  return BigInt(view.amount);
}

async function walletKeys() {
  const list = await new Account(walletId, provider).getAccessKeyList();
  return list.keys;
}

/** Everything an error says, including a contract panic message. */
function text(error) {
  return [error?.message, error?.panicMessage, error?.name, error?.constructor?.name].filter(Boolean).join(' | ');
}

async function chainTimeMs() {
  const block = await provider.viewBlock({ finality: 'final' });
  return Number(BigInt(block.header.timestamp_nanosec) / 1_000_000n);
}

/** Runs `send` and returns the error it must throw. */
async function rejected(send) {
  try {
    await send();
  } catch (error) {
    return error;
  }
  assert.fail('expected the network or the contract to reject this, but it went through');
}

describe('Leash on NEAR testnet', { concurrency: false }, () => {
  before(async () => {
    const funded = await fundedAccount(provider, 'leash-test');
    owner = funded.account;
    ownerId = funded.accountId;
    const names = await runNames(provider, ownerId);
    shop = names.id('shop');
    ghost = names.id('ghost');
    attacker = names.id('attacker');
    const strangerKey = newKeyPair();
    await owner.createSubAccount({ accountOrPrefix: shop, publicKey: newKeyPair().publicKey, nearToTransfer: nearToYocto('0.01') });
    await owner.createSubAccount({ accountOrPrefix: names.id('stranger'), publicKey: strangerKey.publicKey, nearToTransfer: nearToYocto('0.1') });
    stranger = new Account(names.id('stranger'), provider, strangerKey.secretKey);
    strangerSecretKey = strangerKey.secretKey;
    const deployed = await deployWallet({ owner, prefix: `wallet${names.suffix}`, fundNear: '5' });
    walletId = deployed.walletId;
    setupSecretKey = deployed.setupSecretKey;
    setupPublicKey = deployed.setupPublicKey;
    leash = new LeashOwner(owner, walletId);
    console.log(`owner ${ownerId}\nwallet ${walletId}`);
  });

  test('a new wallet reports its owner and is not frozen', async () => {
    const info = await leash.info();
    assert.equal(info.owner_id, ownerId);
    assert.equal(info.frozen, false);
    assert.equal(info.agents, 0);
    assert.ok(BigInt(info.available) > nearToYocto('1.9'), `available ${info.available}`);
  });

  test('the contract cannot be initialised a second time', async () => {
    const error = await rejected(() =>
      stranger.callFunctionRaw({ contractId: walletId, methodName: 'new', args: { owner_id: stranger.accountId } }),
    );
    assert.match(text(error), /already been initialized|already initialized/i);
  });

  test('an account that is not the owner cannot add an agent, approve, unfreeze or withdraw', async () => {
    const calls = [
      ['add_agent', { public_key: newKeyPair().publicKey, label: 'x', policy: policy(), gas_allowance: null }],
      ['approve', { request_id: 1 }],
      ['unfreeze', {}],
      ['freeze', {}],
      ['withdraw', { amount: N('1') }],
      ['revoke_agent', { public_key: keys.shopper.publicKey }],
    ];
    for (const [methodName, args] of calls) {
      const error = await rejected(() => stranger.callFunctionRaw({ contractId: walletId, methodName, args }));
      assert.match(text(error), /only the owner/, methodName);
    }
  });

  test('a policy that makes no sense is rejected', async () => {
    const bad = [
      [policy({ window_limit: N('0.01') }), /window_limit must be at least per_tx_limit/],
      [policy({ receivers: [] }), /receivers must list at least one account/],
      [policy({ approval_limit: N('0.1') }), /approval_limit must be above per_tx_limit/],
      [policy({ receivers: ['Not A Valid Account!'] }), /not a valid account id/],
      [policy({ receivers: [walletId] }), /cannot be its own receiver/],
    ];
    for (const [badPolicy, pattern] of bad) {
      const error = await rejected(() => leash.addAgent({ publicKey: newKeyPair().publicKey, label: 'bad', policy: badPolicy }));
      assert.match(text(error), pattern);
    }
    assert.equal((await leash.info()).agents, 0);
  });

  test('add_agent puts a function-call key on the wallet that can only call pay on the wallet', async () => {
    const added = await leash.addAgent({ publicKey: keys.shopper.publicKey, label: 'shopper', policy: policy() });
    assert.equal(added.value, true);
    assert.equal(added.events[0].event, 'agent_added');
    const key = (await walletKeys()).find((k) => k.public_key === keys.shopper.publicKey);
    assert.ok(key, 'agent key is on the account');
    assert.deepEqual(key.access_key.permission.FunctionCall.method_names, ['pay']);
    assert.equal(key.access_key.permission.FunctionCall.receiver_id, walletId);
    assert.equal(key.access_key.permission.FunctionCall.allowance, N('0.1'));
    const view = await leash.agent(keys.shopper.publicKey);
    assert.equal(view.label, 'shopper');
    assert.equal(view.remaining, N('0.3'));
  });

  test('the same key cannot be registered twice', async () => {
    const error = await rejected(() => leash.addAgent({ publicKey: keys.shopper.publicKey, label: 'again', policy: policy() }));
    assert.match(text(error), /already an agent/);
  });

  test('registering a key that is already on the account is rolled back', async () => {
    // The setup key is a full-access key. Calling it an "agent" must not stick.
    const added = await leash.addAgent({ publicKey: setupPublicKey, label: 'not-an-agent', policy: policy() });
    assert.equal(added.value, false);
    assert.ok(added.events.some((e) => e.event === 'agent_key_rejected'));
    assert.equal(await leash.agent(setupPublicKey), null);
    const key = (await walletKeys()).find((k) => k.public_key === setupPublicKey);
    assert.equal(key.access_key.permission, 'FullAccess');
  });

  test('after lockWallet the account has no full-access key left', async () => {
    await lockWallet({ provider, walletId, setupSecretKey });
    const list = await walletKeys();
    assert.equal(list.length, 1);
    assert.ok(list.every((k) => k.access_key.permission !== 'FullAccess'));
    const error = await rejected(() =>
      new Account(walletId, provider, setupSecretKey).transfer({ receiverId: ownerId, amount: nearToYocto('1') }),
    );
    assert.ok(error, 'the deleted setup key can no longer sign');
  });

  test('the agent wrapper passes its self check, and refuses to run on a full-access key', async () => {
    const check = await agentFor('shopper').selfCheck();
    assert.deepEqual(check.methods, ['pay']);
    const overPrivileged = new LeashAgent({ provider, walletId: stranger.accountId, secretKey: strangerSecretKey });
    await assert.rejects(overPrivileged.selfCheck(), (e) => e instanceof LeashRefusal && e.code === 'full_access_key');
    assert.throws(
      () => new LeashAgent({ provider, walletId, secretKey: keys.shopper.secretKey, expectedPublicKey: keys.windowed.publicKey }),
      (e) => e instanceof LeashRefusal && e.code === 'unexpected_key',
    );
    await assert.rejects(
      new LeashAgent({ provider, walletId, secretKey: newKeyPair().secretKey }).selfCheck(),
      (e) => e instanceof LeashRefusal && e.code === 'key_not_found',
    );
  });

  test('a payment inside the policy is paid and the receiver gets exactly the amount', async () => {
    const before = await balanceOf(shop);
    const result = await agentFor('shopper').pay(shop, N('0.05'), 'api credits');
    assert.equal(result.outcome, 'allowed');
    assert.equal(result.transfer_failed, false);
    assert.equal(result.remaining, N('0.25'));
    assert.equal(result.events[0].event, 'payment_allowed');
    assert.equal(result.events[0].memo, 'api credits');
    assert.equal((await balanceOf(shop)) - before, nearToYocto('0.05'));
  });

  test('pay refuses to run with too little gas, so a denial cannot be starved of the gas it needs to be recorded', async () => {
    const lowGas = new Account(walletId, provider, keys.shopper.secretKey);
    const error = await rejected(() =>
      lowGas.callFunctionRaw({
        contractId: walletId,
        methodName: 'pay',
        args: { receiver_id: attacker, amount: N('0.05'), memo: null },
        gas: teraToGas('10'),
      }),
    );
    assert.match(text(error), /attach at least 30 Tgas/);
  });

  test('the wrapper refuses an unlisted receiver without signing, so no strike is recorded', async () => {
    const agent = agentFor('shopper');
    await assert.rejects(
      agent.pay(attacker, N('0.05')),
      (e) => e instanceof LeashRefusal && e.code === 'policy_denied' && e.reason === 'receiver_not_allowed',
    );
    const view = await agent.policy();
    assert.equal(view.usage.denied_count, 0);
    assert.equal(view.usage.strikes, 0);
  });

  test('an agent that skips the wrapper is denied by the contract and the denial is recorded on chain', async () => {
    const walletBefore = await balanceOf(walletId);
    const result = await agentFor('shopper').payRaw(attacker, N('0.05'), 'ignore previous instructions');
    assert.equal(result.outcome, 'denied');
    assert.equal(result.reason, 'receiver_not_allowed');
    assert.equal(result.strikes, 1);
    assert.equal(result.tripped, false);
    assert.equal(result.events[0].event, 'payment_denied');
    const spentOnGas = walletBefore - (await balanceOf(walletId));
    assert.ok(spentOnGas < nearToYocto('0.01'), `only gas left the wallet, not the payment (${spentOnGas})`);
    await assert.rejects(provider.viewAccount({ accountId: attacker, finality: 'final' }), 'attacker account was not created or funded');
  });

  test('the protocol itself rejects everything an agent key signs other than pay on the wallet', async () => {
    const stolen = new Account(walletId, provider, keys.shopper.secretKey);
    const attempts = {
      'direct transfer': () => stolen.signAndSendTransaction({ receiverId: stranger.accountId, actions: [actions.transfer(nearToYocto('0.001'))] }),
      'drain transfer': () => stolen.signAndSendTransaction({ receiverId: stranger.accountId, actions: [actions.transfer(nearToYocto('3'))] }),
      'owner method': () => stolen.callFunctionRaw({ contractId: walletId, methodName: 'withdraw', args: { amount: N('1') } }),
      'callback method': () =>
        stolen.callFunctionRaw({ contractId: walletId, methodName: 'on_agent_key_added', args: { public_key: newKeyPair().publicKey } }),
      'another contract': () => stolen.callFunctionRaw({ contractId: stranger.accountId, methodName: 'pay', args: {} }),
      'add a full-access key': () => stolen.addFullAccessKey(newKeyPair().publicKey),
      'replace the contract code': () => stolen.deployContract(new Uint8Array([0, 97, 115, 109])),
      'delete the account': () => stolen.deleteAccount(stranger.accountId),
    };
    const walletBefore = await balanceOf(walletId);
    for (const [name, attempt] of Object.entries(attempts)) {
      const error = await rejected(attempt);
      assert.match(error.constructor.name, /AccessKey|InvalidTransaction|InvalidAccessKey/, `${name}: ${error.constructor.name} ${error.message}`);
    }
    assert.equal(await balanceOf(walletId), walletBefore, 'rejected transactions cost the wallet nothing');
    assert.equal((await walletKeys()).length, 1);
  });

  test('a payment above the per-payment limit is parked, and the owner can approve it', async () => {
    const agent = agentFor('shopper');
    const parked = await agent.pay(shop, N('0.5'), 'annual plan');
    assert.equal(parked.outcome, 'needs_approval');
    assert.equal(parked.reason, 'over_per_tx_limit');
    const requestId = parked.request_id;
    const pending = await leash.pending();
    assert.equal(pending.length, 1);
    assert.equal(pending[0].request_id, requestId);
    assert.equal(pending[0].amount, N('0.5'));
    assert.equal(pending[0].memo, 'annual plan');

    const before = await balanceOf(shop);
    const approved = await leash.approve(requestId);
    assert.equal(approved.value, true);
    assert.ok(approved.events.some((e) => e.event === 'payment_approved'));
    assert.equal((await balanceOf(shop)) - before, nearToYocto('0.5'));
    assert.equal((await leash.pending()).length, 0);
    const view = await agent.policy();
    assert.equal(view.usage.approved_spent, N('0.5'));
    assert.equal(view.remaining, N('0.25'), 'an approved payment does not eat the agent budget');
    const again = await rejected(() => leash.approve(requestId));
    assert.match(text(again), /no such request/);
  });

  test('the owner can reject a parked payment and nothing is paid', async () => {
    const parked = await agentFor('shopper').pay(shop, N('0.9'));
    assert.equal(parked.outcome, 'needs_approval');
    const before = await balanceOf(shop);
    const rejectedCall = await leash.reject(parked.request_id);
    assert.ok(rejectedCall.events.some((e) => e.event === 'payment_rejected'));
    assert.equal(await balanceOf(shop), before);
    assert.equal((await leash.pending()).length, 0);
  });

  test('a payment above the approval limit is denied outright', async () => {
    const agent = agentFor('shopper');
    const preview = await agent.check(shop, N('1.5'));
    assert.equal(preview.outcome, 'denied');
    assert.equal(preview.reason, 'over_approval_limit');
  });

  test('a payment to a listed account that does not exist fails and the budget is given back', async () => {
    const agent = agentFor('shopper');
    const before = await agent.policy();
    const result = await agent.pay(ghost, N('0.05'));
    assert.equal(result.outcome, 'allowed');
    assert.equal(result.transfer_failed, true);
    assert.ok(result.events.some((e) => e.event === 'payment_failed'));
    const after = await agent.policy();
    assert.equal(after.usage.window_spent, before.usage.window_spent);
    assert.equal(after.usage.lifetime_spent, before.usage.lifetime_spent);
    assert.equal(after.remaining, before.remaining);
  });

  test('the window limit holds, then resets when the window is over; the lifetime limit does not reset', async () => {
    const windowMs = 25_000;
    await leash.addAgent({
      publicKey: keys.windowed.publicKey,
      label: 'windowed',
      policy: policy({ per_tx_limit: N('0.05'), window_limit: N('0.1'), window_ms: windowMs, lifetime_limit: N('0.15'), approval_limit: null }),
    });
    const agent = agentFor('windowed');
    const first = await agent.pay(shop, N('0.05'));
    assert.equal(first.outcome, 'allowed');
    const second = await agent.pay(shop, N('0.05'));
    assert.equal(second.outcome, 'allowed');
    assert.equal(second.remaining, '0');
    const third = await agent.check(shop, N('0.05'));
    assert.equal(third.outcome, 'denied');
    assert.equal(third.reason, 'over_window_limit');

    const windowStart = (await agent.policy()).usage.window_start_ms;
    while ((await chainTimeMs()) < windowStart + windowMs + 2000) await sleep(2000);
    const fourth = await agent.pay(shop, N('0.05'));
    assert.equal(fourth.outcome, 'allowed', 'a new window opened');
    const fifth = await agent.check(shop, N('0.05'));
    assert.equal(fifth.outcome, 'denied');
    assert.equal(fifth.reason, 'over_lifetime_limit');
    const overPerTx = await agent.check(shop, N('0.06'));
    assert.equal(overPerTx.reason, 'over_per_tx_limit', 'no approval limit means over-limit is a denial');
  });

  test('the tripwire freezes a key after repeated denials and deletes it from the account', async () => {
    await leash.addAgent({ publicKey: keys.tripwire.publicKey, label: 'tripwire', policy: policy({ max_strikes: 2 }) });
    const agent = agentFor('tripwire');
    const ok = await agent.pay(shop, N('0.01'));
    assert.equal(ok.outcome, 'allowed');
    const one = await agent.payRaw(attacker, N('0.1'));
    assert.equal(one.strikes, 1);
    assert.equal(one.tripped, false);
    const two = await agent.payRaw(attacker, N('0.1'));
    assert.equal(two.outcome, 'denied');
    assert.equal(two.strikes, 2);
    assert.equal(two.tripped, true);
    assert.ok(two.events.some((e) => e.event === 'agent_tripped'));

    const view = await leash.agent(keys.tripwire.publicKey);
    assert.equal(view.frozen, true);
    assert.equal(view.key_active, false);
    assert.ok(!(await walletKeys()).some((k) => k.public_key === keys.tripwire.publicKey), 'key is gone from the account');
    const preview = await agent.check(shop, N('0.01'));
    assert.equal(preview.reason, 'agent_frozen');
    const error = await rejected(() => agent.payRaw(shop, N('0.01')));
    assert.ok(error, 'a deleted key cannot even submit a transaction');
    await assert.rejects(agent.selfCheck(), (e) => e instanceof LeashRefusal && e.code === 'key_not_found');
  });

  test('a key with an expiry stops paying once the expiry has passed', async () => {
    const expiresAt = (await chainTimeMs()) + 20_000;
    await leash.addAgent({ publicKey: keys.expiring.publicKey, label: 'expiring', policy: policy({ expires_at_ms: expiresAt }) });
    const agent = agentFor('expiring');
    const early = await agent.pay(shop, N('0.01'));
    assert.equal(early.outcome, 'allowed');
    while ((await chainTimeMs()) < expiresAt + 2000) await sleep(2000);
    const late = await agent.payRaw(shop, N('0.01'));
    assert.equal(late.outcome, 'denied');
    assert.equal(late.reason, 'key_expired');
  });

  test('at most four requests can wait for approval per agent', async () => {
    await leash.addAgent({ publicKey: keys.queue.publicKey, label: 'queue', policy: policy() });
    const agent = agentFor('queue');
    for (let i = 0; i < 4; i += 1) {
      const parked = await agent.pay(shop, N('0.2'));
      assert.equal(parked.outcome, 'needs_approval');
    }
    const fifth = await agent.check(shop, N('0.2'));
    assert.equal(fifth.outcome, 'denied');
    assert.equal(fifth.reason, 'too_many_pending');
    assert.equal((await leash.agent(keys.queue.publicKey)).pending_requests, 4);
  });

  test('a guardian key can freeze the wallet and do nothing else; only the owner can unfreeze', async () => {
    const added = await leash.addGuardian(keys.guardian.publicKey);
    assert.equal(added.value, true);
    const key = (await walletKeys()).find((k) => k.public_key === keys.guardian.publicKey);
    assert.deepEqual(key.access_key.permission.FunctionCall.method_names, ['freeze']);
    assert.deepEqual(await leash.guardians(), [keys.guardian.publicKey]);

    const guardian = new Account(walletId, provider, keys.guardian.secretKey);
    for (const methodName of ['unfreeze', 'pay', 'withdraw', 'approve']) {
      const error = await rejected(() => guardian.callFunctionRaw({ contractId: walletId, methodName, args: {} }));
      assert.match(error.constructor.name, /AccessKey/, methodName);
    }
    // An agent key is not a guardian: it cannot even reach freeze.
    const agentError = await rejected(() =>
      new Account(walletId, provider, keys.shopper.secretKey).callFunctionRaw({ contractId: walletId, methodName: 'freeze', args: {} }),
    );
    assert.match(agentError.constructor.name, /AccessKey/);

    const frozen = await freezeAsGuardian({ provider, walletId, guardianSecretKey: keys.guardian.secretKey });
    assert.equal(frozen.events[0].event, 'wallet_frozen');
    assert.equal(frozen.events[0].by, 'guardian');
    assert.equal((await leash.info()).frozen, true);

    const agent = agentFor('shopper');
    const strikesBefore = (await agent.policy()).usage.strikes;
    const denied = await agent.payRaw(shop, N('0.01'));
    assert.equal(denied.outcome, 'denied');
    assert.equal(denied.reason, 'wallet_frozen');
    assert.equal(denied.strikes, strikesBefore, 'a frozen wallet is not a strike against the agent');
    const approveWhileFrozen = await rejected(() => leash.approve(999));
    assert.match(text(approveWhileFrozen), /wallet is frozen/);

    await leash.unfreeze();
    assert.equal((await leash.info()).frozen, false);
    const paid = await agent.pay(shop, N('0.01'));
    assert.equal(paid.outcome, 'allowed');
    assert.equal(paid.strikes, strikesBefore, 'an allowed payment does not clear earlier strikes');
    assert.ok(strikesBefore >= 1);

    await leash.removeGuardian(keys.guardian.publicKey);
    assert.ok(!(await walletKeys()).some((k) => k.public_key === keys.guardian.publicKey));
  });

  test('update_policy changes what the agent may do, and a request parked under the old policy can no longer be approved', async () => {
    const agent = agentFor('shopper');
    const parked = await agent.pay(shop, N('0.5'), 'parked before the policy change');
    assert.equal(parked.outcome, 'needs_approval');
    await leash.updatePolicy(keys.shopper.publicKey, policy({ receivers: [stranger.accountId], per_tx_limit: N('0.02'), approval_limit: null }));
    assert.equal((await agent.policy()).usage.strikes, 0, 'only the owner clears strikes, by setting a policy');
    assert.equal((await agent.check(shop, N('0.01'))).reason, 'receiver_not_allowed');
    assert.equal((await agent.check(stranger.accountId, N('0.03'))).reason, 'over_per_tx_limit');
    const paid = await agent.pay(stranger.accountId, N('0.02'));
    assert.equal(paid.outcome, 'allowed');
    const stale = await rejected(() => leash.approve(parked.request_id));
    assert.match(text(stale), /no longer on the list/);
    await leash.reject(parked.request_id);
  });

  test('revoke_agent removes the policy, the parked requests and the access key', async () => {
    assert.equal((await leash.pending()).length, 4);
    await leash.revokeAgent(keys.queue.publicKey);
    assert.equal(await leash.agent(keys.queue.publicKey), null);
    assert.equal((await leash.pending()).length, 0);
    assert.ok(!(await walletKeys()).some((k) => k.public_key === keys.queue.publicKey));
    const error = await rejected(() => agentFor('queue').payRaw(shop, N('0.01')));
    assert.ok(error);
  });

  test('the owner can withdraw down to the reserve and not below it', async () => {
    const info = await leash.info();
    const tooMuch = await rejected(() => leash.withdraw(BigInt(info.available) + nearToYocto('0.1')));
    assert.match(text(tooMuch), /not enough funds above the reserve/);
    const before = await balanceOf(ownerId);
    await leash.withdraw(nearToYocto('1'));
    const gained = (await balanceOf(ownerId)) - before;
    assert.ok(gained > nearToYocto('0.99') && gained <= nearToYocto('1'), `owner gained ${gained}`);
  });

  test('with every agent gone or bounded, the wallet still holds only function-call keys', async () => {
    const list = await walletKeys();
    assert.ok(list.length >= 1);
    for (const k of list) {
      assert.notEqual(k.access_key.permission, 'FullAccess');
      assert.equal(k.access_key.permission.FunctionCall.receiver_id, walletId);
    }
  });
});
