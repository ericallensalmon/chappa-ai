//! The relay and Windows Firewall.
//!
//! The relay listens on an engine's gateway address, so on Windows its
//! listeners need an inbound rule. Without one, the first listener raises
//! the system's allow-access prompt, and a Cancel there leaves a Block rule
//! for the program that silently drops every container connection from then
//! on (block rules win over allow rules). This module:
//!
//! - **checks, read-only**, whenever the relay's gateways or ports change,
//!   whenever it binds a listener, and at most every [`RECHECK`] while the
//!   view is read: is there an inbound TCP allow rule named [`RULE_NAME`]
//!   for this program, its ports and [`CONTAINER_RANGE`], on every profile;
//!   is there an enabled inbound Block rule that drops the relay's traffic
//!   (one for this program, or one for any program on the relay's ports,
//!   either on an interface type the container adapter has); is the gateway
//!   outside that range; does any interface have forwarding or weak-host
//!   receive on (then binding the gateway address alone no longer keeps
//!   other networks away; the relay's peer check still closes their
//!   connections). It runs one PowerShell command (argv, `-NoProfile`, a
//!   constant script, the program path passed in the environment, never
//!   interpolated), never elevated, with a short time limit, off the
//!   request path. A Block rule that drops the traffic is a `bad` finding;
//!   the rest are warnings. A check that cannot run says so in one banner
//!   and never holds the relay back;
//! - **offers the one-step allow**: a confirm-gated action whose confirm
//!   shows the exact elevated command, run through
//!   `Start-Process powershell.exe -Verb RunAs` (one elevation prompt). It
//!   replaces our older allow rule and removes the Block rules for this
//!   program. It never touches another program's rules: a Block rule for
//!   any program on the relay's ports is reported (`bad`) with the exact
//!   line to remove it, for the user to run or to narrow the rule. The
//!   rule covers the stack's whole port range (every slot), so bringing up
//!   another slot needs no second prompt, plus any explicit ports the rule
//!   it replaces had (another stack running through the same program keeps
//!   its ports). Every value put into the command is validated first: the
//!   program path is a plain absolute path quoted for PowerShell, the ports
//!   are numbers;
//! - **offers the same allow with blocks on other networks**: in the same
//!   single elevation it also adds inbound Block rules for the program on
//!   the [`HARDEN_TYPES`] interface types. Types, not adapter names, so
//!   nothing needs upkeep as adapters come and go. `Wired` is never among
//!   them: the container network's virtual adapter is an Ethernet adapter
//!   to the firewall, and a Block rule on it would drop the relay's own
//!   traffic;
//! - **offers removing the rules** the same way, from the relay's facts.
//!
//! Every rule this module creates is in the firewall group [`GROUP`], and
//! that is how ours are told from anyone else's: Remove and the
//! allow-and-block variant delete only rules of that group, and a user's
//! rule that merely shares the name is never touched. An allow rule made
//! before rules had a group is still recognised by its exact name and the
//! relay's program, so it is cleaned up too.
//!
//! Nothing elevates without that confirm.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use stack_core::cli::{CliError, EngineCli, Invocation};
use stack_core::hosts::HostOs;

use crate::relay::Network;

/// The display name of the allow rule, so it is recognisable in the
/// firewall's own console.
pub const RULE_NAME: &str = "Chappa'Ai relay";
/// The firewall group of every rule this module creates: how ours are
/// found and removed.
pub const GROUP: &str = "Chappa'Ai relay";
/// What the rule lets in: the private range container NAT networks are
/// handed out from (the gateway's own subnet moves within it).
pub const CONTAINER_RANGE: &str = "172.16.0.0/12";
/// The interface types the optional block rules cover: wireless adapters,
/// and the system's own VPN and dial-up connections. Never `Wired`.
pub const HARDEN_TYPES: [&str; 2] = ["Wireless", "RemoteAccess"];
pub const ALLOW_ACTION_ID: &str = "relay-firewall-allow";
pub const HARDEN_ACTION_ID: &str = "relay-firewall-allow-harden";
pub const REMOVE_ACTION_ID: &str = "relay-firewall-remove";
pub const RULE_BANNER: &str = "relay-firewall";
pub const RANGE_BANNER: &str = "relay-firewall-range";
pub const FORWARDING_BANNER: &str = "relay-firewall-forwarding";
pub const CHECK_BANNER: &str = "relay-firewall-check";
/// The read-only check's time limit.
pub const CHECK_TIMEOUT_SECS: u64 = 30;
/// How long the elevated command may wait for the user and run.
pub const ELEVATED_TIMEOUT_SECS: u64 = 600;
/// The least time between two checks started by reading the view.
pub const RECHECK: Duration = Duration::from_secs(60);
const PROGRAM_ENV: &str = "CHAPPA_RELAY_PROGRAM";
const RULE_ENV: &str = "CHAPPA_RELAY_RULE";
const RULE_DESCRIPTION: &str = "Lets containers reach the stack relay on its gateway address";
const BLOCK_DESCRIPTION: &str =
    "Keeps the stack relay off networks other than the container network";

/// The display name of the block rule on interface type `t`.
pub fn block_rule_name(t: &str) -> String {
    format!("{RULE_NAME} (block on {t})")
}

/// The read-only check: the rules of [`GROUP`], the rules named
/// [`RULE_NAME`] (the same text), the rules tied to the program, and every enabled inbound
/// Block rule, each with its filters, and every interface's forwarding and
/// weak-host receive, as one JSON document. Its inputs come from the
/// environment, so nothing is ever spliced into it.
pub const CHECK_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
$program = $env:CHAPPA_RELAY_PROGRAM
$ruleName = $env:CHAPPA_RELAY_RULE
function Describe($r) {
  $app = $r | Get-NetFirewallApplicationFilter
  $port = $r | Get-NetFirewallPortFilter
  $addr = $r | Get-NetFirewallAddressFilter
  $type = $r | Get-NetFirewallInterfaceTypeFilter
  [pscustomobject]@{
    name = [string]$r.Name
    displayName = [string]$r.DisplayName
    enabled = [string]$r.Enabled
    direction = [string]$r.Direction
    action = [string]$r.Action
    profile = [string]$r.Profile
    program = [string]$app.Program
    protocol = [string]$port.Protocol
    localPort = @($port.LocalPort | ForEach-Object { [string]$_ })
    remoteAddress = @($addr.RemoteAddress | ForEach-Object { [string]$_ })
    interfaceType = [string]$type.InterfaceType
    group = [string]$r.Group
  }
}
$named = @(Get-NetFirewallRule -Group $ruleName -ErrorAction SilentlyContinue) + @(Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue)
$forProgram = @(Get-NetFirewallApplicationFilter -Program $program -ErrorAction SilentlyContinue | Get-NetFirewallRule)
$blocking = @(Get-NetFirewallRule -Direction Inbound -Action Block -Enabled True -ErrorAction SilentlyContinue)
$rules = @(@($named + $forProgram + $blocking) | Where-Object { $_ } | Sort-Object Name -Unique | ForEach-Object { Describe $_ })
$interfaces = @(Get-NetIPInterface | ForEach-Object {
  [pscustomobject]@{
    alias = [string]$_.InterfaceAlias
    family = [string]$_.AddressFamily
    forwarding = [string]$_.Forwarding
    weakHostReceive = [string]$_.WeakHostReceive
  }
})
[pscustomobject]@{ rules = $rules; interfaces = $interfaces } | ConvertTo-Json -Depth 4 -Compress
"#;

/// The check as an invocation (argv, no shell line).
pub fn check_invocation(program: &str) -> Invocation {
    let mut inv = Invocation::new(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", CHECK_SCRIPT],
    );
    inv.env.insert(PROGRAM_ENV.into(), program.into());
    inv.env.insert(RULE_ENV.into(), RULE_NAME.into());
    inv.timeout_secs = CHECK_TIMEOUT_SECS;
    inv
}

// ------------------------------------------------------------ parsing

/// One firewall rule as the check reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FwRule {
    pub name: String,
    pub display_name: String,
    pub enabled: bool,
    pub inbound: bool,
    /// `Allow`, `Block` (anything else is neither).
    pub action: String,
    /// `Any`, or a list such as `Domain, Private, Public`.
    pub profile: String,
    /// Empty or `Any` when the rule is not tied to a program.
    pub program: String,
    pub protocol: String,
    pub local_ports: Vec<String>,
    pub remote_addresses: Vec<String>,
    /// `Any`, or a list such as `Wireless, RemoteAccess`; empty when not
    /// reported (counted as `Any`).
    pub interface_type: String,
    /// The rule's firewall group; empty when it has none.
    pub group: String,
}

impl FwRule {
    pub fn is_allow(&self) -> bool {
        self.action.eq_ignore_ascii_case("allow")
    }

    pub fn is_block(&self) -> bool {
        self.action.eq_ignore_ascii_case("block")
    }

    fn any_program(&self) -> bool {
        self.program.trim().is_empty() || self.program.eq_ignore_ascii_case("any")
    }

    /// Whether the rule acts on the container network's adapter: on every
    /// interface type, or on `Wired` (the virtual Ethernet adapter).
    fn on_container_adapter(&self) -> bool {
        let t = self.interface_type.to_ascii_lowercase();
        t.is_empty() || t == "any" || t.split(',').any(|x| x.trim() == "wired")
    }

    fn tcp(&self) -> bool {
        let p = self.protocol.to_ascii_lowercase();
        p.is_empty() || p == "tcp" || p == "6" || p == "any"
    }

    /// `name (TCP)` for the banner.
    fn label(&self) -> String {
        let name = if self.display_name.is_empty() {
            &self.name
        } else {
            &self.display_name
        };
        if self.protocol.is_empty() {
            name.clone()
        } else {
            format!("{name} ({})", self.protocol)
        }
    }
}

/// One interface of one address family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpInterface {
    pub alias: String,
    pub family: String,
    pub forwarding: bool,
    pub weak_host_receive: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub rules: Vec<FwRule>,
    pub interfaces: Vec<IpInterface>,
}

/// A value PowerShell wrote as a string, a list of strings, or nothing.
fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) if !s.is_empty() => vec![s.clone()],
        Value::Array(a) => a.iter().flat_map(strings).collect(),
        Value::Number(n) => vec![n.to_string()],
        _ => Vec::new(),
    }
}

fn text(v: &Value, key: &str) -> String {
    strings(&v[key]).join(", ")
}

/// A list PowerShell may have written as one object when it held one.
fn items(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![v],
        _ => Vec::new(),
    }
}

fn is_on(v: &Value) -> bool {
    matches!(v.as_str(), Some(s) if s.eq_ignore_ascii_case("enabled") || s.eq_ignore_ascii_case("true"))
}

/// The check's JSON.
pub fn parse_snapshot(output: &str) -> Result<Snapshot, String> {
    let v: Value = serde_json::from_str(output.trim())
        .map_err(|e| format!("the check's output is not the expected JSON ({e})"))?;
    if !v.is_object() {
        return Err("the check's output is not the expected JSON object".to_string());
    }
    let rules = items(&v["rules"])
        .into_iter()
        .map(|r| FwRule {
            name: text(r, "name"),
            display_name: text(r, "displayName"),
            enabled: text(r, "enabled").eq_ignore_ascii_case("true"),
            inbound: text(r, "direction").eq_ignore_ascii_case("inbound"),
            action: text(r, "action"),
            profile: text(r, "profile"),
            program: text(r, "program"),
            protocol: text(r, "protocol"),
            local_ports: strings(&r["localPort"]),
            remote_addresses: strings(&r["remoteAddress"]),
            interface_type: text(r, "interfaceType"),
            group: text(r, "group"),
        })
        .collect();
    let interfaces = items(&v["interfaces"])
        .into_iter()
        .map(|i| IpInterface {
            alias: text(i, "alias"),
            family: text(i, "family"),
            forwarding: is_on(&i["forwarding"]),
            weak_host_receive: is_on(&i["weakHostReceive"]),
        })
        .collect();
    Ok(Snapshot { rules, interfaces })
}

// -------------------------------------------------------------- ports

/// An inclusive run of ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortRange {
    pub lo: u16,
    pub hi: u16,
}

impl PortRange {
    pub const ALL: PortRange = PortRange {
        lo: 1,
        hi: u16::MAX,
    };

