# Leash

A wallet contract for NEAR that lets an AI agent spend money without holding a key that can do anything else.

Built for NEAR HACKATHON (1 to 14 November 2026), track **AI Security & Defense**. It also fits Autonomous Business and Agent Economy.

## The problem

An agent that pays for things needs a key. On most chains a key is all or nothing: whoever holds it can move every token, so one leaked key, one poisoned web page or one bad tool call can empty the account. Putting the limits in the agent's own code does not help, because the attacker who has the key does not run your code.

## What Leash does

The money sits in a NEAR account that runs the Leash contract. A separate owner account sets the rules. Each agent gets its own key pair, and three things hold that key in place:

1. **The NEAR protocol.** `add_agent` makes the contract add the agent's public key to the wallet account as a *function-call access key* scoped to one method, `pay`, on the wallet itself. NEAR rejects anything else that key signs before it reaches any contract: transfers, other methods, other contracts, key changes, code deploys, account deletion.
2. **The contract.** `pay` looks up the policy stored for the signing key and decides: allow, park for the owner, or deny.
3. **The tripwire.** Denials are returned, not thrown, so they are written to the chain and counted. After `max_strikes` denials the contract freezes the agent and deletes its key from the account. An allowed payment does not clear strikes; only the owner does, by setting the policy again.

A policy is stored per agent key:

| Field | Meaning |
| --- | --- |
| `receivers` | the only accounts this key can pay |
| `per_tx_limit` | largest single payment without a human |
| `window_limit`, `window_ms` | most the key can spend per window (for example 0.3 NEAR per hour) |
| `lifetime_limit` | most the key can ever spend on its own (optional) |
| `approval_limit` | payments above `per_tx_limit` up to this amount are parked for the owner to approve or reject (optional) |
| `expires_at_ms` | the key stops paying after this time (optional) |
| `max_strikes` | denied payments that kill the key (0 turns it off) |

Who can do what:

| Caller | How it is recognised | Can do |
| --- | --- | --- |
| Owner account | `predecessor == owner_id` | add, update and revoke agents; approve or reject parked payments; add and remove guardians; freeze and unfreeze; withdraw |
| Agent key | function-call key, method `pay` | call `pay`, nothing else |
| Guardian key | function-call key, method `freeze` | stop all agent payments, nothing else (meant for a monitor that must never move money) |
| Anyone | | read-only views |

After setup the wallet account's own full-access key is deleted (`lockWallet`). From then on the account holds only the function-call keys the contract handed out, and the contract code cannot be replaced.

## Try it

You need Node.js 22.22.2 or newer. You do not need Rust or a NEAR account: the compiled contract is in `res/leash.wasm`, and the scripts get a funded testnet account from the public faucet. Everything runs on NEAR testnet.

```sh
npm install
npm run demo
```

Do that and you see one full run, about a minute:

- a wallet is created and funded with 4 testnet NEAR, an agent key is added, the setup key is deleted
- the agent makes two payments inside its policy: `ALLOWED`
- the agent asks for 0.5 NEAR, above its 0.1 limit: `PARKED`, then the owner approves it
- the agent is told by a "poisoned page" to send 3 NEAR to an attacker: `REFUSED` by the agent wrapper before anything is signed
- the attacker uses the stolen key directly: a transfer, an owner method, adding a key and replacing the code are each `REJECTED by the NEAR protocol`
- the attacker calls `pay` three times: `DENIED by the contract`, strike 1, 2, 3, then `TRIPPED`, and the key is gone from the account
- result: `attacker received 0 NEAR`

Every line marked `tx` is a real transaction with an explorer link. The agent in the demo is scripted, so no model or API key is needed. The point is what the key can do, whoever is driving it.

```sh
npm test
```

Do that and you see `pass 27, fail 0` after about 5 minutes. These are end-to-end tests that sign real testnet transactions (`test/e2e.test.js`). Each test name says what it proves.

```sh
cargo test
```

Do that (needs Rust; `rust-toolchain.toml` pins 1.95.0) and you see `23 passed`. These are unit tests of the policy engine (`policy/src/lib.rs`), which has no NEAR dependency.

The faucet gives 10 NEAR per new account and limits how many accounts one IP address can create per day (it stopped us after about ten in one day). If it refuses, or to reuse one account for several runs, set `NEAR_ACCOUNT_ID` and `NEAR_PRIVATE_KEY` to an existing testnet account. The demo needs about 4.1 NEAR and the tests about 5.2 NEAR per run, most of it locked as storage for the deployed contract. A second run on the same account uses suffixed sub-account names so it does not collide with the first. Set `NEAR_RPC_URL` to use another testnet RPC endpoint.

To rebuild the contract yourself: `npm run build:contract` (needs Rust). It runs `cargo build` for `wasm32-unknown-unknown` and then `wasm-opt`, and prints the size and sha256 of `res/leash.wasm`. The committed file is 234,675 bytes, sha256 `958019d5d00f8d689d6ec57c47a314a7c94d5a58438eb0ab111ba53d64b1d3c5`. A rebuild from a clean copy on the build machine (Windows, Rust 1.95.0) gave the same hash; another platform may give a different one.

