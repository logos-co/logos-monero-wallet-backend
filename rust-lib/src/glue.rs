//! Logos module glue for `monero_wallet_backend` (rust-first authoring).
//!
//! `concurrency: "multi"`: every method takes `&self`; state lives behind a `Mutex`. A reactor
//! thread drives the engine's tickets, polls sync/balances into events, and expires stale
//! send previews — so the UIs poll for startup state and subscribe only for steady-state
//! updates. Structured values cross as JSON strings: `{ ok, ... }` / `{ ok: false, error }`.
//!
//! The UI never sees a core receipt: this module tracks `(jobId, receipt)` and hands the UI
//! its own job id, so a job id that crosses the event plane authorises nothing.

use std::collections::HashMap;
use std::sync::Arc;
use logos_rust_sdk::LogosError;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::model::{format_xmr, is_network, normalize_history, parse_xmr, Registry, SendState, Sends, WalletMeta, NETWORKS};

pub trait MoneroWalletBackendModule: Send + Sync + 'static {
    /// `{ ok, networks: [...], active }`.
    fn list_networks(&self) -> String;
    /// Refused while a wallet is open or a send is in flight.
    fn set_active_network(&self, network: String) -> String;

    /// `{ ok, wallets: [{ name, network, label, viewOnly, restoreHeight, address }] }` — the
    /// registry merged with what the engine finds on disk.
    fn list_wallets(&self) -> String;
    /// Open a registered wallet on the active network. `{ ok, jobId }`; poll job_status.
    fn open_wallet(&self, name: String, password: String) -> String;
    /// `{ ok, jobId }`. The wallet is registered on the active network.
    fn create_wallet(&self, name: String, password: String, label: String) -> String;
    /// `params_json`: `{ name, password, seed, restoreHeight, seedOffset?, label? }` → `{ ok, jobId }`.
    fn restore_from_seed(&self, params_json: String) -> String;
    /// `params_json`: `{ name, password, address, viewKey, spendKey?, restoreHeight, label? }` → `{ ok, jobId }`.
    fn restore_from_keys(&self, params_json: String) -> String;
    /// `{ ok, jobId }`.
    fn close_wallet(&self) -> String;
    fn change_password(&self, old_password: String, new_password: String) -> String;
    /// Pass-through to the engine, which re-checks the password. Never cached here.
    fn reveal_seed(&self, password: String) -> String;
    fn reveal_view_key(&self, password: String) -> String;
    /// `{ ok, state: queued|running|done|failed, result?, error? }` for a backend job id.
    fn job_status(&self, job_id: String) -> String;

    /// The engine's status plus `syncPercent` and the registry's meta for the open wallet.
    fn wallet_status(&self) -> String;
    /// `{ ok, balance, unlocked, balanceXmr, unlockedXmr }` — atomic units as decimal strings.
    fn balances(&self, account_index: i64) -> String;
    /// `{ ok, address, subaddresses: [{ index, address, label }] }`.
    fn receive_info(&self, account_index: i64) -> String;
    fn create_subaddress(&self, account_index: i64, label: String) -> String;
    /// `{ ok, rows: [...] }`, newest first, amounts as decimal strings + XMR strings.
    fn history(&self) -> String;
    fn address_valid(&self, address: String) -> bool;
    /// The node module's health for the active network.
    fn node_health(&self) -> String;

    /// Build a transaction for review. `send_json`: `{ address, amountXmr | amount, priority?, accountIndex? }`.
    /// `{ ok, requestId }`; poll send_status for the preview. At most one send in flight.
    fn prepare_send(&self, send_json: String) -> String;
    /// `{ ok, requestId, state: preparing|previewed|committing|sent|failed|cancelled, preview?, txids?, error? }`.
    fn send_status(&self, request_id: String) -> String;
    /// Broadcast a previewed transaction. This governs BROADCAST — the engine already signed
    /// when it built the preview.
    fn confirm_send(&self, request_id: String) -> String;
    fn cancel_send(&self, request_id: String) -> String;

    fn format_xmr(&self, atomic: String) -> String;
    fn parse_xmr(&self, xmr: String) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