    /// `Any`, `14000` or `14000-14399`; `None` for anything else (`RPC`,
    /// `IPHTTPS` and the other keywords cover no plain port).
    pub fn parse(text: &str) -> Option<PortRange> {
        let t = text.trim();
        if t.eq_ignore_ascii_case("any") {
            return Some(PortRange::ALL);
        }
        let (lo, hi) = match t.split_once('-') {
            Some((a, b)) => (a.trim().parse().ok()?, b.trim().parse().ok()?),
            None => {
                let p = t.parse().ok()?;
                (p, p)
            }
        };
        (lo >= 1 && lo <= hi).then_some(PortRange { lo, hi })
    }

    /// How many ports the run holds.
    pub fn count(&self) -> u32 {
        u32::from(self.hi) - u32::from(self.lo) + 1
    }

    fn overlaps(&self, other: &PortRange) -> bool {
        self.lo <= other.hi && other.lo <= self.hi
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.lo == self.hi {
            write!(f, "{}", self.lo)
        } else {
            write!(f, "{}-{}", self.lo, self.hi)
        }
    }
}

/// Sorted, with overlapping and adjacent runs merged.
pub fn normalize(ranges: impl IntoIterator<Item = PortRange>) -> Vec<PortRange> {
    let mut all: Vec<PortRange> = ranges.into_iter().collect();
    all.sort();
    let mut out: Vec<PortRange> = Vec::new();
    for r in all {
        match out.last_mut() {
            Some(last) if u32::from(r.lo) <= u32::from(last.hi) + 1 => {
                last.hi = last.hi.max(r.hi);
            }
            _ => out.push(r),
        }
    }
    out
}

/// Whether every port of `need` is in `have`.
pub fn covers(have: &[PortRange], need: &[PortRange]) -> bool {
    let have = normalize(have.iter().copied());
    need.iter()
        .all(|n| have.iter().any(|h| h.lo <= n.lo && n.hi <= h.hi))
}

/// `14000-14399, 15000`.
pub fn ranges_text(ranges: &[PortRange]) -> String {
    if ranges.is_empty() {
        return "none".to_string();
    }
    ranges
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The ports the rule should cover: the stack's whole range, every slot
/// (`portBase` to the last port of the last slot), plus whatever is relayed
/// now outside it.
pub fn stack_ports(base: u16, stride: u16, slots: u32, relayed: &BTreeSet<u16>) -> Vec<PortRange> {
    let mut out = Vec::new();
    let span = u64::from(stride) * u64::from(slots.max(1));
    if base >= 1 && span > 0 {
        let hi = (u64::from(base) + span - 1).min(u64::from(u16::MAX)) as u16;
        out.push(PortRange { lo: base, hi });
    }
    out.extend(
        relayed
            .iter()
            .filter(|p| **p >= 1)
            .map(|p| PortRange { lo: *p, hi: *p }),
    );
    normalize(out)
}

// ---------------------------------------------------------- evaluation

/// What the rule must say, from the relay as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expect {
    /// The absolute path of the program that listens (this one).
    pub program: String,
    pub ports: Vec<PortRange>,
    pub gateways: Vec<IpAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowState {
    Present,
    Missing,
    /// A rule of that name exists but does not let the relay's traffic in;
    /// the reasons, for the banner.
    Mismatch(Vec<String>),
}

/// A Block rule that is not the relay's own to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignBlock {
    pub label: String,
    /// `Remove-NetFirewallRule -Name '<name>'`, when the name can be quoted
    /// safely; for the user to copy and run (elevated) themselves.
    pub remove_line: Option<String>,
}

/// What the check found, judged against [`Expect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub allow: AllowState,
    /// Our allow rule exists (in [`GROUP`], or made before rules had one).
    pub named_rule: bool,
    /// The explicit port runs the named rules have now, kept when the rule
    /// is replaced.
    pub named_ports: Vec<PortRange>,
    /// The Block rules for the relay's program that drop its traffic,
    /// labelled: the allow removes them.
    pub blocks: Vec<String>,
    /// Block rules for any program that drop the relay's traffic: never
    /// changed from here; reported with the line that removes each.
    pub foreign_blocks: Vec<ForeignBlock>,
    /// The interface types this program is already blocked on by the
    /// optional rules.
    pub hardened: Vec<String>,
    /// Gateways the rule's range does not cover.
    pub outside_range: Vec<IpAddr>,
    /// Interfaces with forwarding or weak-host receive on, described.
    pub open_interfaces: Vec<String>,
}

impl Report {
    /// Nothing to tell the user.
    pub fn is_clean(&self) -> bool {
        self.allow == AllowState::Present
            && !self.is_dropping()
            && self.outside_range.is_empty()
            && self.open_interfaces.is_empty()
    }

    /// The allow action is worth offering.
    pub fn wants_allow(&self) -> bool {
        self.allow != AllowState::Present || !self.blocks.is_empty()
    }

    /// Some Block rule drops the relay's traffic.
    pub fn is_dropping(&self) -> bool {
        !self.blocks.is_empty() || !self.foreign_blocks.is_empty()
    }

    /// Every Block rule that drops the relay's traffic, labelled.
    pub fn dropping_labels(&self) -> Vec<String> {
        self.blocks
            .iter()
            .cloned()
            .chain(self.foreign_blocks.iter().map(|f| f.label.clone()))
            .collect()
    }

    /// Every optional block rule is in place.
    pub fn is_hardened(&self) -> bool {
        HARDEN_TYPES
            .iter()
            .all(|t| self.hardened.iter().any(|h| h.eq_ignore_ascii_case(t)))
    }

    /// There is a rule of ours to remove.
    pub fn has_own_rules(&self) -> bool {
        self.named_rule || !self.hardened.is_empty()
    }
}

/// Windows paths compare without case and with either separator.
pub fn same_program(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim().replace('/', "\\").to_lowercase();
    !a.trim().is_empty() && norm(a) == norm(b)
}

fn container_range() -> Network {
    Network::parse(CONTAINER_RANGE).expect("the container range is a valid prefix")
}

/// The inclusive IPv4 span of `n` (`None` for IPv6).
fn v4_span(n: &Network) -> Option<(u32, u32)> {
    match n.addr() {
        IpAddr::V4(a) => {
            let first = u32::from(a);
            let last = first | u32::MAX.checked_shr(u32::from(n.prefix_len())).unwrap_or(0);
            Some((first, last))
        }
        IpAddr::V6(_) => None,
    }
}

/// A remote-address entry as an IPv4 span: a prefix or an `a-b` range;
/// `None` for keywords and IPv6.
fn entry_span(entry: &str) -> Option<(u32, u32)> {
    if let Some(n) = Network::parse(entry) {
        return v4_span(&n);
    }
    let (a, b) = entry.split_once('-')?;
    match (a.trim().parse(), b.trim().parse()) {
        (Ok(IpAddr::V4(a)), Ok(IpAddr::V4(b))) => Some((u32::from(a), u32::from(b))),
        _ => None,
    }
}

/// Whether a rule's remote-address entry includes the whole container
/// range: `Any`, a prefix holding it, or an address range spanning it.
fn remote_covers(entry: &str, range: &Network) -> bool {
    let e = entry.trim();
    if e.eq_ignore_ascii_case("any") {
        return true;
    }
    match (entry_span(e), v4_span(range)) {
        (Some((a, b)), Some((first, last))) => a <= first && last <= b,
        _ => false,
    }
}

/// Whether a rule's remote-address entry can match a container peer:
/// `Any`, `LocalSubnet` (the container network is the adapter's own
/// subnet), or a prefix or range that meets the container range.
fn remote_meets(entry: &str, range: &Network) -> bool {
    let e = entry.trim();
    if e.eq_ignore_ascii_case("any") || e.eq_ignore_ascii_case("localsubnet") {
        return true;
    }
    match (entry_span(e), v4_span(range)) {
        (Some((a, b)), Some((first, last))) => a <= last && first <= b,
        _ => false,
    }
}

fn all_profiles(profile: &str) -> bool {
    let p = profile.to_ascii_lowercase();
    p == "any"
        || ["domain", "private", "public"]
            .iter()
            .all(|x| p.contains(x))
}

/// Why `rule` does not let the relay's traffic in (empty: it does).
fn rule_problems(rule: &FwRule, expect: &Expect) -> Vec<String> {
    let mut out = Vec::new();
    if !rule.enabled {
        out.push("it is disabled".to_string());
    }
    if !rule.inbound {
        out.push("it is not an inbound rule".to_string());
    }
    if !rule.is_allow() {
        out.push(format!("its action is {}, not Allow", rule.action));
    }
    if !rule.tcp() {
        out.push(format!("it is for {}, not TCP", rule.protocol));
    }
    if !same_program(&rule.program, &expect.program) {
        let what = if rule.any_program() {
            "any program".to_string()
        } else {
            rule.program.clone()
        };
        out.push(format!(
            "it is for {what}, not the relay's program {}",
            expect.program
        ));
    }
    let have: Vec<PortRange> = rule
        .local_ports
        .iter()
        .filter_map(|p| PortRange::parse(p))
        .collect();
    if !covers(&have, &expect.ports) {
        out.push(format!(
            "its ports ({}) do not cover {}",
            if rule.local_ports.is_empty() {
                "none".to_string()
            } else {
                rule.local_ports.join(", ")
            },
            ranges_text(&expect.ports)
        ));
    }
    let range = container_range();
    if !rule
        .remote_addresses
        .iter()
        .any(|a| remote_covers(a, &range))
    {
        out.push(format!(
            "its remote addresses ({}) do not include {CONTAINER_RANGE}",
            if rule.remote_addresses.is_empty() {
                "none".to_string()
            } else {
                rule.remote_addresses.join(", ")
            }
        ));
    }
    if !rule.on_container_adapter() {
        out.push(format!(
            "it applies to {} interfaces only, not the container network's",
            rule.interface_type
        ));
    }
    if !all_profiles(&rule.profile) {
        out.push(format!(
            "it applies to the {} profile only, not every network profile",
            rule.profile
        ));
    }
    out
}

/// Whether `rule` drops traffic from containers to the relay: an enabled
/// inbound Block rule acting on the container adapter, either tied to the
/// program, or for any program on TCP, on one of the relay's ports, from
/// addresses that meet the container range.
fn drops_relay_traffic(rule: &FwRule, expect: &Expect) -> bool {
    if !(rule.is_block() && rule.inbound && rule.enabled && rule.on_container_adapter()) {
        return false;
    }
    if same_program(&rule.program, &expect.program) {
        return true;
    }
    if !rule.any_program() || !rule.tcp() {
        return false;
    }
    let ports: Vec<PortRange> = rule
        .local_ports
        .iter()
        .filter_map(|p| PortRange::parse(p))
        .collect();
    let range = container_range();
    let remote = rule.remote_addresses.is_empty()
        || rule
            .remote_addresses
            .iter()
            .any(|a| remote_meets(a, &range));
    remote
        && ports
            .iter()
            .any(|p| expect.ports.iter().any(|n| n.overlaps(p)))
}

/// Whether `rule` is one this module made: in [`GROUP`], or the allow rule
/// from before rules had a group (exact name, no group, the relay's
/// program).
pub fn is_ours(rule: &FwRule, expect: &Expect) -> bool {
    rule.group == GROUP
        || (rule.group.is_empty()
            && rule.display_name == RULE_NAME
            && same_program(&rule.program, &expect.program))
}

