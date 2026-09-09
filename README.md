# logos-monero-wallet-backend

`monero_wallet_backend` — the Monero wallet family's coordinator. It sits between the wallet
surfaces (one GUI, one headless CLI) and the engine (`monero_wallet_core_module`): keeps the wallet registry, drives the
engine's tickets, polls sync and balances into events, normalises history, and orchestrates a
send as **build → review → broadcast**. It holds no key material and caches no password.

Amounts are decimal strings of atomic units (1 XMR = 1e12); `format_xmr` / `parse_xmr` are
exact (no floating point) and `parse_xmr` refuses more than 12 fractional digits rather than
rounding. At most one send is in flight per wallet — a built transaction reserves nothing,
so a second build could spend the same outputs — and a preview older than 120 s expires.

The review step governs **broadcast**, not signing: the engine signed when it built the
preview. Nothing here may describe it as offline signing.

```bash
cargo test --manifest-path rust-lib/Cargo.toml --no-default-features --locked
nix build
```
