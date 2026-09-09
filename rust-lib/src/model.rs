//! The coordinator's pure core: amount formatting, the wallet registry, history
//! normalisation and the send state machine. No Logos deps; `cargo test --no-default-features`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const NETWORKS: &[&str] = &["mainnet", "stagenet", "testnet", "regtest"];
pub const ATOMIC_PER_XMR: u64 = 1_000_000_000_000;

pub fn is_network(n: &str) -> bool { NETWORKS.contains(&n) }

/// Atomic units (decimal string, u64) → "1.234567890000" XMR. Exact: no floating point.
pub fn format_xmr(atomic: &str) -> Option<String> {
    let v: u64 = atomic.parse().ok()?;
    let whole = v / ATOMIC_PER_XMR;
    let frac = v % ATOMIC_PER_XMR;
    Some(format!("{whole}.{frac:012}"))
}

/// "1.5" / "0.000000000001" / "12" XMR → atomic units as a decimal string. Refuses more than
/// 12 fractional digits rather than rounding: a wallet must not silently move value.
pub fn parse_xmr(xmr: &str) -> Option<String> {
    let s = xmr.trim();
    if s.is_empty() || s.starts_with('-') { return None; }
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    if frac.len() > 12 || !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let mut frac_s = frac.to_string();
    while frac_s.len() < 12 { frac_s.push('0'); }
    let frac: u64 = if frac_s.is_empty() { 0 } else { frac_s.parse().ok()? };
    let total = whole.checked_mul(ATOMIC_PER_XMR)?.checked_add(frac)?;
    Some(total.to_string())
}

/// What the registry remembers about a wallet. NEVER a password, NEVER a key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletMeta {
    pub network: String,
    #[serde(default)]
    pub label: String,
    #[serde(default, rename = "viewOnly")]
    pub view_only: bool,
    #[serde(default, rename = "restoreHeight")]
    pub restore_height: u64,
    #[serde(default)]
    pub address: String,
}

pub struct Registry {
    wallets: BTreeMap<String, WalletMeta>,
    path: Option<PathBuf>,
}

impl Registry {
    pub fn new(path: Option<PathBuf>) -> Self {
        let wallets = path.as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { wallets, path }
    }

    fn persist(&self) -> Result<(), String> {
        let Some(p) = self.path.as_ref() else { return Ok(()) };
        let body = serde_json::to_string_pretty(&self.wallets).map_err(|e| e.to_string())?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, p).map_err(|e| e.to_string())
    }

    pub fn get(&self, name: &str) -> Option<&WalletMeta> { self.wallets.get(name) }
    pub fn names(&self) -> Vec<String> { self.wallets.keys().cloned().collect() }

    pub fn upsert(&mut self, name: &str, meta: WalletMeta) -> Result<(), String> {
        self.wallets.insert(name.into(), meta);
        self.persist()
    }

    pub fn remove(&mut self, name: &str) -> Result<bool, String> {
        let r = self.wallets.remove(name).is_some();
        if r { self.persist()?; }
        Ok(r)
    }

    pub fn list_json(&self) -> Value {
        self.wallets.iter().map(|(n, m)| {
            let mut v = serde_json::to_value(m).unwrap_or(json!({}));
            v["name"] = json!(n);
            v
        }).collect()
    }
}

/// One row of the core's history, restated for a UI: amounts also as XMR strings.
pub fn normalize_history(rows: &Value) -> Value {
    let Some(arr) = rows.as_array() else { return json!([]) };
    let mut out: Vec<Value> = arr.iter().map(|r| {
        let amount = r.get("amount").and_then(Value::as_str).unwrap_or("0").to_string();
        let fee = r.get("fee").and_then(Value::as_str).unwrap_or("0").to_string();
        json!({
            "txid": r.get("txid").cloned().unwrap_or(json!("")),
            "direction": r.get("direction").cloned().unwrap_or(json!("in")),
            "amount": amount, "amountXmr": format_xmr(&amount).unwrap_or_default(),
            "fee": fee, "feeXmr": format_xmr(&fee).unwrap_or_default(),
            "height": r.get("height").cloned().unwrap_or(json!(0)),
            "confirmations": r.get("confirmations").cloned().unwrap_or(json!(0)),
            "timestamp": r.get("timestamp").cloned().unwrap_or(json!(0)),
            "pending": r.get("pending").cloned().unwrap_or(json!(false)),
            "failed": r.get("failed").cloned().unwrap_or(json!(false)),
            "unlockTime": r.get("unlockTime").cloned().unwrap_or(json!(0)),
            "account": r.get("account").cloned().unwrap_or(json!(0)),
            // Detail the row does not show but the expanded view does. `destinations` is empty
            // for an incoming transfer — wallet2 records them only for transfers we made — and
            // the view must say "not recorded" rather than implying the sender is unknown.
            "paymentId": r.get("paymentId").cloned().unwrap_or(json!("")),
            "description": r.get("description").cloned().unwrap_or(json!("")),
            "subaddrIndex": r.get("subaddrIndex").cloned().unwrap_or(json!("")),
            "coinbase": r.get("coinbase").cloned().unwrap_or(json!(false)),
            "destinations": r.get("destinations").cloned().unwrap_or(json!([])),
        })
    }).collect();
    // Newest first; a pending row (height 0) sorts to the top.
    out.sort_by(|a, b| {
        let ha = a["height"].as_u64().unwrap_or(0); let hb = b["height"].as_u64().unwrap_or(0);
        let ka = if ha == 0 { u64::MAX } else { ha }; let kb = if hb == 0 { u64::MAX } else { hb };
        kb.cmp(&ka)
    });
    Value::Array(out)
}