## A recorded run

The demo video, [docs/leash-demo.mp4](docs/leash-demo.mp4) (53 seconds), is a replay of this run: the terminal log on the left, the wallet's on-chain state on the right. The full terminal log is in [docs/recorded-run.txt](docs/recorded-run.txt) and the timed data in [docs/recorded-run.json](docs/recorded-run.json).

![Last frame of the demo: attacker received 0 NEAR, the agent key is deleted](docs/leash-demo-end.png)

<!-- recorded-run:start -->
- Wallet: [`wallet.leash-probe-1794520670768.testnet`](https://testnet.nearblocks.io/address/wallet.leash-probe-1794520670768.testnet)
- Owner: [`leash-probe-1794520670768.testnet`](https://testnet.nearblocks.io/address/leash-probe-1794520670768.testnet)
- Attacker account (received nothing): [`attacker.leash-probe-1794520670768.testnet`](https://testnet.nearblocks.io/address/attacker.leash-probe-1794520670768.testnet)

| Step | Transaction |
| --- | --- |
| Wallet created, funded, contract deployed and initialised (one transaction) | [`GsKGq8frdA...`](https://testnet.nearblocks.io/txns/GsKGq8frdAhUjBvdBj9JPcNKc3Vwyvc3bV1MqVPWcwth) |
| Agent key added with its policy | [`7dHPuJT5Cj...`](https://testnet.nearblocks.io/txns/7dHPuJT5CjYQW7TzX9hsVf33bmxCmwBETUAwr7UQnWMp) |
| Setup key deleted, wallet locked | [`5iHt1g2NFM...`](https://testnet.nearblocks.io/txns/5iHt1g2NFMhyTHaPjr5dprSk95qKwDSSoBbBNpdTkSrF) |
| Allowed: 0.05 NEAR to shop for "api credits" | [`EzbMPQsDun...`](https://testnet.nearblocks.io/txns/EzbMPQsDunUop5tJAYh5vZoDKGC3Nnjn4Veredt8PLWs) |
| Allowed: 0.08 NEAR to shop for "dataset licence" | [`9m8VhYjyDF...`](https://testnet.nearblocks.io/txns/9m8VhYjyDFg7ZvZHcnaaAW21TFVvpYpgxAXDaWvyaXKa) |
| Parked for the owner: 0.5 NEAR, above the per-payment limit | [`DDnUBVjtaa...`](https://testnet.nearblocks.io/txns/DDnUBVjtaaK4QM2Qjcb2nxMQR2e7doAHZZT1rs3xd7MT) |
| Owner approves the parked payment | [`J9jYV8L1tp...`](https://testnet.nearblocks.io/txns/J9jYV8L1tpvAeZ48JgYbxqYDLmpbFLUi8tHEpAGMa24v) |
| Denied by the contract: 0.1 NEAR to attacker: receiver_not_allowed, strike 1 | [`Dvbx84wnvF...`](https://testnet.nearblocks.io/txns/Dvbx84wnvFM6pQNwXk2SByZcJU72FcfApgUMr4mfa9PU) |
| Denied by the contract: 0.05 NEAR to attacker: receiver_not_allowed, strike 2 | [`22m9sDzKv2...`](https://testnet.nearblocks.io/txns/22m9sDzKv2G4p2zpz38sfLhb9oJ7qGkgsHERKnD3U9nt) |
| Denied by the contract: 0.01 NEAR to attacker: receiver_not_allowed, strike 3 | [`7doFpEXtdL...`](https://testnet.nearblocks.io/txns/7doFpEXtdLZDepHEG74q5j78LkXsvkdR3tzmWCS5bJK9) |

The four attempts that the protocol rejected (direct transfer, owner method, adding a key, replacing the code) and the last attempt with the deleted key have no transaction hash: NEAR refuses them before they are included in a block.
<!-- recorded-run:end -->

## What a stolen agent key can and cannot do

Cannot, because NEAR rejects the transaction before it runs:

- send NEAR anywhere directly
- call any method other than `pay`, on the wallet or on any other contract
- attach NEAR to a call
- add or delete keys, deploy code, delete the account

Cannot, because the contract denies it:

- pay an account that is not on the key's `receivers` list
- pay more than `per_tx_limit` at once without the owner
- pay more than `window_limit` in a window or more than `lifetime_limit` in total
- pay after `expires_at_ms`, while the wallet is frozen, or after the key has tripped

Can, and this is the whole exposure:

- pay the listed receivers up to what is left of the window and lifetime limits
- park up to four approval requests, which the owner sees and can reject
- burn gas up to the key's gas allowance (0.1 NEAR by default, set per key)

## Use it from an agent

```js
import { LeashAgent, LeashRefusal, testnetProvider } from './client/src/index.js';

const agent = new LeashAgent({
  provider: testnetProvider(),
  walletId: 'wallet.alice.testnet',
  secretKey: process.env.AGENT_KEY,          // a function-call key, pay only
  expectedPublicKey: process.env.AGENT_PUBLIC_KEY,
});

await agent.selfCheck();                      // throws if the key is not exactly "pay on this wallet"
try {
  const result = await agent.pay('shop.testnet', '50000000000000000000000', 'api credits');
  // result.outcome is 'allowed' or 'needs_approval'
} catch (error) {
  if (error instanceof LeashRefusal) {
    // the policy would deny it; nothing was signed, no strike was recorded
  }
}
```

The wrapper does three things before signing: it refuses to start if its secret key does not derive to the expected public key, it refuses to run on a key that is full-access or scoped to anything other than `pay` on the wallet, and it asks the contract's `check_payment` view so that a well-behaved agent never sends a payment that would be denied. The wrapper is a convenience. The key scope and the contract are the boundary.

Owner side (`client/src/owner.js`): `deployWallet`, `lockWallet`, and `LeashOwner` with `addAgent`, `updatePolicy`, `revokeAgent`, `approve`, `reject`, `addGuardian`, `freeze`, `unfreeze`, `withdraw` and the views.

## How NEAR fits

Leash depends on things NEAR has at the protocol level:

- **Function-call access keys.** An account can hold many keys, and a key can be limited to named methods on one contract, with a cap on the gas it may burn. The agent's limits are not a convention inside an app, they are enforced by validators.
- **A contract can manage its own account's keys.** The wallet contract adds and deletes access keys on the account it runs on. That is what lets the owner hand out and revoke keys through the contract after the last full-access key is gone, and what lets the tripwire delete a key in the same transaction that detects the abuse.
- **`signer_account_pk`.** The contract can see which key signed the transaction, so the key itself is the agent's identity and the policy is stored per key. No extra login, token or session is needed.
- **Named sub-accounts.** The wallet is `wallet.<owner>`, created, funded, deployed and initialised in one transaction.

The owner is just an account id. It can be a person's account, a multisig, or a DAO contract that calls the owner methods by proposal. Only a plain account owner is tested here.

## Limits and known gaps

- **Testnet only, not audited.** One automated security review was run during the build and its findings were fixed or are listed here. Do not put real money in it as is.
- **Native NEAR only.** No NEP-141 tokens yet, so no stablecoins.
- **Windows are fixed, not rolling.** A key can spend up to twice `window_limit` across a window boundary (end of one window, start of the next).
- **The tripwire catches loud misuse, not careful misuse.** `check_payment` is a public view, so a thief can learn what would be denied without sending anything and never earn a strike. A careful thief is held by the policy limits, not by the tripwire.
- **Strikes do not expire.** An honest agent that uses the wrapper does not earn them, except in a race where the wallet's state changes between the check and the payment. The owner clears them with `update_policy`.
- **Gas is outside the policy.** Each key can burn its gas allowance. The wallet keeps 0.5 NEAR back for gas; several compromised keys together could burn more than that, which would stop agent and guardian transactions until the wallet is topped up. Owner calls are not affected.
- **No upgrade and no owner change.** Once locked, the code is fixed and `owner_id` cannot be changed. If the owner account is lost, the wallet's funds are stuck. The storage stake (about 2.4 NEAR for the contract code) and the 0.5 NEAR reserve cannot be withdrawn.
- **Locking is the owner's step.** Until `lockWallet` runs, the setup key is a full-access key on the wallet. `lockWallet` reads the key list back and fails if any full-access key remains.
- **An approved payment is the owner's decision.** It is not counted against the agent's limits, only tracked in `approved_spent`.
- **The demo agent is scripted.** No language model is involved in the demo or the tests.
- **Tests need the network.** They use the public testnet faucet and RPC. There is no local sandbox run, because the machine this was built on cannot run one.

## Repository layout

| Path | What |
| --- | --- |
| `policy/` | Rust crate, pure policy engine and its 23 unit tests |
| `contract/` | Rust crate, the wallet contract (near-sdk) |
| `res/leash.wasm` | compiled contract, built by `npm run build:contract` |
| `client/src/` | JavaScript library: `LeashOwner`, `LeashAgent`, helpers |
| `scripts/demo.js` | the demo |
| `scripts/build-contract.js` | contract build |
| `test/e2e.test.js` | 27 end-to-end tests on testnet |
| `docs/` | video, log and data of the recorded run |

## Hackathon disclosures

- All code in this repository was written on 12 November 2026, inside the hackathon window. There is no earlier baseline.
- Libraries: [near-sdk](https://crates.io/crates/near-sdk) 5.30.1, [borsh](https://crates.io/crates/borsh), [serde](https://crates.io/crates/serde), [near-api-js](https://www.npmjs.com/package/near-api-js) 7.5.3, [binaryen](https://www.npmjs.com/package/binaryen) (wasm-opt, build only). No template or boilerplate was used.
- The code and text were written with an AI coding agent (Claude Code) directed and reviewed by the author.
- Per rule Q-015: the same author has a second, independent submission to this hackathon. It is **Epoch Ledger** (https://github.com/enerzy17/epoch-ledger), a data-backed investment thesis for NEAR in the Investment Thesis track. The two share no code and no data. See [OTHER_PROJECT.md](OTHER_PROJECT.md).

## License

MIT, see [LICENSE](LICENSE).