pub trait MoneroWalletBackendModuleEvents {
    /// The engine's state moved; payload is wallet_status()'s shape.
    fn wallet_state_changed(&self, payload: String);
    /// `{ walletHeight, daemonHeight, percent, synchronized }`, on every change.
    fn sync_progress(&self, payload: String);
    /// `{ balance, unlocked }` for account 0, on every change.
    fn balance_changed(&self, payload: String);
    fn send_status_changed(&self, request_id: String, state: String);
    fn job_finished(&self, job_id: String, state: String);
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

struct TrackedJob {
    kind: String,
    core: (String, String),
    state: String,
    result: Value,
    error: String,
    wallet: String,
    meta: Option<WalletMeta>,
}

struct Inner {
    registry: Registry,
    settings_path: Option<std::path::PathBuf>,
    active: String,
    jobs: HashMap<String, TrackedJob>,
    next_job: u64,
    sends: Sends,
    last_state: String,
    last_sync: Value,
    last_balance: (String, String),
}

struct MoneroWalletBackendModuleImpl {
    inner: Arc<Mutex<Inner>>,
    reactor: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for MoneroWalletBackendModuleImpl {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                registry: Registry::new(None), settings_path: None, active: "stagenet".into(),
                jobs: HashMap::new(), next_job: 1, sends: Sends::default(),
                last_state: String::new(), last_sync: Value::Null, last_balance: (String::new(), String::new()),
            })),
            reactor: Mutex::new(None),
        }
    }
}

fn ok(v: Value) -> String { let mut m = v; m["ok"] = json!(true); m.to_string() }
fn err(e: impl std::fmt::Display) -> String { json!({ "ok": false, "error": e.to_string() }).to_string() }

fn core_json(r: Result<Value, LogosError>) -> Result<Value, String> {
    r.map_err(|e| format!("wallet core: {e:?}"))
}

/// The engine's `result`-typed replies come back as the StdLogosResult envelope; unwrap one.
fn unwrap_result(v: Value) -> Result<Value, String> {
    if let Some(s) = v.get("success").and_then(Value::as_bool) {
        if s { return Ok(v.get("value").cloned().unwrap_or(Value::Null)); }
        return Err(v.get("error").and_then(Value::as_str).unwrap_or("engine error").to_string());
    }
    if let Some(o) = v.get("ok").and_then(Value::as_bool) {
        if o { return Ok(v.get("result").cloned().unwrap_or(v.clone())); }
        return Err(v.get("error").and_then(Value::as_str).unwrap_or("engine error").to_string());
    }
    Ok(v)
}

impl MoneroWalletBackendModuleImpl {
    fn start_core_job(&self, kind: &str, params: Value, wallet: &str, meta: Option<WalletMeta>) -> String {
        let core = modules().monero_wallet_core_module;
        let v = match core_json(core.start_job(kind, &params)).and_then(unwrap_result) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        let (Some(id), Some(receipt)) = (v.get("jobId").and_then(Value::as_str), v.get("receipt").and_then(Value::as_str)) else {
            return err("engine returned no job id");
        };
        let mut g = self.inner.lock().unwrap();
        let jid = format!("b{}", g.next_job); g.next_job += 1;
        g.jobs.insert(jid.clone(), TrackedJob {
            kind: kind.into(), core: (id.into(), receipt.into()), state: "queued".into(),
            result: Value::Null, error: String::new(), wallet: wallet.into(), meta,
        });
        ok(json!({ "jobId": jid }))
    }