/// The send state machine. At most ONE prepared-but-uncommitted transaction per wallet:
/// a `PendingTransaction` reserves nothing, so a second build can spend the same outputs
/// and the second commit then fails. A prepared tx also expires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendState {
    Preparing,
    Previewed,
    Committing,
    Sent,
    Failed(String),
    Cancelled,
}

impl SendState {
    pub fn name(&self) -> &'static str {
        match self {
            SendState::Preparing => "preparing",
            SendState::Previewed => "previewed",
            SendState::Committing => "committing",
            SendState::Sent => "sent",
            SendState::Failed(_) => "failed",
            SendState::Cancelled => "cancelled",
        }
    }
    pub fn terminal(&self) -> bool {
        matches!(self, SendState::Sent | SendState::Failed(_) | SendState::Cancelled)
    }
}

#[derive(Clone, Debug)]
pub struct SendRequest {
    pub request_id: String,
    pub state: SendState,
    pub request: Value,
    pub preview: Option<Value>,
    pub tx_handle: Option<String>,
    pub txids: Option<String>,
    pub core_job: Option<(String, String)>,
    pub prepared_at: Option<Instant>,
}

pub const PREVIEW_TTL: Duration = Duration::from_secs(120);

pub struct Sends {
    reqs: BTreeMap<String, SendRequest>,
    next: u64,
}

impl Default for Sends { fn default() -> Self { Self { reqs: BTreeMap::new(), next: 1 } } }

