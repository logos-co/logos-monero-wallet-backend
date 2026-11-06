# logos-monero-wallet-backend

`monero_wallet_backend` — the Monero wallet family's coordinator. It sits between the wallet
surfaces (one GUI, one headless CLI) and the engine (`monero_wallet_core_module`): keeps the wallet registry, drives the
engine's tickets, polls sync and balances into events, normalises history, and orchestrates a
send as **build → review → broadcast**. It holds no key material and caches no password.

The selected network governs opening a wallet: a registered Stagenet wallet cannot open while
Mainnet is selected. Files discovered on disk have an unknown network until successfully opened;
the registry does not infer a network from the current selector. Open-wallet status, node information,
and address validation use the engine's wallet network.

Amounts are decimal strings of atomic units (1 XMR = 1e12); `format_xmr` / `parse_xmr` are
exact (no floating point) and `parse_xmr` refuses more than 12 fractional digits rather than
rounding. At most one send is in flight per wallet — a built transaction reserves nothing,
so a second build could spend the same outputs — and a preview older than 120 s expires.

The review step governs **broadcast**, not signing: the engine signed when it built the
preview. Nothing here may describe it as offline signing.

**Use Tor for** (`all`, `identifying` or `none`) is the node module's setting, per network;
`tor_use()` relays the active network's, with `torUseChosen` and `existing`, for the first-run
screen. `set_tor_use(mode)` sets it for the active network. It is a
custodian method, like `set_node_config`, and both are refused while a wallet is open ("Close
your open accounts to change this."). `wallet_status` carries the engine's `torUse`, `route`,
`onTor` and `waitingForTor`; while `waitingForTor` is true (`identifying` before the move to Tor,
or a Tor route waiting for Tor), `prepare_send` answers `{ ok: false, code: "waiting_for_tor" }`.

**Sending payments** with your own node is the node module's setting too, per network:
`set_local_sends(via)` stores `servers` (a public node over Tor) or `node` for the active network,
a custodian method refused while a wallet is open. `tor_use()` and `node_health()` carry it as
`sends: { localSends, localSendsChosen, localSendsRecommended, sendNode }` while your own node is on.

```bash
cargo test --manifest-path rust-lib/Cargo.toml --no-default-features --locked
nix build
```