    fn persist_settings(g: &Inner) {
        if let Some(p) = g.settings_path.as_ref() {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, json!({ "activeNetwork": g.active }).to_string()).is_ok() {
                let _ = std::fs::rename(&tmp, p).map_err(|e| eprintln!("monero_wallet_backend: settings persist failed: {e}"));
            }
        }
    }

    /// One reactor turn: advance tracked jobs, then sync/balance, then send expiry.
    fn reactor_tick(inner: &Arc<Mutex<Inner>>, slow: bool) {
        let core = modules().monero_wallet_core_module;

        // 1) Tracked jobs.
        let pending: Vec<(String, (String, String))> = {
            let g = inner.lock().unwrap();
            g.jobs.iter().filter(|(_, j)| j.state == "queued" || j.state == "running")
                .map(|(id, j)| (id.clone(), j.core.clone())).collect()
        };
        for (jid, (cid, receipt)) in pending {
            let st = core.job_status(&cid, &receipt).unwrap_or(Value::Null);
            let state = st.get("state").and_then(Value::as_str).unwrap_or("").to_string();
            if state == "done" || state == "failed" {
                let outcome = if state == "done" {
                    core_json(core.job_result(&cid, &receipt)).and_then(unwrap_result)
                } else {
                    Err(st.get("error").and_then(Value::as_str).unwrap_or("job failed").to_string())
                };
                let _ = core.ack_job(&cid, &receipt);
                let mut g = inner.lock().unwrap();
                let (kind, wallet, meta) = match g.jobs.get(&jid) {
                    Some(j) => (j.kind.clone(), j.wallet.clone(), j.meta.clone()),
                    None => continue,
                };
                let final_state;
                match outcome {
                    Ok(res) => {
                        final_state = "done";
                        // A wallet that opened is registered/refreshed with what the engine learned.
                        if kind == "open_wallet" || kind == "create_wallet" || kind == "restore_from_seed" || kind == "restore_from_keys" {
                            let mut m = meta.or_else(|| g.registry.get(&wallet).cloned()).unwrap_or_default();
                            if m.network.is_empty() { m.network = g.active.clone(); }
                            if let Some(a) = res.get("address").and_then(Value::as_str) { m.address = a.into(); }
                            if let Some(v) = res.get("watchOnly").and_then(Value::as_bool) { m.view_only = v; }
                            let _ = g.registry.upsert(&wallet, m).map_err(|e| eprintln!("monero_wallet_backend: registry persist failed: {e}"));
                        }
                        // Send jobs advance the send state machine.
                        if kind == "create_transaction" || kind == "commit_transaction" {
                            let rid = wallet.clone();   // the send request id rides in `wallet`
                            let mut changed = None;
                            if let Some(s) = g.sends.get_mut(&rid) {
                                if kind == "create_transaction" {
                                    s.tx_handle = res.get("txHandle").and_then(Value::as_str).map(String::from);
                                    let amount = res.get("amount").and_then(Value::as_str).unwrap_or("0").to_string();
                                    let fee = res.get("fee").and_then(Value::as_str).unwrap_or("0").to_string();
                                    let total = amount.parse::<u64>().unwrap_or(0).saturating_add(fee.parse::<u64>().unwrap_or(0)).to_string();
                                    s.preview = Some(json!({
                                        "destination": res.get("destination").cloned().unwrap_or(json!("")),
                                        "amount": amount, "amountXmr": format_xmr(&amount).unwrap_or_default(),
                                        "fee": fee, "feeXmr": format_xmr(&fee).unwrap_or_default(),
                                        "total": total, "totalXmr": format_xmr(&total).unwrap_or_default(),
                                        "txCount": res.get("txCount").cloned().unwrap_or(json!(1)),
                                        "txids": res.get("txids").cloned().unwrap_or(json!("")),
                                    }));
                                    s.state = SendState::Previewed;
                                    s.prepared_at = Some(Instant::now());
                                } else {
                                    s.txids = res.get("txids").and_then(Value::as_str).map(String::from);
                                    s.state = SendState::Sent;
                                }
                                changed = Some((rid.clone(), s.state.name().to_string()));
                            }
                            if let Some((r, st)) = changed { emit_send_status_changed(&r, &st); }
                        }
                        if let Some(j) = g.jobs.get_mut(&jid) { j.state = "done".into(); j.result = res; }
                    }
                    Err(e) => {
                        final_state = "failed";
                        if kind == "create_transaction" || kind == "commit_transaction" {
                            let rid = wallet.clone();
                            if let Some(s) = g.sends.get_mut(&rid) { s.state = SendState::Failed(e.clone()); }
                            emit_send_status_changed(&rid, "failed");
                        }
                        if let Some(j) = g.jobs.get_mut(&jid) { j.state = "failed".into(); j.error = e; }
                    }
                }
                drop(g);
                emit_job_finished(&jid, final_state);
            } else if !state.is_empty() {
                let mut g = inner.lock().unwrap();
                if let Some(j) = g.jobs.get_mut(&jid) { j.state = state; }
            }
        }

        // 2) Sync + balance, on the slow tick.
        if slow {
            if let Ok(st) = core.status() {
                let state = st.get("state").and_then(Value::as_str).unwrap_or("").to_string();
                let wh = st.get("walletHeight").and_then(Value::as_u64).unwrap_or(0);
                let dh = st.get("daemonHeight").and_then(Value::as_u64).unwrap_or(0);
                let synced = st.get("synchronized").and_then(Value::as_bool).unwrap_or(false);
                let percent = if dh == 0 { 0 } else { ((wh.min(dh) as u128 * 100) / dh as u128) as u64 };
                let sync = json!({ "walletHeight": wh, "daemonHeight": dh, "percent": percent, "synchronized": synced });
                let (state_changed, sync_changed) = {
                    let mut g = inner.lock().unwrap();
                    let sc = g.last_state != state; if sc { g.last_state = state.clone(); }
                    let yc = g.last_sync != sync; if yc { g.last_sync = sync.clone(); }
                    (sc, yc)
                };
                if state_changed { emit_wallet_state_changed(&st.to_string()); }
                if sync_changed { emit_sync_progress(&sync.to_string()); }
                if state == "ready" || state == "syncing" {
                    let b = core.balance(0).unwrap_or_else(|_| "0".into());
                    let u = core.unlocked_balance(0).unwrap_or_else(|_| "0".into());
                    let changed = { let mut g = inner.lock().unwrap(); let c = g.last_balance != (b.clone(), u.clone()); if c { g.last_balance = (b.clone(), u.clone()); } c };
                    if changed { emit_balance_changed(&json!({ "balance": b, "unlocked": u }).to_string()); }
                }
            }
            let expired = { let mut g = inner.lock().unwrap(); g.sends.expire(Instant::now()) };
            for r in expired { emit_send_status_changed(&r, "failed"); }
        }
    }
}