/// Judge what the check found.
pub fn evaluate(snap: &Snapshot, expect: &Expect) -> Report {
    let named: Vec<&FwRule> = snap
        .rules
        .iter()
        .filter(|r| r.display_name == RULE_NAME && is_ours(r, expect))
        .collect();
    let allow = if named.is_empty() {
        AllowState::Missing
    } else {
        let problems: Vec<Vec<String>> = named.iter().map(|r| rule_problems(r, expect)).collect();
        if problems.iter().any(Vec::is_empty) {
            AllowState::Present
        } else {
            let mut reasons = problems.into_iter().next().unwrap_or_default();
            if named.len() > 1 {
                reasons.push(format!(
                    "{} rules have that name and none of them fits",
                    named.len()
                ));
            }
            AllowState::Mismatch(reasons)
        }
    };
    let named_ports = normalize(
        named
            .iter()
            .flat_map(|r| r.local_ports.iter())
            .filter_map(|p| PortRange::parse(p))
            .filter(|r| *r != PortRange::ALL),
    );
    let mut blocks: Vec<String> = Vec::new();
    let mut foreign_blocks: Vec<ForeignBlock> = Vec::new();
    for r in &snap.rules {
        if r.group == GROUP || !drops_relay_traffic(r, expect) {
            continue;
        }
        let label = r.label();
        if same_program(&r.program, &expect.program) {
            if !blocks.contains(&label) {
                blocks.push(label);
            }
        } else if !foreign_blocks.iter().any(|f| f.label == label) {
            foreign_blocks.push(ForeignBlock {
                label,
                remove_line: check_rule_name(&r.name)
                    .ok()
                    .map(|_| format!("Remove-NetFirewallRule -Name {}", ps_quote(&r.name))),
            });
        }
    }
    let mut hardened: Vec<String> = Vec::new();
    for t in HARDEN_TYPES {
        let present = snap.rules.iter().any(|r| {
            r.display_name == block_rule_name(t)
                && r.group == GROUP
                && r.is_block()
                && r.inbound
                && r.enabled
                && same_program(&r.program, &expect.program)
                && r.interface_type.eq_ignore_ascii_case(t)
        });
        if present {
            hardened.push(t.to_string());
        }
    }
    let range = container_range();
    let outside_range = expect
        .gateways
        .iter()
        .copied()
        .filter(|g| !range.contains(*g))
        .collect();
    let mut open_interfaces = Vec::new();
    for i in &snap.interfaces {
        let what = match (i.forwarding, i.weak_host_receive) {
            (true, true) => "forwarding and weak-host receive",
            (true, false) => "forwarding",
            (false, true) => "weak-host receive",
            (false, false) => continue,
        };
        let family = if i.family.is_empty() {
            String::new()
        } else {
            format!(" ({})", i.family)
        };
        open_interfaces.push(format!("{}{family}: {what}", i.alias));
    }
    Report {
        allow,
        named_rule: !named.is_empty(),
        named_ports,
        blocks,
        foreign_blocks,
        hardened,
        outside_range,
        open_interfaces,
    }
}

// ------------------------------------------------------------ commands

/// A command run elevated: the statements the elevated PowerShell runs, in
/// order, and the argv that starts it (one elevation prompt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elevated {
    pub statements: Vec<String>,
    pub argv: Vec<String>,
}

impl Elevated {
    fn new(statements: Vec<String>) -> Elevated {
        let script = statements.join("; ");
        // `Start-Process -ArgumentList` joins its items with spaces, and the
        // elevated PowerShell joins the words after `-Command` back with
        // spaces: a script without double quotes, runs of spaces or line
        // breaks arrives unchanged.
        let argv = vec![
            "powershell.exe".to_string(),
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            format!(
                "$ErrorActionPreference = 'Stop'; $p = Start-Process powershell.exe -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile','-NonInteractive','-Command',{}; exit $p.ExitCode",
                ps_quote(&script)
            ),
        ];
        Elevated { statements, argv }
    }

    /// What a call must bring back: the command it confirmed.
    pub fn fingerprint(&self) -> String {
        let mut h = Sha256::new();
        for a in &self.argv {
            h.update(a.as_bytes());
            h.update([0u8]);
        }
        h.finalize()
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The elevated part, one statement per line, for the confirm.
    pub fn shown(&self) -> String {
        self.statements.join("\n")
    }
}

/// A PowerShell single-quoted string: nothing inside is expanded, and a
/// quote is written twice.
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// What the elevation transports intact and PowerShell takes literally in
/// single quotes: no double quote, backtick, typographic quote (PowerShell
/// treats those as quotes), control character or run of spaces.
fn transportable(s: &str) -> Result<(), char> {
    if let Some(c) = s.chars().find(|c| {
        c.is_control()
            || matches!(
                c,
                '"' | '`'
                    | '\u{2018}'
                    | '\u{2019}'
                    | '\u{201A}'
                    | '\u{201B}'
                    | '\u{201C}'
                    | '\u{201D}'
                    | '\u{201E}'
            )
    }) {
        return Err(c);
    }
    if s.contains("  ") {
        return Err(' ');
    }
    Ok(())
}

/// A program path that is safe to put into the elevated command: absolute
/// (`X:\...`), letters, digits and `\ : . _ - ( ) ~ ' + ,` and single
/// spaces only. Double quotes, backticks, `$`, wildcards, typographic
/// quotes (PowerShell treats them as quotes) and control characters are
/// refused, as is a run of spaces (the elevation would collapse it).
pub fn check_program_path(path: &str) -> Result<(), String> {
    let b = path.as_bytes();
    let absolute = b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
    if !absolute {
        return Err(format!(
            "the relay's program path {path:?} is not an absolute drive path"
        ));
    }
    if path.len() > 1024 {
        return Err("the relay's program path is too long".to_string());
    }
    if let Some(c) = path
        .chars()
        .skip(2)
        .find(|c| !(c.is_alphanumeric() || " \\._-()~'+,".contains(*c)))
    {
        return Err(format!(
            "the relay's program path has {c:?}, which is not put into an elevated command"
        ));
    }
    if path.contains("  ") || path.ends_with(' ') {
        return Err("the relay's program path has a run of spaces or ends with one".to_string());
    }
    Ok(())
}

/// A firewall rule's `Name` that is safe to name in the elevated command:
/// not empty, at most 1024 characters, no wildcard (`* ? [ ]`), nothing
/// [`transportable`] refuses, no surrounding space.
pub fn check_rule_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 1024 || name.trim() != name {
        return Err(format!(
            "the rule name {name:?} is empty, too long or padded"
        ));
    }
    if let Some(c) = name.chars().find(|c| matches!(c, '*' | '?' | '[' | ']')) {
        return Err(format!("the rule name {name:?} has the wildcard {c:?}"));
    }
    transportable(name).map_err(|c| format!("the rule name {name:?} has {c:?}"))
}

/// The statement that removes the allow rule made before rules had a
/// group: exact name, no group, the relay's program (quoted `prog`).
fn legacy_removal(prog: &str) -> String {
    format!(
        "Get-NetFirewallRule -DisplayName {} -ErrorAction SilentlyContinue | Where-Object {{ -not $_.Group -and ($_ | Get-NetFirewallApplicationFilter).Program -eq {prog} }} | Remove-NetFirewallRule",
        ps_quote(RULE_NAME)
    )
}

/// The one-step allow: drop our allow rule (with `harden`, every rule of
/// [`GROUP`]) and the relay program's inbound Block rules outside the
/// group, add the allow rule, and with `harden` the block rules on
/// [`HARDEN_TYPES`], all in [`GROUP`]. No other program's rule is ever
/// removed.
pub fn allow_command(program: &str, ports: &[PortRange], harden: bool) -> Result<Elevated, String> {
    check_program_path(program)?;
    check_rule_name(GROUP)?;
    let ports = normalize(ports.iter().copied());
    if ports.is_empty() {
        return Err("there are no relay ports to allow".to_string());
    }
    let port_list = ports
        .iter()
        .map(|r| ps_quote(&r.to_string()))
        .collect::<Vec<_>>()
        .join(",");
    let name = ps_quote(RULE_NAME);
    let group = ps_quote(GROUP);
    let prog = ps_quote(program);
    let mut s = vec!["$ErrorActionPreference = 'Stop'".to_string()];
    if harden {
        s.push(format!(
            "Get-NetFirewallRule -Group {group} -ErrorAction SilentlyContinue | Remove-NetFirewallRule"
        ));
    } else {
        s.push(format!(
            "Get-NetFirewallRule -Group {group} -ErrorAction SilentlyContinue | Where-Object {{ $_.DisplayName -eq {name} }} | Remove-NetFirewallRule"
        ));
    }
    s.push(legacy_removal(&prog));
    s.push(format!("Get-NetFirewallApplicationFilter -Program {prog} -ErrorAction SilentlyContinue | Get-NetFirewallRule | Where-Object {{ $_.Direction -eq 'Inbound' -and $_.Action -eq 'Block' -and $_.Group -ne {group} }} | Remove-NetFirewallRule"));
    s.push(format!(
        "New-NetFirewallRule -DisplayName {name} -Group {group} -Description {} -Direction Inbound -Action Allow -Protocol TCP -Program {prog} -LocalPort {port_list} -RemoteAddress {} -Profile Any | Out-Null",
        ps_quote(RULE_DESCRIPTION),
        ps_quote(CONTAINER_RANGE)
    ));
    if harden {
        for t in HARDEN_TYPES {
            s.push(format!(
                "New-NetFirewallRule -DisplayName {} -Group {group} -Description {} -Direction Inbound -Action Block -Program {prog} -InterfaceType {t} -Profile Any | Out-Null",
                ps_quote(&block_rule_name(t)),
                ps_quote(BLOCK_DESCRIPTION)
            ));
        }
    }
    Ok(Elevated::new(s))
}

/// Remove every rule of [`GROUP`], and the allow rule made before rules
/// had a group when `program` can be quoted: the cleanup.
pub fn remove_command(program: &str) -> Elevated {
    let mut s = vec![
        "$ErrorActionPreference = 'Stop'".to_string(),
        format!(
            "Get-NetFirewallRule -Group {} -ErrorAction SilentlyContinue | Remove-NetFirewallRule",
            ps_quote(GROUP)
        ),
    ];
    if check_program_path(program).is_ok() {
        s.push(legacy_removal(&ps_quote(program)));
    }
    Elevated::new(s)
}

// ---------------------------------------------------------------- view

/// The last check's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Not looked yet (the first check is running or has nothing to look at).
    Unchecked,
    Checked(Report),
    /// The check could not run; why.
    Failed(String),
}

/// What the Environment shows about the firewall, at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirewallView {
    pub outcome: Outcome,
    pub expect: Expect,
    pub checking: bool,
    /// An elevated command is waiting for the user or running.
    pub elevated_running: bool,
    /// How the last elevated command ended, when it did not succeed.
    pub last_failure: Option<String>,
}

const BUSY: &str = "Waiting for the elevated PowerShell to finish";

impl FirewallView {
    fn report(&self) -> Option<&Report> {
        match &self.outcome {
            Outcome::Checked(r) => Some(r),
            _ => None,
        }
    }

    /// The ports the allow rule gets: the relay's, plus the explicit ports
    /// the rule it replaces had.
    fn allow_ports(&self, report: &Report) -> Vec<PortRange> {
        normalize(
            self.expect
                .ports
                .iter()
                .chain(report.named_ports.iter())
                .copied(),
        )
    }

    fn allow_with(&self, report: &Report, harden: bool) -> Result<Elevated, String> {
        allow_command(&self.expect.program, &self.allow_ports(report), harden)
    }

    /// The allow command as it would run now, when it is offered.
    pub fn allow(&self) -> Option<Result<Elevated, String>> {
        let report = self.report()?;
        if !report.wants_allow() {
            return None;
        }
        Some(self.allow_with(report, false))
    }

    /// The allow-and-block command, offered whenever the optional block
    /// rules are not all in place (or the plain allow is offered).
    pub fn harden(&self) -> Option<Result<Elevated, String>> {
        let report = self.report()?;
        if !report.wants_allow() && report.is_hardened() {
            return None;
        }
        Some(self.allow_with(report, true))
    }

    /// The remove command, when there is a rule of ours to remove.
    pub fn remove(&self) -> Option<Elevated> {
        self.report()
            .filter(|r| r.has_own_rules())
            .map(|_| remove_command(&self.expect.program))
    }

    /// The command action `id` stands for, when it is offered now.
    pub fn offered(&self, id: &str) -> Option<Elevated> {
        match id {
            ALLOW_ACTION_ID => self.allow().and_then(Result::ok),
            HARDEN_ACTION_ID => self.harden().and_then(Result::ok),
            REMOVE_ACTION_ID => self.remove(),
            _ => None,
        }
    }

    fn action(&self, id: &str, label: &str, confirm: String, cmd: &Elevated) -> Value {
        let mut a = json!({
            "kind": "endpoint",
            "id": id,
            "label": label,
            "mutates": true,
            "confirm": confirm,
            "params": { "fingerprint": cmd.fingerprint() },
        });
        if self.elevated_running {
            a["disabled"] = BUSY.into();
        }
        a
    }

