//! Who may do what, as a pure function of the caller. Mirrors keystore_module's gate: a role is
//! a SET of module names, an empty set admits nobody, `HostAnchor` is always refused (it is one
//! undifferentiated bag covering the shells and every relayed CLI token), and a method the
//! registry does not name is refused outright — a typo fails closed, never falls through ungated.

use serde::Deserialize;

/// Default custodian — the surface that takes the wallet password and shows a seed.
///
/// The same surface as the approver, and deliberately so: Monero's password is a
/// once-per-session UNLOCK for one wallet file, not a per-signature credential, so there is no
/// second GUI for it to be split away from. The roles stay separate VALUES because they are
/// sets: an operator enrolling a headless module can grant one without the other, which is the
/// whole point of `configure`.
pub const DEFAULT_CUSTODIAN: &str = "monero_wallet_ui";
/// Default approver — the surface that reviews a built transaction and broadcasts it.
pub const DEFAULT_APPROVER: &str = "monero_wallet_ui";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    Unknown,
    HostAnchor,
    Module(String),
    Derived { parent: String, leaf: String },
    Operator(String),
}

impl Caller {
    pub fn is_module(&self, name: &str) -> bool {
        matches!(self, Caller::Module(n) if n == name)
    }
    /// Only a plainly-named module has a name a request can be recorded against.
    pub fn named(&self) -> Option<&str> {
        match self { Caller::Module(n) => Some(n.as_str()), _ => None }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roles {
    pub approvers: Vec<String>,
    pub custodians: Vec<String>,
}

impl Default for Roles {
    fn default() -> Self {
        Self { approvers: vec![DEFAULT_APPROVER.into()], custodians: vec![DEFAULT_CUSTODIAN.into()] }
    }
}

/// One role's holders, as a bare name or a list — `"custodians": "monero_wallet_ui"` is the common case.
#[derive(Deserialize)]
#[serde(untagged)]
enum RoleWire { One(String), Many(Vec<String>) }

impl Default for RoleWire { fn default() -> Self { RoleWire::Many(Vec::new()) } }

impl RoleWire {
    fn into_holders(self) -> Vec<String> {
        let raw = match self { RoleWire::One(s) => vec![s], RoleWire::Many(v) => v };
        let mut out: Vec<String> = Vec::with_capacity(raw.len());
        for n in raw { let n = n.trim().to_string(); if !n.is_empty() && !out.contains(&n) { out.push(n); } }
        out
    }
}

/// `{ approvers?, custodians? }`. Unknown keys are REFUSED: under the total rule a typo'd key
/// would otherwise empty both roles — fail-closed, but silent about why.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RolesWire {
    #[serde(default)]
    approvers: RoleWire,
    #[serde(default)]
    custodians: RoleWire,
}

impl Roles {
    /// Replace both roles from a document. TOTAL, not a patch: a role the document does not
    /// name is held by nobody. A malformed document is refused and the roles in force stay.
    pub fn configure(&mut self, config_json: &str) -> Result<(), String> {
        let v: serde_json::Value = serde_json::from_str(config_json).map_err(|e| format!("bad roles document: {e}"))?;
        if !v.is_object() { return Err("roles document must be an object".into()); }
        let wire: RolesWire = serde_json::from_value(v).map_err(|e| format!("bad roles document: {e}"))?;
        self.approvers = wire.approvers.into_holders();
        self.custodians = wire.custodians.into_holders();
        Ok(())
    }
}

pub fn holds_role(role_holder: &str, caller: &Caller) -> bool {
    !role_holder.is_empty() && caller.is_module(role_holder)
}

/// An EMPTY SET admits nobody — `any` over nothing is false, which is the fail-closed direction.
pub fn holds_any_role(role_holders: &[String], caller: &Caller) -> bool {
    role_holders.iter().any(|h| holds_role(h, caller))
}

/// Custodian-only: everything that takes or reveals the wallet password or a key, plus the
/// network switch — switching networks re-targets which wallet files are addressable at all, so
/// it belongs with opening rather than with spending. A list, so the set is one assertable value.
pub const CUSTODIAN_METHODS: &[&str] = &[
    "open_wallet", "create_wallet", "restore_from_seed", "restore_from_keys",
    "change_password", "reveal_seed", "reveal_view_key", "set_active_network",
    // Which daemon the wallet talks to is a privacy and trust decision, and it is device-wide:
    // it belongs with opening a wallet, not with spending from one.
    "set_node_config",
];

/// Approver-only: the one decision that moves money. Governs BROADCAST — the engine signed at build.
pub const APPROVER_METHODS: &[&str] = &["confirm_send"];

/// Either role: ending the session is not a secret, but it is not for an arbitrary module either.
pub const SESSION_METHODS: &[&str] = &["close_wallet"];

pub fn custodian_admits(method: &str, custodians: &[String], caller: &Caller) -> bool {
    CUSTODIAN_METHODS.contains(&method) && holds_any_role(custodians, caller)
}

pub fn approver_admits(method: &str, approvers: &[String], caller: &Caller) -> bool {
    APPROVER_METHODS.contains(&method) && holds_any_role(approvers, caller)
}

pub fn session_admits(method: &str, roles: &Roles, caller: &Caller) -> bool {
    SESSION_METHODS.contains(&method)
        && (holds_any_role(&roles.custodians, caller) || holds_any_role(&roles.approvers, caller))
}

/// Requesting a send (build for review) or withdrawing one: any NAMED module. Never the host anchor.
pub fn requester_admits(caller: &Caller) -> bool {
    caller.named().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(n: &str) -> Caller { Caller::Module(n.into()) }
    fn holders(names: &[&str]) -> Vec<String> { names.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn defaults_admit_the_gui_and_nobody_else() {
        let r = Roles::default();
        assert!(custodian_admits("open_wallet", &r.custodians, &m("monero_wallet_ui")));
        assert!(approver_admits("confirm_send", &r.approvers, &m("monero_wallet_ui")));
        // One GUI holds both by default; every other module holds neither until `configure`
        // says so — including the headless relay, which an operator must enrol on purpose.
        assert!(!custodian_admits("open_wallet", &r.custodians, &m("monero_wallet_cli")));
        assert!(!approver_admits("confirm_send", &r.approvers, &m("monero_wallet_cli")));
        assert!(!custodian_admits("open_wallet", &r.custodians, &m("some_other_module")));
    }

    #[test]
    fn the_host_anchor_is_refused_everywhere_however_roles_are_set() {
        let mut r = Roles::default();
        for method in CUSTODIAN_METHODS { assert!(!custodian_admits(method, &r.custodians, &Caller::HostAnchor)); }
        assert!(!approver_admits("confirm_send", &r.approvers, &Caller::HostAnchor));
        assert!(!session_admits("close_wallet", &r, &Caller::HostAnchor));
        assert!(!requester_admits(&Caller::HostAnchor));
        assert!(!requester_admits(&Caller::Unknown));
        r.configure(r#"{"custodians":["core","capability_module",""]}"#).unwrap();
        assert!(!custodian_admits("open_wallet", &r.custodians, &Caller::HostAnchor), "naming the anchor's aliases grants nothing: it carries no name");
    }

    #[test]
    fn a_method_the_registry_does_not_name_is_refused_even_for_the_holder() {
        let r = Roles::default();
        assert!(!custodian_admits("balances", &r.custodians, &m("monero_wallet_ui")));
        assert!(!custodian_admits("open_walet", &r.custodians, &m("monero_wallet_ui")), "a typo fails closed");
        assert!(!approver_admits("prepare_send", &r.approvers, &m("monero_wallet_ui")), "prepare is a request, not an approval");
    }

    #[test]
    fn roles_are_sets_and_independent() {
        let mut r = Roles::default();
        // The headless relay may hold one role without the other: an operator box that unlocks
        // at boot but must never broadcast is exactly `custodians` without `approvers`.
        r.configure(r#"{"approvers":["monero_wallet_ui"],"custodians":["monero_wallet_ui","monero_wallet_cli"]}"#).unwrap();
        assert!(custodian_admits("open_wallet", &r.custodians, &m("monero_wallet_cli")));
        assert!(!approver_admits("confirm_send", &r.approvers, &m("monero_wallet_cli")), "a custodian is not thereby an approver");
        assert!(approver_admits("confirm_send", &r.approvers, &m("monero_wallet_ui")));
        r.configure(r#"{"approvers":"monero_wallet_cli"}"#).unwrap();
        assert!(approver_admits("confirm_send", &r.approvers, &m("monero_wallet_cli")));
        assert!(!approver_admits("confirm_send", &r.approvers, &m("monero_wallet_ui")), "configure is total: the GUI was not restated");
        assert!(r.custodians.is_empty(), "a role the document does not name is held by nobody");
    }

    #[test]
    fn configure_is_total_and_refuses_unknown_keys() {
        let mut r = Roles::default();
        assert!(r.configure(r#"{"custodian":"x"}"#).is_err(), "the singular spelling is refused, not silently ignored");
        assert_eq!(r, Roles::default(), "a refused document leaves the roles in force");
        assert!(r.configure(r#"[]"#).is_err());
        r.configure(r#"{}"#).unwrap();
        assert!(r.approvers.is_empty() && r.custodians.is_empty(), "an empty document is 'nobody', by design");
        assert!(!custodian_admits("open_wallet", &r.custodians, &m("monero_wallet_ui")));
    }

    #[test]
    fn either_role_may_close_the_session() {
        let mut r = Roles::default();
        assert!(session_admits("close_wallet", &r, &m("monero_wallet_ui")));
        // Both defaults name one GUI, so "either role" is only expressible via configure — cover
        // BOTH branches explicitly or the || in session_admits goes untested.
        r.configure(r#"{"custodians":"monero_wallet_cli"}"#).unwrap();
        assert!(session_admits("close_wallet", &r, &m("monero_wallet_cli")), "a custodian alone may end the session");
        r.configure(r#"{"approvers":"monero_wallet_cli"}"#).unwrap();
        assert!(session_admits("close_wallet", &r, &m("monero_wallet_cli")), "an approver alone may end the session too");
        r.configure(r#"{}"#).unwrap();
        assert!(!session_admits("close_wallet", &r, &m("monero_wallet_cli")), "holding neither role ends nothing");
        assert!(!session_admits("close_wallet", &r, &m("some_other_module")));
        assert!(requester_admits(&m("some_other_module")), "but any named module may ask for a send to be built");
        assert!(!requester_admits(&Caller::Derived { parent: "a".into(), leaf: "b".into() }));
    }

    #[test]
    fn the_registries_do_not_overlap() {
        for c in CUSTODIAN_METHODS { assert!(!APPROVER_METHODS.contains(c) && !SESSION_METHODS.contains(c)); }
        let _ = holders(&["x"]);
    }
}