impl MoneroWalletBackendModule for MoneroWalletBackendModuleImpl {
    fn on_context_ready(&self, ctx: &RustModuleContext) {
        let base = std::path::Path::new(&ctx.instance_persistence_path);
        {
            let mut g = self.inner.lock().unwrap();
            g.registry = Registry::new(Some(base.join("wallets.json")));
            let sp = base.join("settings.json");
            if let Ok(s) = std::fs::read_to_string(&sp) {
                if let Ok(v) = serde_json::from_str::<Value>(&s) {
                    if let Some(n) = v.get("activeNetwork").and_then(Value::as_str) { if is_network(n) { g.active = n.into(); } }
                }
            }
            g.settings_path = Some(sp);
        }
        let inner = Arc::clone(&self.inner);
        let handle = std::thread::spawn(move || {
            let mut n: u64 = 0;
            loop {
                std::thread::sleep(Duration::from_millis(1000));
                n += 1;
                Self::reactor_tick(&inner, n % 3 == 0);
            }
        });
        *self.reactor.lock().unwrap() = Some(handle);
    }

    fn list_networks(&self) -> String {
        let g = self.inner.lock().unwrap();
        ok(json!({ "networks": NETWORKS, "active": g.active }))
    }

    fn set_active_network(&self, network: String) -> String {
        if !is_network(&network) { return err(format!("unknown network: {network}")); }
        let st = modules().monero_wallet_core_module.status().unwrap_or(Value::Null);
        if st.get("state").and_then(Value::as_str).unwrap_or("no_wallet") != "no_wallet" {
            return err("close the open wallet before switching networks");
        }
        let mut g = self.inner.lock().unwrap();
        if !g.sends.open_requests().is_empty() { return err("a send is in flight"); }
        g.active = network;
        Self::persist_settings(&g);
        ok(json!({ "active": g.active }))
    }

    fn list_wallets(&self) -> String {
        let on_disk = modules().monero_wallet_core_module.list_wallets().unwrap_or(json!([]));
        let mut g = self.inner.lock().unwrap();
        // A wallet on disk the registry has not seen is registered on the active network.
        if let Some(arr) = on_disk.as_array() {
            for n in arr.iter().filter_map(Value::as_str) {
                if g.registry.get(n).is_none() {
                    let m = WalletMeta { network: g.active.clone(), ..Default::default() };
                    let _ = g.registry.upsert(n, m);
                }
            }
        }
        ok(json!({ "wallets": g.registry.list_json() }))
    }

    fn open_wallet(&self, name: String, password: String) -> String {
        let network = {
            let g = self.inner.lock().unwrap();
            g.registry.get(&name).map(|m| m.network.clone()).unwrap_or_else(|| g.active.clone())
        };
        self.start_core_job("open_wallet", json!({ "name": name, "password": password, "network": network }), &name, None)
    }

    fn create_wallet(&self, name: String, password: String, label: String) -> String {
        let network = self.inner.lock().unwrap().active.clone();
        let meta = WalletMeta { network: network.clone(), label, ..Default::default() };
        self.start_core_job("create_wallet", json!({ "name": name, "password": password, "network": network }), &name, Some(meta))
    }