    fn allow_confirm(&self, report: &Report, cmd: &Elevated, harden: bool) -> String {
        let ports = ranges_text(&self.allow_ports(report));
        let blocks = match report.blocks.len() {
            0 => String::new(),
            1 => format!(
                ", and removes the block rule for this program {}",
                report.blocks[0]
            ),
            n => format!(
                ", and removes the {n} block rules for this program that drop its traffic ({})",
                report.blocks.join(", ")
            ),
        };
        let extra = if harden {
            format!(
                " It also adds inbound Block rules for {} on {} interfaces (wireless adapters, and the system's own VPN and dial-up connections; a third-party VPN adapter the firewall counts as another type is not covered), so the relay cannot be reached from those networks. Wired interfaces are left alone: the container network's adapter is one.",
                self.expect.program,
                HARDEN_TYPES.join(" and ")
            )
        } else {
            String::new()
        };
        format!(
            "Adds an inbound Windows Firewall rule named `{RULE_NAME}` that lets containers ({CONTAINER_RANGE}) reach {} on TCP ports {ports}, on every network profile, in the firewall group `{GROUP}`. It replaces our older rule{blocks}.{extra} Rules of other programs, or of any program, are never changed. Windows asks for administrator permission once. The elevated PowerShell runs:\n\n{}",
            self.expect.program,
            cmd.shown()
        )
    }

    pub fn allow_action(&self) -> Option<Value> {
        let cmd = self.allow()?.ok()?;
        let report = self.report()?;
        Some(self.action(
            ALLOW_ACTION_ID,
            "Allow the relay through Windows Firewall",
            self.allow_confirm(report, &cmd, false),
            &cmd,
        ))
    }

    pub fn harden_action(&self) -> Option<Value> {
        let cmd = self.harden()?.ok()?;
        let report = self.report()?;
        Some(self.action(
            HARDEN_ACTION_ID,
            "Allow the relay and block it on other networks",
            self.allow_confirm(report, &cmd, true),
            &cmd,
        ))
    }

    pub fn remove_action(&self) -> Option<Value> {
        let cmd = self.remove()?;
        let confirm = format!(
            "Removes the Windows Firewall rules in the group `{GROUP}` (the relay's allow rule and its optional block rules) and an allow rule named `{RULE_NAME}` for this program from before rules had a group; no other rule is touched. Containers cannot reach the relay afterwards until it is allowed again. Windows asks for administrator permission once. The elevated PowerShell runs:\n\n{}",
            cmd.shown()
        );
        Some(self.action(
            REMOVE_ACTION_ID,
            "Remove the relay's firewall rules",
            confirm,
            &cmd,
        ))
    }

