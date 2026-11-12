// Owner side: deploy a Leash wallet, hand out agent keys, approve, freeze.

import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { Account, actions, KeyPair, nearToYocto, teraToGas } from 'near-api-js';
import { eventsOf, newKeyPair, returnValue, txHash } from './chain.js';

export const DEFAULT_WASM = fileURLToPath(new URL('../../res/leash.wasm', import.meta.url));

const WAIT = 'FINAL';
const GAS = teraToGas('60');

/**
 * Create `<prefix>.<owner>` as a sub-account, fund it, deploy the contract and
 * initialise it with the owner, all in one transaction.
 *
 * The new account gets one temporary full-access key (returned as `setupSecretKey`).
 * Call `lockWallet` when setup is done to delete it. After that the only keys on
 * the account are the function-call keys the contract itself hands out.
 */
export async function deployWallet({ owner, prefix = 'wallet', fundNear = '6', wasmPath = DEFAULT_WASM }) {
  const walletId = `${prefix}.${owner.accountId}`;
  const setup = newKeyPair();
  const code = await readFile(wasmPath);
  const outcome = await owner.signAndSendTransaction({
    receiverId: walletId,
    actions: [
      actions.createAccount(),
      actions.transfer(nearToYocto(fundNear)),
      actions.addFullAccessKey(KeyPair.fromString(setup.secretKey).getPublicKey()),
      actions.deployContract(new Uint8Array(code)),
      actions.functionCall('new', { owner_id: owner.accountId }, GAS, 0n),
    ],
    waitUntil: WAIT,
  });
  return { walletId, setupSecretKey: setup.secretKey, setupPublicKey: setup.publicKey, txHash: txHash(outcome) };
}

/** Delete the temporary full-access key. From here on nobody can sign freely as the wallet. */
export async function lockWallet({ provider, walletId, setupSecretKey }) {
  const wallet = new Account(walletId, provider, setupSecretKey);
  const publicKey = KeyPair.fromString(setupSecretKey).getPublicKey();
  const outcome = await wallet.signAndSendTransaction({
    receiverId: walletId,
    actions: [actions.deleteKey(publicKey)],
    waitUntil: WAIT,
  });
  // Do not take the deletion on trust: read the keys back.
  const { keys } = await new Account(walletId, provider).getAccessKeyList();
  const fullAccess = keys.filter((k) => k.access_key.permission === 'FullAccess');
  if (fullAccess.length > 0) {
    throw new Error(
      `wallet ${walletId} still has ${fullAccess.length} full-access key(s): ${fullAccess.map((k) => k.public_key).join(', ')}`,
    );
  }
  return { txHash: txHash(outcome), keys };
}

export class LeashOwner {
  /**
   * @param {Account} owner the owner account, with its signer
   * @param {string} walletId the account the Leash contract is deployed on
   */
  constructor(owner, walletId) {
    this.owner = owner;
    this.walletId = walletId;
  }

  async #call(methodName, args = {}) {
    const outcome = await this.owner.callFunctionRaw({
      contractId: this.walletId,
      methodName,
      args,
      gas: GAS,
      waitUntil: WAIT,
    });
    return { value: returnValue(outcome), events: eventsOf(outcome), txHash: txHash(outcome) };
  }

  #view(method, args = {}) {
    return this.owner.provider.callFunction({ contractId: this.walletId, method, args, finality: 'final' });
  }

  /**
   * Register an agent key. `policy` amounts are yoctoNEAR as decimal strings:
   * { per_tx_limit, window_limit, window_ms, lifetime_limit?, receivers, approval_limit?, expires_at_ms?, max_strikes }
   */
  addAgent({ publicKey, label, policy, gasAllowance }) {
    return this.#call('add_agent', {
      public_key: publicKey,
      label,
      policy,
      gas_allowance: gasAllowance ? String(gasAllowance) : null,
    });
  }

  updatePolicy(publicKey, policy) {
    return this.#call('update_policy', { public_key: publicKey, policy });
  }

  revokeAgent(publicKey) {
    return this.#call('revoke_agent', { public_key: publicKey });
  }

  approve(requestId) {
    return this.#call('approve', { request_id: requestId });
  }

  reject(requestId) {
    return this.#call('reject', { request_id: requestId });
  }

  addGuardian(publicKey) {
    return this.#call('add_guardian', { public_key: publicKey });
  }

  removeGuardian(publicKey) {
    return this.#call('remove_guardian', { public_key: publicKey });
  }

  freeze() {
    return this.#call('freeze');
  }

  unfreeze() {
    return this.#call('unfreeze');
  }

  withdraw(amountYocto) {
    return this.#call('withdraw', { amount: String(amountYocto) });
  }

  info() {
    return this.#view('get_info');
  }

  agent(publicKey) {
    return this.#view('get_agent', { public_key: publicKey });
  }

  agents() {
    return this.#view('list_agents');
  }

  pending() {
    return this.#view('list_pending');
  }

  guardians() {
    return this.#view('list_guardians');
  }
}

/** A guardian key can do exactly one thing: stop all agent spending. */
export async function freezeAsGuardian({ provider, walletId, guardianSecretKey }) {
  const guardian = new Account(walletId, provider, guardianSecretKey);
  const outcome = await guardian.callFunctionRaw({
    contractId: walletId,
    methodName: 'freeze',
    args: {},
    gas: teraToGas('30'),
    waitUntil: WAIT,
  });
  return { events: eventsOf(outcome), txHash: txHash(outcome) };
}