    fn restore_from_seed(&self, params_json: String) -> String {
        let p: Value = match serde_json::from_str(&params_json) { Ok(v) => v, Err(e) => return err(e) };
        let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if name.is_empty() { return err("name is required"); }
        let network = self.inner.lock().unwrap().active.clone();
        let rh = p.get("restoreHeight").and_then(Value::as_u64).unwrap_or(0);
        let meta = WalletMeta { network: network.clone(), label: p.get("label").and_then(Value::as_str).unwrap_or("").into(), restore_height: rh, ..Default::default() };
        let params = json!({ "name": name, "password": p.get("password").cloned().unwrap_or(json!("")), "network": network,
                             "seed": p.get("seed").cloned().unwrap_or(json!("")), "restoreHeight": rh,
                             "seedOffset": p.get("seedOffset").cloned().unwrap_or(json!("")) });
        self.start_core_job("restore_from_seed", params, &name, Some(meta))
    }

    fn restore_from_keys(&self, params_json: String) -> String {
        let p: Value = match serde_json::from_str(&params_json) { Ok(v) => v, Err(e) => return err(e) };
        let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if name.is_empty() { return err("name is required"); }
        let network = self.inner.lock().unwrap().active.clone();
        let rh = p.get("restoreHeight").and_then(Value::as_u64).unwrap_or(0);
        let view_only = p.get("spendKey").and_then(Value::as_str).map_or(true, str::is_empty);
        let meta = WalletMeta { network: network.clone(), label: p.get("label").and_then(Value::as_str).unwrap_or("").into(), restore_height: rh, view_only, ..Default::default() };
        let params = json!({ "name": name, "password": p.get("password").cloned().unwrap_or(json!("")), "network": network,
                             "address": p.get("address").cloned().unwrap_or(json!("")), "viewKey": p.get("viewKey").cloned().unwrap_or(json!("")),
                             "spendKey": p.get("spendKey").cloned().unwrap_or(json!("")), "restoreHeight": rh });
        self.start_core_job("restore_from_keys", params, &name, Some(meta))
    }

    fn close_wallet(&self) -> String { self.start_core_job("close_wallet", json!({}), "", None) }

    fn change_password(&self, old_password: String, new_password: String) -> String {
        self.start_core_job("change_password", json!({ "oldPassword": old_password, "newPassword": new_password }), "", None)
    }

    fn reveal_seed(&self, password: String) -> String {
        match core_json(modules().monero_wallet_core_module.reveal_seed(&password)).and_then(unwrap_result) {
            Ok(v) => ok(v), Err(e) => err(e),
        }
    }

    fn reveal_view_key(&self, password: String) -> String {
        match core_json(modules().monero_wallet_core_module.reveal_view_key(&password)).and_then(unwrap_result) {
            Ok(v) => ok(v), Err(e) => err(e),
        }
    }

    fn job_status(&self, job_id: String) -> String {
        let g = self.inner.lock().unwrap();
        match g.jobs.get(&job_id) {
            None => err("unknown jobId"),
            Some(j) => ok(json!({ "jobId": job_id, "kind": j.kind, "state": j.state,
                                  "result": if j.state == "done" { j.result.clone() } else { Value::Null },
                                  "error": if j.state == "failed" { Some(j.error.clone()) } else { None } })),
        }
    }

    fn wallet_status(&self) -> String {
        let mut st = modules().monero_wallet_core_module.status().unwrap_or(json!({ "state": "unavailable" }));
        let g = self.inner.lock().unwrap();
        let wh = st.get("walletHeight").and_then(Value::as_u64).unwrap_or(0);
        let dh = st.get("daemonHeight").and_then(Value::as_u64).unwrap_or(0);
        st["syncPercent"] = json!(if dh == 0 { 0 } else { ((wh.min(dh) as u128 * 100) / dh as u128) as u64 });
        st["activeNetwork"] = json!(g.active);
        if let Some(n) = st.get("wallet").and_then(Value::as_str) {
            if let Some(m) = g.registry.get(n) { st["meta"] = serde_json::to_value(m).unwrap_or(Value::Null); }
        }
        ok(st)
    }

    fn balances(&self, account_index: i64) -> String {
        let core = modules().monero_wallet_core_module;
        let b = core.balance(account_index).unwrap_or_else(|_| "0".into());
        let u = core.unlocked_balance(account_index).unwrap_or_else(|_| "0".into());
        ok(json!({ "balance": b, "unlocked": u,
                   "balanceXmr": format_xmr(&b).unwrap_or_default(), "unlockedXmr": format_xmr(&u).unwrap_or_default() }))
    }

