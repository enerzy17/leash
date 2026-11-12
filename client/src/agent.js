// Agent side: the wrapper an AI agent uses to pay through a Leash wallet.
//
// The wrapper is a convenience and a first line of defence. It is not the
// security boundary: an agent that ignores it and signs raw transactions is
// still held by the access key (protocol) and by the contract (policy).

import { Account, KeyPair, teraToGas } from 'near-api-js';
import { eventsOf, returnValue, txHash } from './chain.js';

const WAIT = 'FINAL';
const GAS = teraToGas('50');

/** Thrown when the wrapper declines to sign. Nothing was sent to the chain. */
export class LeashRefusal extends Error {
  constructor(message, details = {}) {
    super(message);
    this.name = 'LeashRefusal';
    Object.assign(this, details);
  }
}

export class LeashAgent {
  /**
   * @param {object} options
   * @param {import('near-api-js').Provider} options.provider
   * @param {string} options.walletId the account the Leash contract is deployed on
   * @param {string} options.secretKey the agent's own key, "ed25519:..."
   * @param {string} [options.expectedPublicKey] refuse to run when the secret key does not derive to this
   */
  constructor({ provider, walletId, secretKey, expectedPublicKey }) {
    this.provider = provider;
    this.walletId = walletId;
    this.publicKey = KeyPair.fromString(secretKey).getPublicKey().toString();
    if (expectedPublicKey && expectedPublicKey !== this.publicKey) {
      throw new LeashRefusal(
        `secret key derives to ${this.publicKey}, expected ${expectedPublicKey}; refusing to sign with an unexpected key`,
        { code: 'unexpected_key' },
      );
    }
    this.account = new Account(walletId, provider, secretKey);
    this.checked = false;
  }

  /**
   * Refuse to operate unless this key is exactly what a Leash agent key should be:
   * a function-call key on the wallet, limited to `pay` on the wallet itself, and
   * registered in the contract. An agent that finds itself holding a full-access
   * key stops here instead of using it.
   */
  async selfCheck() {
    let key;
    try {
      key = await this.provider.viewAccessKey({
        accountId: this.walletId,
        publicKey: this.publicKey,
        finality: 'final',
      });
    } catch (error) {
      throw new LeashRefusal(`this key is not on ${this.walletId} (${error.message})`, { code: 'key_not_found' });
    }
    const permission = key.permission;
    const scope = permission?.FunctionCall ?? permission?.functionCall;
    if (!scope) {
      throw new LeashRefusal(
        'this key is a full-access key; an agent must only hold a function-call key, refusing to use it',
        { code: 'full_access_key' },
      );
    }
    const receiver = scope.receiver_id ?? scope.receiverId;
    const methods = scope.method_names ?? scope.methodNames ?? [];
    if (receiver !== this.walletId || methods.length !== 1 || methods[0] !== 'pay') {
      throw new LeashRefusal(
        `this key is scoped to ${receiver} [${methods.join(', ') || 'any method'}], expected ${this.walletId} [pay]`,
        { code: 'wrong_scope' },
      );
    }
    const registered = await this.policy();
    if (!registered) {
      throw new LeashRefusal('this key has no policy in the contract', { code: 'not_registered' });
    }
    this.checked = true;
    return { receiver, methods, allowance: scope.allowance ?? null, agent: registered };
  }

  /** This key's policy, usage and remaining budget as the contract sees them. */
  policy() {
    return this.provider.callFunction({
      contractId: this.walletId,
      method: 'get_agent',
      args: { public_key: this.publicKey },
      finality: 'final',
    });
  }

  /** Ask the contract what it would decide. Free, signs nothing, changes nothing. */
  check(receiverId, amountYocto) {
    return this.provider.callFunction({
      contractId: this.walletId,
      method: 'check_payment',
      args: { public_key: this.publicKey, receiver_id: receiverId, amount: String(amountYocto) },
      finality: 'final',
    });
  }

  /**
   * Pay through the wallet.
   * - allowed: the payment is sent, returns { outcome: 'allowed', ... }
   * - needs approval: the request is parked on chain for the owner, returns { outcome: 'needs_approval', request_id }
   * - denied: throws LeashRefusal without signing anything, so the agent earns no strike
   */
  async pay(receiverId, amountYocto, memo) {
    if (!this.checked) await this.selfCheck();
    const preview = await this.check(receiverId, amountYocto);
    if (preview.outcome === 'denied') {
      throw new LeashRefusal(`policy would deny this payment: ${preview.reason}`, {
        code: 'policy_denied',
        reason: preview.reason,
        decision: preview,
      });
    }
    return this.payRaw(receiverId, amountYocto, memo);
  }

  /**
   * Sign and send `pay` without any local check. This is what a compromised or
   * prompt-injected agent that bypasses the wrapper would do. The contract still decides.
   */
  async payRaw(receiverId, amountYocto, memo) {
    const outcome = await this.account.callFunctionRaw({
      contractId: this.walletId,
      methodName: 'pay',
      args: { receiver_id: receiverId, amount: String(amountYocto), memo: memo ?? null },
      gas: GAS,
      waitUntil: WAIT,
    });
    const events = eventsOf(outcome);
    return {
      ...returnValue(outcome),
      // The contract answers "allowed" before the transfer settles. If the transfer
      // then fails (for example the receiver account does not exist) the contract
      // gives the budget back and logs payment_failed.
      transfer_failed: events.some((e) => e.event === 'payment_failed'),
      events,
      txHash: txHash(outcome),
    };
  }
}