    /// The banners for what the last check found: dropped traffic is
    /// `bad`, the rest `warn`.
    pub fn banners(&self) -> Vec<Value> {
        let mut out = Vec::new();
        let report = match &self.outcome {
            Outcome::Unchecked => return out,
            Outcome::Failed(why) => {
                out.push(json!({
                    "id": CHECK_BANNER,
                    "tone": "warn",
                    "title": "The relay's Windows Firewall check could not run",
                    "lines": [
                        why,
                        "The relay keeps running; if containers cannot reach it, look at the firewall's inbound rules for the relay by hand.",
                    ],
                }));
                return out;
            }
            Outcome::Checked(r) => r,
        };
        if report.wants_allow() || report.is_dropping() {
            let mut lines = Vec::new();
            if !report.blocks.is_empty() {
                lines.push(format!(
                    "Windows Firewall is dropping every connection from containers to the relay: the Block rule{} {} {} it, and a Block rule wins over any allow rule. Cancelling the system's allow-access prompt leaves such rules.",
                    if report.blocks.len() == 1 { "" } else { "s" },
                    report.blocks.join(", "),
                    if report.blocks.len() == 1 { "matches" } else { "match" },
                ));
            }
            if !report.foreign_blocks.is_empty() {
                lines.push(format!(
                    "Windows Firewall is dropping container traffic to the relay through Block rules that are not the relay's to change: {}. Remove each one, or narrow it so it no longer covers the relay's ports, yourself (from an elevated PowerShell):",
                    report
                        .foreign_blocks
                        .iter()
                        .map(|f| f.label.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                for f in &report.foreign_blocks {
                    lines.push(match &f.remove_line {
                        Some(line) => line.clone(),
                        None => format!(
                            "{}: its name cannot be written into a command safely; remove it in the firewall console",
                            f.label
                        ),
                    });
                }
            }
            match &report.allow {
                AllowState::Present => {}
                AllowState::Missing => lines.push(format!(
                    "Containers reach the relay only through an inbound rule for its program; there is no rule named `{RULE_NAME}`."
                )),
                AllowState::Mismatch(reasons) => lines.push(format!(
                    "The rule `{RULE_NAME}` does not let them in: {}.",
                    reasons.join("; ")
                )),
            }
            let (title, tone) = match (&report.allow, !report.is_dropping()) {
                (_, false) => (
                    "Windows Firewall is dropping container traffic to the relay",
                    "bad",
                ),
                (AllowState::Mismatch(_), true) => {
                    ("The relay's Windows Firewall rule does not match", "warn")
                }
                _ => ("Windows Firewall has no rule for the relay", "warn"),
            };
            if let Some(Err(e)) = self.allow() {
                lines.push(format!("The rule cannot be added from here: {e}."));
            }
            if let Some(f) = &self.last_failure {
                lines.push(f.clone());
            }
            let mut b = json!({ "id": RULE_BANNER, "tone": tone, "title": title, "lines": lines });
            let actions: Vec<Value> = if report.wants_allow() {
                self.allow_action()
                    .into_iter()
                    .chain(self.harden_action())
                    .collect()
            } else {
                Vec::new()
            };
            if !actions.is_empty() {
                b["actions"] = actions.into();
            }
            out.push(b);
        }
        if !report.outside_range.is_empty() {
            let addrs: Vec<String> = report.outside_range.iter().map(|a| a.to_string()).collect();
            out.push(json!({
                "id": RANGE_BANNER,
                "tone": "warn",
                "title": format!("The relay's gateway {} is outside the firewall rule's range", addrs.join(", ")),
                "lines": [format!(
                    "The rule `{RULE_NAME}` lets in {CONTAINER_RANGE} only, so containers on the network of {} are blocked unless another rule lets them in.",
                    addrs.join(", ")
                )],
            }));
        }
        if !report.open_interfaces.is_empty() {
            let mut lines = vec![
                "Binding the gateway address alone no longer keeps other networks away from the relay; it still closes every connection from outside the container network.".to_string(),
            ];
            lines.extend(report.open_interfaces.iter().cloned());
            out.push(json!({
                "id": FORWARDING_BANNER,
                "tone": "warn",
                "title": "Forwarding or weak-host receive is on",
                "lines": lines,
            }));
        }
        out
    }

    /// The alerts: dropped traffic, `bad`.
    pub fn alerts(&self) -> Vec<Value> {
        match self.report() {
            Some(r) if r.is_dropping() => vec![json!({
                "text": format!(
                    "Windows Firewall is dropping container traffic to the relay ({})",
                    r.dropping_labels().join(", ")
                ),
                "severity": "bad",
            })],
            _ => Vec::new(),
        }
    }

    /// The relay section's firewall row.
    pub fn row(&self) -> Value {
        let mut value = match &self.outcome {
            Outcome::Unchecked => "checking…".to_string(),
            Outcome::Failed(why) => format!("not checked: {why}"),
            Outcome::Checked(r) => {
                let mut v = match &r.allow {
                    AllowState::Present => format!("allowed by the rule `{RULE_NAME}`"),
                    AllowState::Missing => format!("no rule `{RULE_NAME}`"),
                    AllowState::Mismatch(_) => format!("the rule `{RULE_NAME}` does not match"),
                };
                if r.is_dropping() {
                    v.push_str(&format!("; dropped by {}", r.dropping_labels().join(", ")));
                }
                if !r.hardened.is_empty() {
                    v.push_str(&format!(
                        "; blocked on {} networks",
                        r.hardened.join(" and ")
                    ));
                }
                v
            }
        };
        if self.elevated_running {
            value.push_str(" (waiting for the elevated PowerShell)");
        } else if self.checking && !matches!(self.outcome, Outcome::Unchecked) {
            value.push_str(" (checking again)");
        }
        let mut row = json!({ "label": "Windows Firewall", "value": value });
        let badge = match self.report() {
            Some(r) if r.is_dropping() => Some(("dropped", "bad")),
            Some(r) if r.is_clean() => None,
            None if matches!(self.outcome, Outcome::Unchecked) => None,
            _ => Some(("check", "warn")),
        };
        if let Some((text, tone)) = badge {
            row["badges"] = json!([{ "text": text, "tone": tone }]);
        }
        row
    }

    /// The relay section's actions: hardening when the allow is not
    /// already offered in a banner, and the cleanup.
    pub fn section_actions(&self) -> Vec<Value> {
        let mut out = Vec::new();
        if self.report().is_some_and(|r| !r.wants_allow()) {
            out.extend(self.harden_action());
        }
        out.extend(self.remove_action());
        out
    }
}

/// Put `fw` into an Environment payload: its banners and alerts, and its
/// row and actions in the facts section `relay` (when there is one).
pub fn decorate(v: &mut Value, fw: &FirewallView) {
    if let Some(banners) = v.get_mut("banners").and_then(Value::as_array_mut) {
        banners.extend(fw.banners());
    }
    if let Some(alerts) = v.get_mut("alerts").and_then(Value::as_array_mut) {
        alerts.extend(fw.alerts());
    }
    let section = v
        .get_mut("facts")
        .and_then(Value::as_array_mut)
        .and_then(|f| f.iter_mut().find(|s| s["id"] == "relay"));
    if let Some(section) = section {
        if let Some(rows) = section.get_mut("rows").and_then(Value::as_array_mut) {
            rows.push(fw.row());
        }
        let actions = fw.section_actions();
        if !actions.is_empty() {
            match section.get_mut("actions").and_then(Value::as_array_mut) {
                Some(list) => list.extend(actions),
                None => section["actions"] = actions.into(),
            }
        }
    }
}

// --------------------------------------------------------------- check

#[derive(Debug)]
struct Inner {
    expect: Option<Expect>,
    outcome: Outcome,
    /// Bumped by every check started; a check's result counts only while
    /// it is the latest.
    generation: u64,
    checking: bool,
    /// When the last check started.
    started: Option<Instant>,
    elevated_running: bool,
    last_failure: Option<String>,
}

/// The firewall check of one relay, and its elevated commands.
pub struct FirewallCheck {
    os: HostOs,
    shell: Arc<dyn EngineCli + Send + Sync>,
    /// The program the rule is for; `None` when it cannot be told.
    program: Option<String>,
    inner: Arc<Mutex<Inner>>,
    recheck: Duration,
}

/// This process's own program path as Windows stores it in a rule.
pub fn own_program() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let text = exe.display().to_string();
    Some(
        text.strip_prefix(r"\\?\")
            .map(str::to_string)
            .unwrap_or(text),
    )
}

impl FirewallCheck {
    pub fn new(
        os: HostOs,
        shell: Arc<dyn EngineCli + Send + Sync>,
        program: Option<String>,
    ) -> FirewallCheck {
        FirewallCheck {
            os,
            shell,
            program,
            inner: Arc::new(Mutex::new(Inner {
                expect: None,
                outcome: Outcome::Unchecked,
                generation: 0,
                checking: false,
                started: None,
                elevated_running: false,
                last_failure: None,
            })),
            recheck: RECHECK,
        }
    }

    /// The same check with another least time between view-driven checks.
    pub fn with_recheck(mut self, recheck: Duration) -> FirewallCheck {
        self.recheck = recheck;
        self
    }

    /// The check of this machine, for this process.
    pub fn real() -> FirewallCheck {
        FirewallCheck::new(
            HostOs::current(),
            Arc::new(stack_core::cli::CliRunner),
            own_program(),
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn twin(&self) -> FirewallCheck {
        FirewallCheck {
            os: self.os,
            shell: self.shell.clone(),
            program: self.program.clone(),
            inner: self.inner.clone(),
            recheck: self.recheck,
        }
    }

    /// Only Windows Firewall is looked at.
    pub fn applies(&self) -> bool {
        self.os == HostOs::Windows
    }

    /// The relay as it is now: check again when its gateways or ports
    /// changed, when it just bound a listener (`rebound`), or when the last
    /// check started [`RECHECK`] ago or more. No gateway means nothing can
    /// listen yet, so nothing is checked.
    pub fn want(&self, gateways: Vec<IpAddr>, ports: Vec<PortRange>, rebound: bool) {
        if !self.applies() || gateways.is_empty() {
            return;
        }
        let expect = Expect {
            program: self.program.clone().unwrap_or_default(),
            ports,
            gateways,
        };
        let mut inner = self.lock();
        let changed = inner.expect.as_ref() != Some(&expect);
        let stale = inner.started.is_none_or(|t| t.elapsed() >= self.recheck);
        if !changed && (inner.checking || !(rebound || stale)) {
            return;
        }
        inner.expect = Some(expect);
        drop(inner);
        self.start_check();
    }

    fn start_check(&self) {
        let mut inner = self.lock();
        let Some(expect) = inner.expect.clone() else {
            return;
        };
        inner.generation += 1;
        inner.started = Some(Instant::now());
        let generation = inner.generation;
        if self.program.is_none() {
            inner.outcome = Outcome::Failed(
                "the relay's own program path cannot be told, so its rule cannot be looked up"
                    .to_string(),
            );
            inner.checking = false;
            return;
        }
        inner.checking = true;
        drop(inner);
        let shell = self.shell.clone();
        let state = self.inner.clone();
        thread::spawn(move || {
            let outcome = run_check(&*shell, &expect);
            let mut inner = state.lock().unwrap_or_else(|p| p.into_inner());
            if inner.generation == generation {
                inner.outcome = outcome;
                inner.checking = false;
            }
        });
    }

    /// The view now; `None` when there is nothing to say (not Windows, or
    /// no gateway seen yet).
    pub fn view(&self) -> Option<FirewallView> {
        if !self.applies() {
            return None;
        }
        let inner = self.lock();
        Some(FirewallView {
            outcome: inner.outcome.clone(),
            expect: inner.expect.clone()?,
            checking: inner.checking,
            elevated_running: inner.elevated_running,
            last_failure: inner.last_failure.clone(),
        })
    }

    /// Whether `id` is one of this module's actions.
    pub fn is_action(id: &str) -> bool {
        matches!(id, ALLOW_ACTION_ID | HARDEN_ACTION_ID | REMOVE_ACTION_ID)
    }

    /// Run the elevated command of action `id` (the confirmed
    /// `fingerprint` must match the command offered now), off the request
    /// path; the check runs again once it ends. The message says what
    /// happens next.
    pub fn run(&self, id: &str, fingerprint: Option<&str>) -> Result<String, (u16, String)> {
        let view = self.view().ok_or((
            409,
            "the relay's firewall is not looked after on this machine".to_string(),
        ))?;
        if view.elevated_running {
            return Err((409, format!("{BUSY} first")));
        }
        let Some(cmd) = view.offered(id) else {
            return Err((
                409,
                format!(
                    "`{id}` is not offered now (the firewall changed since it was shown); look again"
                ),
            ));
        };
        if fingerprint != Some(cmd.fingerprint().as_str()) {
            return Err((
                409,
                format!(
                    "the command changed since it was shown; what would run now is:\n\n{}",
                    cmd.shown()
                ),
            ));
        }
        self.lock().elevated_running = true;
        let shell = self.shell.clone();
        let state = self.inner.clone();
        let me = self.twin();
        thread::spawn(move || {
            let failure = run_elevated(&*shell, &cmd);
            {
                let mut inner = state.lock().unwrap_or_else(|p| p.into_inner());
                inner.elevated_running = false;
                inner.last_failure = failure;
                // Marked before the lock is let go, so nobody sees the
                // change done but the check not yet started.
                inner.checking = me.program.is_some();
            }
            me.start_check();
        });
        Ok("Windows asks for administrator permission now; the relay's firewall is checked again once the elevated PowerShell ends.".to_string())
    }
}

fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

fn run_check(shell: &dyn EngineCli, expect: &Expect) -> Outcome {
    match shell.run(&check_invocation(&expect.program)) {
        Ok(out) if out.ok() => match parse_snapshot(&out.stdout) {
            Ok(snap) => Outcome::Checked(evaluate(&snap, expect)),
            Err(e) => Outcome::Failed(e),
        },
        Ok(out) => Outcome::Failed(format!(
            "PowerShell exited with {}: {}",
            out.status.map_or("no code".to_string(), |c| c.to_string()),
            first_line(&out.stderr)
        )),
        Err(CliError::Spawn { message, .. }) => {
            Outcome::Failed(format!("PowerShell could not start ({message})"))
        }
        Err(CliError::Timeout { seconds, .. }) => {
            Outcome::Failed(format!("PowerShell did not answer within {seconds} s"))
        }
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// `None` when it succeeded, else what to tell the user.
fn run_elevated(shell: &dyn EngineCli, cmd: &Elevated) -> Option<String> {
    let mut inv = Invocation::new(&cmd.argv[0], &[]);
    inv.args = cmd.argv[1..].to_vec();
    inv.timeout_secs = ELEVATED_TIMEOUT_SECS;
    match shell.run(&inv) {
        Ok(out) if out.ok() => None,
        Ok(out) => {
            let why = first_line(&out.stderr);
            Some(
                if why.to_ascii_lowercase().contains("canceled by the user") {
                    "The last firewall change did not run: the elevation prompt was cancelled."
                        .to_string()
                } else {
                    format!(
                        "The last firewall change did not finish (exit {}){}",
                        out.status.map_or("unknown".to_string(), |c| c.to_string()),
                        if why.is_empty() {
                            String::new()
                        } else {
                            format!(": {why}")
                        }
                    )
                },
            )
        }
        Err(e) => Some(format!("The last firewall change did not run: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stack_core::cli::CliOutput;
    use std::time::{Duration, Instant};

    const PROGRAM: &str = r"C:\Apps\Chappa'Ai\chappa-stack.exe";

    /// The shape the check prints: our allow rule, the two block rules a
    /// cancelled prompt leaves (TCP and UDP), and two interfaces.
    const SAMPLE: &str = r#"{"rules":[{"name":"{00000000-0000-0000-0000-000000000001}","displayName":"Chappa'Ai relay","group":"Chappa'Ai relay","enabled":"True","direction":"Inbound","action":"Allow","profile":"Any","program":"C:\\Apps\\Chappa'Ai\\chappa-stack.exe","protocol":"TCP","localPort":["14000-14399"],"remoteAddress":["172.16.0.0/255.240.0.0"]},{"name":"TCP Query User{00000000-0000-0000-0000-000000000002}C:\\apps\\chappa'ai\\chappa-stack.exe","displayName":"chappa-stack","enabled":"True","direction":"Inbound","action":"Block","profile":"Public","program":"C:\\apps\\chappa'ai\\chappa-stack.exe","protocol":"TCP","localPort":["Any"],"remoteAddress":["Any"]},{"name":"UDP Query User{00000000-0000-0000-0000-000000000003}C:\\apps\\chappa'ai\\chappa-stack.exe","displayName":"chappa-stack","enabled":"True","direction":"Inbound","action":"Block","profile":"Public","program":"C:\\apps\\chappa'ai\\chappa-stack.exe","protocol":"UDP","localPort":["Any"],"remoteAddress":["Any"]}],"interfaces":[{"alias":"vEthernet (nat)","family":"IPv4","forwarding":"Disabled","weakHostReceive":"Disabled"},{"alias":"Ethernet","family":"IPv4","forwarding":"Disabled","weakHostReceive":"Disabled"}]}"#;

    fn expect() -> Expect {
        Expect {
            program: PROGRAM.into(),
            ports: vec![PortRange {
                lo: 14000,
                hi: 14399,
            }],
            gateways: vec!["172.29.80.1".parse().unwrap()],
        }
    }

    fn allow_rule() -> FwRule {
        parse_snapshot(SAMPLE).unwrap().rules[0].clone()
    }

    fn snap(rules: Vec<FwRule>) -> Snapshot {
        Snapshot {
            rules,
            interfaces: parse_snapshot(SAMPLE).unwrap().interfaces,
        }
    }

    #[test]
    fn the_check_output_parses_rules_and_interfaces() {
        let s = parse_snapshot(SAMPLE).unwrap();
        assert_eq!(s.rules.len(), 3);
        let r = &s.rules[0];
        assert_eq!(r.display_name, RULE_NAME);
        assert!(r.enabled && r.inbound && r.is_allow());
        assert_eq!(r.program, PROGRAM);
        assert_eq!(r.local_ports, vec!["14000-14399"]);
        assert_eq!(r.remote_addresses, vec!["172.16.0.0/255.240.0.0"]);
        assert!(s.rules[1].is_block());
        assert_eq!(s.rules[2].protocol, "UDP");
        assert_eq!(s.interfaces.len(), 2);
        assert_eq!(s.interfaces[0].alias, "vEthernet (nat)");
        assert!(!s.interfaces[0].forwarding && !s.interfaces[0].weak_host_receive);
    }

    #[test]
    fn the_parser_takes_what_powershell_writes_for_one_item_or_none() {
        // One rule as an object, ports as a bare string, nulls and an empty
        // list: what PowerShell can print for a list of one or none.
        let one = r#"{"rules":{"displayName":"Chappa'Ai relay","group":"Chappa'Ai relay","enabled":"True","direction":"Inbound","action":"Allow","profile":"Domain, Private, Public","program":"C:\\x.exe","protocol":"Any","localPort":"14001","remoteAddress":null},"interfaces":[]}"#;
        let s = parse_snapshot(one).unwrap();
        assert_eq!(s.rules.len(), 1);
        assert_eq!(s.rules[0].local_ports, vec!["14001"]);
        assert!(s.rules[0].remote_addresses.is_empty());
        assert!(s.interfaces.is_empty());
        let none = parse_snapshot("\r\n{\"rules\":[],\"interfaces\":[]}\r\n").unwrap();
        assert_eq!(none, Snapshot::default());
        assert!(parse_snapshot("").is_err());
        assert!(parse_snapshot("WARNING: something").is_err());
        assert!(parse_snapshot("[1,2]").is_err());
    }

    #[test]
    fn interfaces_with_forwarding_or_weak_host_receive_are_named() {
        let text = r#"{"rules":[],"interfaces":[
            {"alias":"vEthernet (nat)","family":"IPv4","forwarding":"Enabled","weakHostReceive":"Disabled"},
            {"alias":"Ethernet","family":"IPv6","forwarding":"Disabled","weakHostReceive":"Enabled"},
            {"alias":"Wi-Fi","family":"IPv4","forwarding":"Enabled","weakHostReceive":"Enabled"},
            {"alias":"Loopback","family":"IPv4","forwarding":"Disabled","weakHostReceive":"Disabled"}]}"#;
        let r = evaluate(&parse_snapshot(text).unwrap(), &expect());
        assert_eq!(
            r.open_interfaces,
            vec![
                "vEthernet (nat) (IPv4): forwarding",
                "Ethernet (IPv6): weak-host receive",
                "Wi-Fi (IPv4): forwarding and weak-host receive",
            ]
        );
        assert!(!r.is_clean());
    }

    #[test]
    fn rules_are_judged_against_the_program_ports_range_and_profiles() {
        // Present: the sample's allow rule alone.
        let r = evaluate(&snap(vec![allow_rule()]), &expect());
        assert_eq!(r.allow, AllowState::Present);
        assert!(r.is_clean() && r.named_rule && !r.wants_allow());
        assert_eq!(
            r.named_ports,
            vec![PortRange {
                lo: 14000,
                hi: 14399
            }]
        );

        // Same path, other case and separators: still the same program.
        let mut rule = allow_rule();
        rule.program = r"c:/apps/CHAPPA'AI/chappa-stack.exe".into();
        assert_eq!(
            evaluate(&snap(vec![rule]), &expect()).allow,
            AllowState::Present
        );

        // None at all.
        let r = evaluate(&snap(vec![]), &expect());
        assert_eq!(r.allow, AllowState::Missing);
        assert!(!r.named_rule && r.wants_allow());

        let mismatch = |f: &dyn Fn(&mut FwRule)| {
            let mut rule = allow_rule();
            f(&mut rule);
            match evaluate(&snap(vec![rule]), &expect()).allow {
                AllowState::Mismatch(reasons) => reasons.join("; "),
                other => panic!("expected a mismatch, got {other:?}"),
            }
        };
        let why = mismatch(&|r| r.program = r"C:\Old\chappa-stack.exe".into());
        assert!(
            why.contains(r"it is for C:\Old\chappa-stack.exe, not the relay's program"),
            "{why}"
        );
        let why = mismatch(&|r| r.program = "Any".into());
        assert!(why.contains("it is for any program"), "{why}");
        let why = mismatch(&|r| r.local_ports = vec!["14000-14099".into()]);
        assert!(
            why.contains("its ports (14000-14099) do not cover 14000-14399"),
            "{why}"
        );
        let why = mismatch(&|r| r.local_ports = vec!["RPC".into()]);
        assert!(why.contains("its ports (RPC)"), "{why}");
        let why = mismatch(&|r| r.remote_addresses = vec!["LocalSubnet".into()]);
        assert!(why.contains("do not include 172.16.0.0/12"), "{why}");
        let why = mismatch(&|r| r.remote_addresses = vec!["172.17.0.0/16".into()]);
        assert!(why.contains("do not include"), "{why}");
        assert!(mismatch(&|r| r.enabled = false).contains("disabled"));
        assert!(mismatch(&|r| r.profile = "Private".into()).contains("Private profile only"));
        assert!(mismatch(&|r| r.protocol = "UDP".into()).contains("for UDP, not TCP"));
        assert!(mismatch(&|r| r.action = "Block".into()).contains("Block, not Allow"));

        // Wider is fine: any port, a remote range spanning the container
        // range, every profile spelled out.
        let mut rule = allow_rule();
        rule.local_ports = vec!["Any".into()];
        rule.remote_addresses = vec!["172.0.0.0-172.63.255.255".into()];
        rule.profile = "Domain, Private, Public".into();
        assert_eq!(
            evaluate(&snap(vec![rule.clone()]), &expect()).allow,
            AllowState::Present
        );
        // ... and an Any port is not carried into a replacement.
        assert!(evaluate(&snap(vec![rule]), &expect())
            .named_ports
            .is_empty());

        // Two rules of the name: one that fits is enough.
        let mut bad = allow_rule();
        bad.enabled = false;
        let r = evaluate(&snap(vec![bad.clone(), allow_rule()]), &expect());
        assert_eq!(r.allow, AllowState::Present);
        let r = evaluate(&snap(vec![bad.clone(), bad]), &expect());
        assert!(
            matches!(r.allow, AllowState::Mismatch(ref w) if w.iter().any(|x| x.contains("2 rules have that name")))
        );
    }

    #[test]
    fn block_rules_for_the_program_are_found_by_path() {
        let s = parse_snapshot(SAMPLE).unwrap();
        let r = evaluate(&s, &expect());
        assert_eq!(r.allow, AllowState::Present);
        assert_eq!(r.blocks, vec!["chappa-stack (TCP)", "chappa-stack (UDP)"]);
        assert!(r.wants_allow(), "a block rule wins over the allow rule");
        // A block rule for another program, or a disabled one, is not ours.
        let mut other = s.rules[1].clone();
        other.program = r"C:\Other\tool.exe".into();
        let mut off = s.rules[1].clone();
        off.enabled = false;
        let r = evaluate(&snap(vec![allow_rule(), other, off]), &expect());
        assert!(r.blocks.is_empty());
    }

    #[test]
    fn a_gateway_outside_the_container_range_is_named() {
        let mut e = expect();
        e.gateways = vec![
            "172.29.80.1".parse().unwrap(),
            "172.31.255.254".parse().unwrap(),
            "10.0.75.1".parse().unwrap(),
            "192.168.65.1".parse().unwrap(),
            "fd00::1".parse().unwrap(),
        ];
        let r = evaluate(&snap(vec![allow_rule()]), &e);
        let outside: Vec<String> = r.outside_range.iter().map(|a| a.to_string()).collect();
        assert_eq!(outside, vec!["10.0.75.1", "192.168.65.1", "fd00::1"]);
    }

    #[test]
    fn ports_parse_merge_and_cover() {
        assert_eq!(PortRange::parse("Any"), Some(PortRange::ALL));
        assert_eq!(
            PortRange::parse("14001"),
            Some(PortRange {
                lo: 14001,
                hi: 14001
            })
        );
        assert_eq!(
            PortRange::parse(" 14000 - 14399 "),
            Some(PortRange {
                lo: 14000,
                hi: 14399
            })
        );
        for bad in ["0", "14399-14000", "RPC", "", "70000", "1-2-3"] {
            assert_eq!(PortRange::parse(bad), None, "{bad}");
        }
        let n = normalize([
            PortRange {
                lo: 15000,
                hi: 15000,
            },
            PortRange {
                lo: 14000,
                hi: 14199,
            },
            PortRange {
                lo: 14200,
                hi: 14399,
            },
            PortRange {
                lo: 14100,
                hi: 14150,
            },
        ]);
        assert_eq!(ranges_text(&n), "14000-14399, 15000");
        assert!(covers(
            &n,
            &[PortRange {
                lo: 14000,
                hi: 14399
            }]
        ));
        assert!(!covers(
            &n,
            &[PortRange {
                lo: 14000,
                hi: 14400
            }]
        ));
        assert!(covers(&[PortRange::ALL], &n));
        assert!(covers(&[], &[]));

        let relayed: BTreeSet<u16> = [14001, 14002, 20000].into_iter().collect();
        assert_eq!(
            ranges_text(&stack_ports(14000, 100, 4, &relayed)),
            "14000-14399, 20000"
        );
        assert_eq!(
            ranges_text(&stack_ports(65500, 100, 2, &BTreeSet::new())),
            "65500-65535"
        );
        assert_eq!(
            ranges_text(&stack_ports(14000, 0, 4, &BTreeSet::new())),
            "none"
        );
    }

    /// The inner script as the outer command carries it.
    fn carried(cmd: &Elevated) -> String {
        let outer = &cmd.argv[4];
        let start = outer.find("'-Command',").unwrap() + "'-Command',".len();
        let end = outer.rfind("; exit $p.ExitCode").unwrap();
        let quoted = &outer[start..end];
        assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
        quoted[1..quoted.len() - 1].replace("''", "'")
    }

    #[test]
    fn the_allow_command_quotes_every_value_and_elevates_once() {
        let ports = [
            PortRange {
                lo: 14000,
                hi: 14399,
            },
            PortRange {
                lo: 15000,
                hi: 15000,
            },
        ];
        let cmd = allow_command(PROGRAM, &ports, false).unwrap();
        assert_eq!(
            cmd.argv[..4],
            [
                "powershell.exe",
                "-NoProfile",
                "-NonInteractive",
                "-Command"
            ]
            .map(String::from)
        );
        assert!(cmd.argv[4].starts_with(
            "$ErrorActionPreference = 'Stop'; $p = Start-Process powershell.exe -Verb RunAs -Wait -PassThru"
        ));
        assert_eq!(cmd.argv[4].matches("-Verb RunAs").count(), 1);
        assert_eq!(carried(&cmd), cmd.statements.join("; "));
        let script = cmd.shown();
        assert!(script.contains(r"Get-NetFirewallRule -DisplayName 'Chappa''Ai relay' -ErrorAction SilentlyContinue | Where-Object { -not $_.Group -and ($_ | Get-NetFirewallApplicationFilter).Program -eq 'C:\Apps\Chappa''Ai\chappa-stack.exe' } | Remove-NetFirewallRule"), "the allow rule from before groups");
        assert!(script.contains(r"Get-NetFirewallApplicationFilter -Program 'C:\Apps\Chappa''Ai\chappa-stack.exe' -ErrorAction SilentlyContinue | Get-NetFirewallRule | Where-Object { $_.Direction -eq 'Inbound' -and $_.Action -eq 'Block' -and $_.Group -ne 'Chappa''Ai relay' } | Remove-NetFirewallRule"));
        assert!(script.contains("Get-NetFirewallRule -Group 'Chappa''Ai relay' -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -eq 'Chappa''Ai relay' } | Remove-NetFirewallRule"));
        assert!(
            !script.contains("-InterfaceType"),
            "the plain allow adds no block rule"
        );
        assert!(script.contains(r"New-NetFirewallRule -DisplayName 'Chappa''Ai relay' -Group 'Chappa''Ai relay' -Description 'Lets containers reach the stack relay on its gateway address' -Direction Inbound -Action Allow -Protocol TCP -Program 'C:\Apps\Chappa''Ai\chappa-stack.exe' -LocalPort '14000-14399','15000' -RemoteAddress '172.16.0.0/12' -Profile Any | Out-Null"));
        // What the elevation transports intact: no double quote, no line
        // break, no run of spaces.
        let joined = cmd.statements.join("; ");
        assert!(!joined.contains('"') && !joined.contains('\n') && !joined.contains("  "));

        // The confirm's fingerprint moves with what would run.
        let other = allow_command(PROGRAM, &ports[..1], false).unwrap();
        assert_ne!(cmd.fingerprint(), other.fingerprint());
        assert_eq!(
            cmd.fingerprint(),
            allow_command(PROGRAM, &ports, false).unwrap().fingerprint()
        );

        let remove = remove_command(PROGRAM);
        assert_eq!(carried(&remove), remove.statements.join("; "));
        assert!(remove.shown().contains(
            "Get-NetFirewallRule -Group 'Chappa''Ai relay' -ErrorAction SilentlyContinue | Remove-NetFirewallRule"
        ));
        assert!(remove.shown().contains(r"Get-NetFirewallRule -DisplayName 'Chappa''Ai relay' -ErrorAction SilentlyContinue | Where-Object { -not $_.Group -and ($_ | Get-NetFirewallApplicationFilter).Program -eq 'C:\Apps\Chappa''Ai\chappa-stack.exe' } | Remove-NetFirewallRule"));
        assert!(!remove.shown().contains("New-NetFirewallRule"));
        assert!(!remove.shown().contains("relay*"), "never a name prefix");
        assert!(!remove.shown().contains("-Name "));
        // Without a path that can be quoted, only the group goes.
        assert_eq!(remove_command("relative.exe").statements.len(), 2);
    }

    #[test]
    fn unsafe_program_paths_and_empty_ports_get_no_command() {
        let ports = [PortRange {
            lo: 14000,
            hi: 14399,
        }];
        assert!(allow_command(
            r"C:\Program Files (x86)\App ~1\chappa-stack.exe",
            &ports,
            false
        )
        .is_ok());
        assert!(allow_command("D:\\Donn\u{e9}es\\chappa-stack.exe", &ports, false).is_ok());
        for bad in [
            r"chappa-stack.exe",
            r"\\server\share\chappa-stack.exe",
            r"C:relative\chappa-stack.exe",
            "/usr/local/bin/chappa-stack",
            r#"C:\a"b\chappa-stack.exe"#,
            r"C:\a`b\chappa-stack.exe",
            r"C:\$env\chappa-stack.exe",
            r"C:\a*\chappa-stack.exe",
            r"C:\a[1]\chappa-stack.exe",
            "C:\\a\u{2019}b\\chappa-stack.exe",
            "C:\\a\nb\\chappa-stack.exe",
            r"C:\a  b\chappa-stack.exe",
            r"C:\a;b\chappa-stack.exe",
            r"C:\a|b\chappa-stack.exe",
        ] {
            assert!(allow_command(bad, &ports, false).is_err(), "{bad:?}");
        }
        assert!(allow_command(PROGRAM, &[], false).is_err());
    }

    fn view(outcome: Outcome) -> FirewallView {
        FirewallView {
            outcome,
            expect: expect(),
            checking: false,
            elevated_running: false,
            last_failure: None,
        }
    }

    fn ids(v: &[Value]) -> Vec<&str> {
        v.iter().filter_map(|b| b["id"].as_str()).collect()
    }

    fn has_line(banner: &Value, part: &str) -> bool {
        banner["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str().unwrap().contains(part))
    }

    #[test]
    fn each_finding_gets_its_banner_and_the_fix_where_there_is_one() {
        // Clean: no banner, a quiet row, the remove action on the section.
        let clean = view(Outcome::Checked(evaluate(
            &snap(vec![allow_rule()]),
            &expect(),
        )));
        assert!(clean.banners().is_empty());
        assert_eq!(
            clean.row()["value"],
            "allowed by the rule `Chappa'Ai relay`"
        );
        assert!(clean.row().get("badges").is_none());
        let actions = clean.section_actions();
        assert_eq!(ids(&actions), vec![HARDEN_ACTION_ID, REMOVE_ACTION_ID]);
        assert!(actions.iter().all(|a| a["mutates"] == true));
        assert!(actions[1]["confirm"]
            .as_str()
            .unwrap()
            .contains("Remove-NetFirewallRule"));
        assert!(clean.allow_action().is_none());
        assert!(clean.alerts().is_empty());

        // Missing.
        let missing = view(Outcome::Checked(evaluate(&snap(vec![]), &expect())));
        let b = missing.banners();
        assert_eq!(ids(&b), vec![RULE_BANNER]);
        assert_eq!(b[0]["tone"], "warn");
        assert_eq!(b[0]["title"], "Windows Firewall has no rule for the relay");
        assert_eq!(
            ids(b[0]["actions"].as_array().unwrap()),
            vec![ALLOW_ACTION_ID, HARDEN_ACTION_ID],
            "the plain allow first, the default"
        );
        let a = &b[0]["actions"][0];
        assert_eq!(a["id"], ALLOW_ACTION_ID);
        assert_eq!(a["label"], "Allow the relay through Windows Firewall");
        assert_eq!(a["mutates"], true);
        let confirm = a["confirm"].as_str().unwrap();
        let cmd = missing.allow().unwrap().unwrap();
        assert!(
            confirm.ends_with(&cmd.shown()),
            "the confirm shows the exact elevated command"
        );
        assert!(confirm.contains("on TCP ports 14000-14399"));
        assert_eq!(a["params"]["fingerprint"], cmd.fingerprint());
        assert!(missing.section_actions().is_empty(), "nothing to remove");

        // Mismatched path: replaced, keeping the old rule's explicit ports.
        let mut old = allow_rule();
        old.program = r"C:\Old\chappa-stack.exe".into();
        old.local_ports = vec!["15000-15099".into()];
        let mismatched = view(Outcome::Checked(evaluate(&snap(vec![old]), &expect())));
        let b = mismatched.banners();
        assert_eq!(
            b[0]["title"],
            "The relay's Windows Firewall rule does not match"
        );
        assert!(has_line(&b[0], r"C:\Old\chappa-stack.exe"));
        let confirm = b[0]["actions"][0]["confirm"].as_str().unwrap();
        assert!(
            confirm.contains("-LocalPort '14000-14399','15000-15099'"),
            "{confirm}"
        );
        assert_eq!(mismatched.section_actions().len(), 1);

        // Block rules: named, the same action removes them.
        let blocked = view(Outcome::Checked(evaluate(
            &parse_snapshot(SAMPLE).unwrap(),
            &expect(),
        )));
        let b = blocked.banners();
        assert_eq!(
            b[0]["title"],
            "Windows Firewall is dropping container traffic to the relay"
        );
        assert_eq!(b[0]["tone"], "bad");
        assert!(has_line(&b[0], "dropping every connection from containers"));
        let alerts = blocked.alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0]["severity"], "bad");
        assert_eq!(blocked.row()["badges"][0]["tone"], "bad");
        assert!(has_line(&b[0], "chappa-stack (TCP), chappa-stack (UDP)"));
        let confirm = b[0]["actions"][0]["confirm"].as_str().unwrap();
        assert!(confirm.contains("removes the 2 block rules"));
        assert!(blocked.row()["value"]
            .as_str()
            .unwrap()
            .contains("dropped by"));

        // Gateway outside the range, forwarding on: their own banners, no fix.
        let mut e = expect();
        e.gateways = vec!["10.0.75.1".parse().unwrap()];
        let mut s = snap(vec![allow_rule()]);
        s.interfaces[0].forwarding = true;
        let mut v = view(Outcome::Checked(evaluate(&s, &e)));
        v.expect = e;
        let b = v.banners();
        assert_eq!(ids(&b), vec![RANGE_BANNER, FORWARDING_BANNER]);
        assert!(b[0]["title"].as_str().unwrap().contains("10.0.75.1"));
        assert_eq!(b[1]["lines"][1], "vEthernet (nat) (IPv4): forwarding");
        assert!(b.iter().all(|x| x.get("actions").is_none()));
        assert_eq!(v.row()["badges"][0]["tone"], "warn");

        // A check that could not run: one banner, no actions.
        let failed = view(Outcome::Failed(
            "PowerShell could not start (not found)".into(),
        ));
        let b = failed.banners();
        assert_eq!(ids(&b), vec![CHECK_BANNER]);
        assert_eq!(b[0]["lines"][0], "PowerShell could not start (not found)");
        assert!(failed.section_actions().is_empty() && failed.allow().is_none());
        assert!(failed.row()["value"]
            .as_str()
            .unwrap()
            .starts_with("not checked"));

        // Not checked yet: nothing but the row.
        let unchecked = view(Outcome::Unchecked);
        assert!(unchecked.banners().is_empty());
        assert_eq!(unchecked.row()["value"], "checking…");

        // While an elevated command runs, the action is drawn disabled, and
        // a failed run is said in the banner.
        let mut busy = view(Outcome::Checked(evaluate(&snap(vec![]), &expect())));
        busy.elevated_running = true;
        busy.last_failure = Some(
            "The last firewall change did not run: the elevation prompt was cancelled.".into(),
        );
        let b = busy.banners();
        assert_eq!(b[0]["actions"][0]["disabled"], BUSY);
        assert!(has_line(&b[0], "cancelled"));
    }

    #[test]
    fn a_program_path_that_cannot_be_quoted_is_said_not_offered() {
        let mut e = expect();
        e.program = r#"C:\a"b\chappa-stack.exe"#.into();
        let mut v = view(Outcome::Checked(evaluate(&snap(vec![]), &e)));
        v.expect = e;
        let b = v.banners();
        assert!(b[0].get("actions").is_none());
        assert!(has_line(&b[0], "cannot be added from here"));
        assert!(v.offered(ALLOW_ACTION_ID).is_none());
    }

    /// A shell that answers the check with `check`, answers any other
    /// command like a cancelled elevation prompt, and records every call.
    struct Shell {
        check: Mutex<Result<CliOutput, String>>,
        calls: Mutex<Vec<Invocation>>,
    }

    impl Shell {
        fn answering(stdout: &str) -> Arc<Shell> {
            Arc::new(Shell {
                check: Mutex::new(Ok(CliOutput {
                    status: Some(0),
                    stdout: stdout.into(),
                    stderr: String::new(),
                })),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Invocation> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl EngineCli for Shell {
        fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
            self.calls.lock().unwrap().push(inv.clone());
            if inv.args.iter().any(|a| a == CHECK_SCRIPT) {
                return match &*self.check.lock().unwrap() {
                    Ok(o) => Ok(o.clone()),
                    Err(e) if e == "timeout" => Err(CliError::Timeout {
                        command: "powershell.exe".into(),
                        seconds: CHECK_TIMEOUT_SECS,
                    }),
                    Err(e) => Err(CliError::Spawn {
                        program: "powershell.exe".into(),
                        message: e.clone(),
                    }),
                };
            }
            Ok(CliOutput {
                status: Some(1),
                stdout: String::new(),
                stderr: "Start-Process : This command cannot be run due to the error: The operation was canceled by the user.".into(),
            })
        }
    }

    fn settle(fw: &FirewallCheck) -> FirewallView {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let v = fw.view().unwrap();
            if !v.checking && !v.elevated_running && v.outcome != Outcome::Unchecked {
                return v;
            }
            assert!(Instant::now() < deadline, "the check never finished");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn ports() -> Vec<PortRange> {
        vec![PortRange {
            lo: 14000,
            hi: 14399,
        }]
    }

    fn gw() -> Vec<IpAddr> {
        vec!["172.29.80.1".parse().unwrap()]
    }

    #[test]
    fn the_check_runs_on_windows_only_when_the_relay_changes_or_binds() {
        let shell = Shell::answering(r#"{"rules":[],"interfaces":[]}"#);
        let unix = FirewallCheck::new(HostOs::Unix, shell.clone(), Some(PROGRAM.into()));
        unix.want(gw(), ports(), true);
        assert!(unix.view().is_none());
        assert!(shell.calls().is_empty());

        let fw = FirewallCheck::new(HostOs::Windows, shell.clone(), Some(PROGRAM.into()));
        fw.want(vec![], ports(), true);
        assert!(fw.view().is_none(), "no gateway, nothing to check");
        fw.want(gw(), ports(), false);
        let v = settle(&fw);
        assert_eq!(shell.calls().len(), 1);
        let inv = &shell.calls()[0];
        assert_eq!(inv.program, "powershell.exe");
        assert_eq!(
            inv.args[..3],
            ["-NoProfile", "-NonInteractive", "-Command"].map(String::from)
        );
        assert_eq!(inv.env[PROGRAM_ENV], PROGRAM);
        assert_eq!(inv.env[RULE_ENV], RULE_NAME);
        assert_eq!(inv.timeout_secs, CHECK_TIMEOUT_SECS);
        assert!(
            !inv.args.iter().any(|a| a.contains("RunAs")),
            "the check never elevates"
        );
        assert!(!CHECK_SCRIPT.contains(PROGRAM), "nothing is spliced in");
        assert!(matches!(v.outcome, Outcome::Checked(ref r) if r.allow == AllowState::Missing));

        // The same relay again: no new check. A bind or a moved gateway: one.
        fw.want(gw(), ports(), false);
        assert_eq!(shell.calls().len(), 1);
        fw.want(gw(), ports(), true);
        settle(&fw);
        assert_eq!(shell.calls().len(), 2);
        fw.want(vec!["172.24.0.1".parse().unwrap()], ports(), false);
        settle(&fw);
        assert_eq!(shell.calls().len(), 3);
    }

    #[test]
    fn a_check_that_cannot_run_says_why() {
        let shell = Shell::answering("");
        *shell.check.lock().unwrap() = Err("program not found".into());
        let fw = FirewallCheck::new(HostOs::Windows, shell.clone(), Some(PROGRAM.into()));
        fw.want(gw(), ports(), true);
        assert_eq!(
            settle(&fw).outcome,
            Outcome::Failed("PowerShell could not start (program not found)".into())
        );

        *shell.check.lock().unwrap() = Err("timeout".into());
        fw.want(gw(), ports(), true);
        assert_eq!(
            settle(&fw).outcome,
            Outcome::Failed(format!(
                "PowerShell did not answer within {CHECK_TIMEOUT_SECS} s"
            ))
        );

        *shell.check.lock().unwrap() = Ok(CliOutput {
            status: Some(0),
            stdout: "not json".into(),
            stderr: String::new(),
        });
        fw.want(gw(), ports(), true);
        assert!(
            matches!(settle(&fw).outcome, Outcome::Failed(ref w) if w.contains("not the expected JSON"))
        );

        let nameless = FirewallCheck::new(HostOs::Windows, shell, None);
        nameless.want(gw(), ports(), true);
        assert!(
            matches!(nameless.view().unwrap().outcome, Outcome::Failed(ref w) if w.contains("program path"))
        );
    }

    #[test]
    fn the_elevated_command_runs_only_as_confirmed_and_the_check_follows() {
        let shell = Shell::answering(r#"{"rules":[],"interfaces":[]}"#);
        let fw = FirewallCheck::new(HostOs::Windows, shell.clone(), Some(PROGRAM.into()));
        assert!(
            fw.run(ALLOW_ACTION_ID, None).is_err(),
            "nothing checked, nothing offered"
        );
        fw.want(gw(), ports(), true);
        let v = settle(&fw);
        let cmd = v.allow().unwrap().unwrap();

        let (status, why) = fw.run(ALLOW_ACTION_ID, Some("stale")).unwrap_err();
        assert_eq!(status, 409);
        assert!(
            why.contains(&cmd.shown()),
            "a stale confirm hears what would run now"
        );
        assert!(
            fw.run(REMOVE_ACTION_ID, Some("x")).is_err(),
            "no rule, nothing to remove"
        );
        assert_eq!(shell.calls().len(), 1, "nothing ran");

        let msg = fw.run(ALLOW_ACTION_ID, Some(&cmd.fingerprint())).unwrap();
        assert!(msg.contains("administrator permission"));
        let v = settle(&fw);
        let calls = shell.calls();
        assert_eq!(calls.len(), 3, "the elevated command, then the check again");
        assert_eq!(calls[1].program, "powershell.exe");
        assert_eq!(calls[1].args, cmd.argv[1..]);
        assert_eq!(calls[1].timeout_secs, ELEVATED_TIMEOUT_SECS);
        assert!(calls[2].args.iter().any(|a| a == CHECK_SCRIPT));
        // The fake answered like a cancelled prompt.
        assert_eq!(
            v.last_failure.as_deref(),
            Some("The last firewall change did not run: the elevation prompt was cancelled.")
        );
    }

    /// An enabled inbound Block rule for any program, on TCP 14050.
    fn port_block() -> FwRule {
        FwRule {
            name: "{00000000-0000-0000-0000-000000000009}".into(),
            display_name: "Block test port".into(),
            enabled: true,
            inbound: true,
            action: "Block".into(),
            profile: "Any".into(),
            program: "Any".into(),
            protocol: "TCP".into(),
            local_ports: vec!["14050".into()],
            remote_addresses: vec!["Any".into()],
            interface_type: "Any".into(),
            group: String::new(),
        }
    }

    #[test]
    fn port_block_rules_count_only_when_they_meet_the_relay_traffic() {
        let found = |f: &dyn Fn(&mut FwRule)| {
            let mut r = port_block();
            f(&mut r);
            evaluate(&snap(vec![allow_rule(), r]), &expect())
        };
        let r = found(&|_| {});
        assert!(r.blocks.is_empty(), "not the relay's own to remove");
        assert_eq!(
            r.foreign_blocks,
            vec![ForeignBlock {
                label: "Block test port (TCP)".into(),
                remove_line: Some(
                    "Remove-NetFirewallRule -Name '{00000000-0000-0000-0000-000000000009}'".into()
                ),
            }]
        );
        assert!(
            r.is_dropping() && !r.wants_allow(),
            "the allow cannot fix it"
        );
        let drops = |f: &dyn Fn(&mut FwRule)| found(f).is_dropping();
        // Ranges and Any ports, a LocalSubnet or a container-range source,
        // Wired interfaces and protocol Any all reach the relay.
        assert!(drops(&|r| r.local_ports = vec!["14000-14010".into()]));
        assert!(drops(&|r| r.local_ports = vec!["Any".into()]));
        assert!(drops(&|r| r.remote_addresses = vec!["LocalSubnet".into()]));
        assert!(drops(&|r| r.remote_addresses = vec!["172.29.0.0/16".into()]));
        assert!(drops(&|r| r.interface_type = "Wired".into()));
        assert!(drops(&|r| r.protocol = "Any".into()));
        // Other ports, UDP, other sources, other interface types, other
        // programs, disabled or outbound rules do not.
        assert!(!drops(&|r| r.local_ports = vec!["15000".into()]));
        assert!(!drops(&|r| r.local_ports = vec!["RPC".into()]));
        assert!(!drops(&|r| r.protocol = "UDP".into()));
        assert!(!drops(&|r| r.remote_addresses = vec!["10.0.0.0/8".into()]));
        assert!(!drops(
            &|r| r.interface_type = "Wireless, RemoteAccess".into()
        ));
        assert!(!drops(&|r| r.program = r"C:\Other\tool.exe".into()));
        assert!(!drops(&|r| r.enabled = false));
        assert!(!drops(&|r| r.inbound = false));
        // A name that cannot be quoted gets no line to copy.
        let r = found(&|r| r.name = "odd*name".into());
        assert_eq!(r.foreign_blocks[0].remove_line, None);
        // A rule of our group is never counted as dropping.
        assert!(!drops(&|r| r.group = GROUP.into()));
    }

    #[test]
    fn our_rules_are_told_by_group_and_a_lookalike_is_not_ours() {
        let e = expect();
        let mut grouped = allow_rule();
        assert!(is_ours(&grouped, &e));
        // The allow rule from before groups: exact name, no group, our program.
        grouped.group = String::new();
        assert!(is_ours(&grouped, &e));
        assert_eq!(
            evaluate(&snap(vec![grouped.clone()]), &e).allow,
            AllowState::Present
        );
        // The same name for another program, without the group: not ours.
        let mut lookalike = grouped.clone();
        lookalike.program = r"C:\Other\tool.exe".into();
        assert!(!is_ours(&lookalike, &e));
        let r = evaluate(&snap(vec![lookalike]), &e);
        assert_eq!(r.allow, AllowState::Missing);
        assert!(!r.has_own_rules(), "nothing of ours to remove");
        // A prefix-sharing name is never ours, whatever its program.
        let mut prefixed = grouped.clone();
        prefixed.display_name = "Chappa'Ai relay (mine)".into();
        assert!(!is_ours(&prefixed, &e));
        // A block rule named like ours but outside the group is not hardening.
        let mut fake = own_block("Wireless");
        fake.group = String::new();
        assert!(evaluate(&snap(vec![allow_rule(), fake]), &e)
            .hardened
            .is_empty());
        assert!(check_rule_name(GROUP).is_ok());
    }

    fn own_block(t: &str) -> FwRule {
        FwRule {
            name: format!("{{0000000{}}}", t.len()),
            display_name: block_rule_name(t),
            enabled: true,
            inbound: true,
            action: "Block".into(),
            profile: "Any".into(),
            program: PROGRAM.into(),
            protocol: "Any".into(),
            local_ports: vec!["Any".into()],
            remote_addresses: vec!["Any".into()],
            interface_type: t.into(),
            group: GROUP.into(),
        }
    }

    #[test]
    fn the_optional_block_rules_are_recognised_and_never_count_as_dropping() {
        let s = snap(vec![
            allow_rule(),
            own_block("Wireless"),
            own_block("RemoteAccess"),
        ]);
        let r = evaluate(&s, &expect());
        assert!(r.blocks.is_empty(), "{r:?}");
        assert_eq!(r.hardened, vec!["Wireless", "RemoteAccess"]);
        assert!(r.is_hardened() && r.is_clean() && r.has_own_rules());
        let v = view(Outcome::Checked(r));
        assert!(v.banners().is_empty());
        assert_eq!(ids(&v.section_actions()), vec![REMOVE_ACTION_ID]);
        assert!(v.row()["value"]
            .as_str()
            .unwrap()
            .ends_with("blocked on Wireless and RemoteAccess networks"));
        // One of them alone: hardening is offered again.
        let r = evaluate(&snap(vec![allow_rule(), own_block("Wireless")]), &expect());
        assert!(!r.is_hardened());
        let v = view(Outcome::Checked(r));
        assert_eq!(
            ids(&v.section_actions()),
            vec![HARDEN_ACTION_ID, REMOVE_ACTION_ID]
        );
        // Only the block rules left: there is still something to remove.
        let r = evaluate(&snap(vec![own_block("Wireless")]), &expect());
        assert!(!r.named_rule && r.has_own_rules());
    }

    #[test]
    fn the_hardened_allow_adds_type_blocks_in_the_same_step_and_never_wired() {
        let ports = [PortRange {
            lo: 14000,
            hi: 14399,
        }];
        let cmd = allow_command(PROGRAM, &ports, true).unwrap();
        assert_eq!(cmd.argv[4].matches("-Verb RunAs").count(), 1, "one prompt");
        assert_eq!(carried(&cmd), cmd.statements.join("; "));
        let script = cmd.shown();
        assert!(script.contains("Get-NetFirewallRule -Group 'Chappa''Ai relay' -ErrorAction SilentlyContinue | Remove-NetFirewallRule"));
        assert!(script.contains(r"New-NetFirewallRule -DisplayName 'Chappa''Ai relay (block on Wireless)' -Group 'Chappa''Ai relay' -Description 'Keeps the stack relay off networks other than the container network' -Direction Inbound -Action Block -Program 'C:\Apps\Chappa''Ai\chappa-stack.exe' -InterfaceType Wireless -Profile Any | Out-Null"));
        assert!(script.contains("-InterfaceType RemoteAccess -Profile Any"));
        assert!(
            !script.contains("Wired"),
            "a block on Wired would drop the relay itself"
        );
        assert_eq!(script.matches("New-NetFirewallRule").count(), 3);
        assert_eq!(
            script.matches("-Group 'Chappa''Ai relay'").count(),
            4,
            "every rule made, and the group removal"
        );
        // The allow rule comes before the blocks.
        assert!(script.find("-Action Allow").unwrap() < script.find("-Action Block").unwrap());
        let plain = allow_command(PROGRAM, &ports, false).unwrap();
        assert!(plain
            .shown()
            .contains("Where-Object { $_.DisplayName -eq 'Chappa''Ai relay' }"));
        assert_ne!(plain.fingerprint(), cmd.fingerprint());

        // Neither variant ever removes a rule by name, by a name prefix, or
        // one that is not the relay program's or the group's: every removal
        // is scoped by the group, by the relay's program, or both.
        for c in [&cmd, &plain, &remove_command(PROGRAM)] {
            for st in c
                .statements
                .iter()
                .filter(|st| st.contains("Remove-NetFirewallRule"))
            {
                assert!(!st.contains("-Name ") && !st.contains("relay*"), "{st}");
                assert!(
                    st.starts_with("Get-NetFirewallRule -Group 'Chappa''Ai relay'")
                        || st.contains(r"-Program 'C:\Apps\Chappa''Ai\chappa-stack.exe'")
                        || st.contains(r".Program -eq 'C:\Apps\Chappa''Ai\chappa-stack.exe'"),
                    "{st}"
                );
            }
        }

        // Rule names are refused when they could not be quoted.
        for bad in [
            "",
            " padded",
            "a*b",
            "a?b",
            "a[1]",
            "a\"b",
            "a`b",
            "a\nb",
            "a  b",
            "a\u{2018}b",
        ] {
            assert!(check_rule_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_rule_name(r"TCP Query User{0000}C:\apps\chappa'ai\chappa-stack.exe").is_ok());
    }

    #[test]
    fn a_dropping_port_rule_of_any_program_is_bad_and_left_to_the_user() {
        let v = view(Outcome::Checked(evaluate(
            &snap(vec![allow_rule(), port_block()]),
            &expect(),
        )));
        let b = v.banners();
        assert_eq!(b[0]["tone"], "bad");
        assert_eq!(
            b[0]["title"],
            "Windows Firewall is dropping container traffic to the relay"
        );
        assert!(has_line(
            &b[0],
            "not the relay's to change: Block test port (TCP)"
        ));
        assert!(has_line(
            &b[0],
            "Remove-NetFirewallRule -Name '{00000000-0000-0000-0000-000000000009}'"
        ));
        assert!(
            b[0].get("actions").is_none(),
            "the allow cannot fix it, so it is not offered"
        );
        assert_eq!(v.alerts()[0]["severity"], "bad");
        // With our rule missing too, the allow is offered and its command
        // still names no foreign rule.
        let v = view(Outcome::Checked(evaluate(
            &snap(vec![port_block()]),
            &expect(),
        )));
        let b = v.banners();
        let confirm = b[0]["actions"][0]["confirm"].as_str().unwrap();
        assert!(!confirm.contains("{00000000-0000-0000-0000-000000000009}"));
        assert!(confirm.contains("Rules of other programs, or of any program, are never changed"));
        let hardened = b[0]["actions"][1]["confirm"].as_str().unwrap();
        assert!(hardened.contains("Wired interfaces are left alone"));
    }

    #[test]
    fn reading_the_view_checks_again_after_the_recheck_time() {
        let shell = Shell::answering(r#"{"rules":[],"interfaces":[]}"#);
        let fw = FirewallCheck::new(HostOs::Windows, shell.clone(), Some(PROGRAM.into()))
            .with_recheck(Duration::from_millis(50));
        fw.want(gw(), ports(), false);
        settle(&fw);
        fw.want(gw(), ports(), false);
        assert_eq!(shell.calls().len(), 1, "not yet");
        thread::sleep(Duration::from_millis(80));
        fw.want(gw(), ports(), false);
        settle(&fw);
        assert_eq!(shell.calls().len(), 2);
    }

    #[test]
    fn the_view_decorates_an_environment_payload() {
        let v = view(Outcome::Checked(evaluate(
            &parse_snapshot(SAMPLE).unwrap(),
            &expect(),
        )));
        let mut payload = json!({
            "banners": [],
            "alerts": [],
            "facts": [{ "id": "relay", "title": "Relay", "rows": [] }],
        });
        decorate(&mut payload, &v);
        assert_eq!(payload["banners"][0]["id"], RULE_BANNER);
        assert_eq!(payload["alerts"][0]["severity"], "bad");
        assert_eq!(payload["facts"][0]["rows"][0]["label"], "Windows Firewall");
        assert_eq!(payload["facts"][0]["actions"][0]["id"], REMOVE_ACTION_ID);
    }
}