impl Sends {
    /// Refuses while another request is not terminal.
    pub fn begin(&mut self, request: Value) -> Result<String, String> {
        if let Some(open) = self.reqs.values().find(|r| !r.state.terminal()) {
            return Err(format!("a send is already in flight ({}: {})", open.request_id, open.state.name()));
        }
        let id = format!("s{}", self.next); self.next += 1;
        self.reqs.insert(id.clone(), SendRequest {
            request_id: id.clone(), state: SendState::Preparing, request, preview: None,
            tx_handle: None, txids: None, core_job: None, prepared_at: None,
        });
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Option<&SendRequest> { self.reqs.get(id) }
    pub fn get_mut(&mut self, id: &str) -> Option<&mut SendRequest> { self.reqs.get_mut(id) }

    pub fn open_requests(&mut self) -> Vec<String> {
        self.reqs.values().filter(|r| !r.state.terminal()).map(|r| r.request_id.clone()).collect()
    }

    /// A preview older than PREVIEW_TTL is cancelled rather than committed with stale decoys.
    pub fn expire(&mut self, now: Instant) -> Vec<String> {
        let mut expired = Vec::new();
        for r in self.reqs.values_mut() {
            if r.state == SendState::Previewed {
                if let Some(t) = r.prepared_at {
                    if now.duration_since(t) > PREVIEW_TTL { r.state = SendState::Failed("preview expired".into()); expired.push(r.request_id.clone()); }
                }
            }
        }
        expired
    }

    pub fn all_json(&self) -> Value {
        self.reqs.values().map(|r| json!({ "requestId": r.request_id, "state": r.state.name() })).collect()
    }

    pub fn status_json(&self, id: &str) -> Value {
        match self.reqs.get(id) {
            None => json!({ "ok": false, "error": "unknown requestId" }),
            Some(r) => json!({
                "ok": true, "requestId": r.request_id, "state": r.state.name(),
                "preview": r.preview, "txids": r.txids,
                "error": match &r.state { SendState::Failed(e) => Some(e.clone()), _ => None },
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_exact_to_twelve_places() {
        assert_eq!(format_xmr("1000000000000").unwrap(), "1.000000000000");
        assert_eq!(format_xmr("1").unwrap(), "0.000000000001");
        assert_eq!(format_xmr("1234567890123").unwrap(), "1.234567890123");
        assert_eq!(format_xmr("18446744073709551615").unwrap(), "18446744.073709551615");
        assert!(format_xmr("x").is_none());
    }

    #[test]
    fn parse_round_trips_and_refuses_precision_loss() {
        assert_eq!(parse_xmr("1.5").unwrap(), "1500000000000");
        assert_eq!(parse_xmr("0.000000000001").unwrap(), "1");
        assert_eq!(parse_xmr("12").unwrap(), "12000000000000");
        assert_eq!(parse_xmr(".5").unwrap(), "500000000000");
        assert!(parse_xmr("0.0000000000001").is_none(), "13 fractional digits must be refused, not rounded");
        assert!(parse_xmr("-1").is_none());
        assert!(parse_xmr("1e3").is_none());
        for a in ["0", "1", "999999999999", "1000000000000", "123456789012345"] {
            assert_eq!(parse_xmr(&format_xmr(a).unwrap()).unwrap(), a);
        }
    }

    #[test]
    fn registry_round_trips_and_never_holds_a_secret() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wallets.json");
        {
            let mut r = Registry::new(Some(p.clone()));
            r.upsert("main", WalletMeta { network: "stagenet".into(), label: "Main".into(), ..Default::default() }).unwrap();
        }
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("password") && !text.contains("seed"));
        let r = Registry::new(Some(p));
        assert_eq!(r.get("main").unwrap().network, "stagenet");
    }

    #[test]
    fn history_carries_every_detail_the_expanded_row_shows() {
        // The engine reports these; normalize_history used to drop them on the floor, so the
        // Activity detail had nothing to render. An incoming transfer has NO destinations —
        // wallet2 records them only for transfers this wallet made — and that must survive as
        // an empty list rather than becoming null.
        let rows = json!([{
            "txid": "a1", "direction": "in", "amount": "100000000000", "fee": "0",
            "height": 10, "confirmations": 2, "timestamp": 1788900000,
            "paymentId": "pid7", "description": "rent", "subaddrIndex": "1",
            "coinbase": false, "destinations": []
        }, {
            "txid": "b2", "direction": "out", "amount": "50000000000", "fee": "30000000",
            "height": 11, "confirmations": 1, "timestamp": 1788900100,
            "destinations": [{ "address": "58hpB", "amount": "50000000000" }]
        }]);
        let out = normalize_history(&rows);
        let a = out.as_array().unwrap();
        let incoming = a.iter().find(|r| r["txid"] == "a1").unwrap();
        assert_eq!(incoming["paymentId"], "pid7");
        assert_eq!(incoming["description"], "rent");
        assert_eq!(incoming["subaddrIndex"], "1");
        assert_eq!(incoming["amountXmr"], "0.100000000000");
        assert!(incoming["destinations"].as_array().unwrap().is_empty(), "an incoming transfer records no destination");
        let outgoing = a.iter().find(|r| r["txid"] == "b2").unwrap();
        assert_eq!(outgoing["destinations"][0]["address"], "58hpB");
        assert_eq!(outgoing["feeXmr"], "0.000030000000", "30000000 atomic units is 0.00003 XMR — 1 XMR is 1e12");
        // Absent fields become empty, never null: the view renders "—" from an empty string.
        assert_eq!(outgoing["paymentId"], "");
        assert_eq!(outgoing["description"], "");
        assert_eq!(outgoing["coinbase"], false);
    }

    #[test]
    fn history_sorts_newest_first_with_pending_on_top() {
        let rows = json!([
            {"txid":"a","height":100,"amount":"1","fee":"0"},
            {"txid":"b","height":0,"amount":"2","fee":"0","pending":true},
            {"txid":"c","height":200,"amount":"3","fee":"0"}
        ]);
        let out = normalize_history(&rows);
        let ids: Vec<&str> = out.as_array().unwrap().iter().map(|r| r["txid"].as_str().unwrap()).collect();
        assert_eq!(ids, ["b", "c", "a"]);
        assert_eq!(out[0]["amountXmr"], "0.000000000002");
    }

    #[test]
    fn only_one_send_in_flight_at_a_time() {
        let mut s = Sends::default();
        let a = s.begin(json!({})).unwrap();
        assert!(s.begin(json!({})).is_err(), "second prepare must be refused while the first is open");
        s.get_mut(&a).unwrap().state = SendState::Cancelled;
        assert!(s.begin(json!({})).is_ok());
    }

    #[test]
    fn a_stale_preview_expires_instead_of_committing() {
        let mut s = Sends::default();
        let a = s.begin(json!({})).unwrap();
        let r = s.get_mut(&a).unwrap();
        r.state = SendState::Previewed;
        r.prepared_at = Some(Instant::now() - PREVIEW_TTL - Duration::from_secs(1));
        let expired = s.expire(Instant::now());
        assert_eq!(expired, vec![a.clone()]);
        assert_eq!(s.status_json(&a)["state"], "failed");
    }
}