    fn receive_info(&self, account_index: i64) -> String {
        let core = modules().monero_wallet_core_module;
        let addr = core.address(account_index, 0).unwrap_or_default();
        let subs = core.subaddresses(account_index).unwrap_or(json!([]));
        ok(json!({ "address": addr, "subaddresses": subs }))
    }

    fn create_subaddress(&self, account_index: i64, label: String) -> String {
        match core_json(modules().monero_wallet_core_module.create_subaddress(account_index, &label)).and_then(unwrap_result) {
            Ok(v) => ok(v), Err(e) => err(e),
        }
    }

    fn history(&self) -> String {
        let rows = modules().monero_wallet_core_module.history().unwrap_or(json!([]));
        ok(json!({ "rows": normalize_history(&rows) }))
    }

    fn address_valid(&self, address: String) -> bool {
        let network = self.inner.lock().unwrap().active.clone();
        modules().monero_wallet_core_module.address_valid(&address, &network).unwrap_or(false)
    }

    fn node_health(&self) -> String {
        let network = self.inner.lock().unwrap().active.clone();
        modules().monero_node_module.node_health(&network).unwrap_or_else(|e| err(format!("node module: {e:?}")))
    }

    fn prepare_send(&self, send_json: String) -> String {
        let p: Value = match serde_json::from_str(&send_json) { Ok(v) => v, Err(e) => return err(e) };
        let amount = match (p.get("amountXmr").and_then(Value::as_str), p.get("amount").and_then(Value::as_str)) {
            (Some(x), _) => match parse_xmr(x) { Some(a) => a, None => return err("amountXmr is not a valid XMR amount (max 12 decimals)") },
            (None, Some(a)) => a.to_string(),
            _ => return err("amountXmr or amount is required"),
        };
        let address = p.get("address").and_then(Value::as_str).unwrap_or("").to_string();
        if address.is_empty() { return err("address is required"); }
        if !self.address_valid(address.clone()) { return err("invalid address for the active network"); }
        let rid = match self.inner.lock().unwrap().sends.begin(p.clone()) { Ok(r) => r, Err(e) => return err(e) };
        let params = json!({ "address": address, "amount": amount,
                             "priority": p.get("priority").cloned().unwrap_or(json!(0)),
                             "accountIndex": p.get("accountIndex").cloned().unwrap_or(json!(0)) });
        let r = self.start_core_job("create_transaction", params, &rid, None);
        let started: Value = serde_json::from_str(&r).unwrap_or(Value::Null);
        if !started.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let mut g = self.inner.lock().unwrap();
            if let Some(s) = g.sends.get_mut(&rid) { s.state = SendState::Failed(started.get("error").and_then(Value::as_str).unwrap_or("engine refused").into()); }
            return r;
        }
        emit_send_status_changed(&rid, "preparing");
        ok(json!({ "requestId": rid }))
    }

    fn send_status(&self, request_id: String) -> String {
        self.inner.lock().unwrap().sends.status_json(&request_id).to_string()
    }

    fn confirm_send(&self, request_id: String) -> String {
        let handle = {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.sends.get_mut(&request_id) else { return err("unknown requestId") };
            if s.state != SendState::Previewed { return err(format!("send is {}, not previewed", s.state.name())); }
            let Some(h) = s.tx_handle.clone() else { return err("no transaction handle") };
            s.state = SendState::Committing;
            h
        };
        emit_send_status_changed(&request_id, "committing");
        self.start_core_job("commit_transaction", json!({ "txHandle": handle }), &request_id, None)
    }

    fn cancel_send(&self, request_id: String) -> String {
        let handle = {
            let mut g = self.inner.lock().unwrap();
            let Some(s) = g.sends.get_mut(&request_id) else { return err("unknown requestId") };
            if s.state.terminal() { return err(format!("send already {}", s.state.name())); }
            if s.state == SendState::Committing { return err("cannot cancel a broadcast in progress"); }
            s.state = SendState::Cancelled;
            s.tx_handle.take()
        };
        emit_send_status_changed(&request_id, "cancelled");
        if let Some(h) = handle {
            let _ = modules().monero_wallet_core_module.start_job("dispose_transaction", &json!({ "txHandle": h }));
        }
        ok(json!({ "requestId": request_id }))
    }

    fn format_xmr(&self, atomic: String) -> String { format_xmr(&atomic).unwrap_or_default() }
    fn parse_xmr(&self, xmr: String) -> String { parse_xmr(&xmr).unwrap_or_default() }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<MoneroWalletBackendModuleImpl>();
}
