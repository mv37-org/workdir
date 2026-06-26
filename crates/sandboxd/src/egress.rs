//! Host-enforced sandbox egress policy.
//!
//! Firecracker guests can run arbitrary root-owned code, so the enforcement
//! point is the host tap/bridge, not guest iptables. This module renders and
//! applies per-sandbox nftables chains keyed by guest IP and runs a small DNS
//! proxy for domain-based allow/deny rules.

use crate::model::{
    domain_matches, parse_ipv4_cidr, EgressMode, NetworkPolicy, NetworkProtocol, NetworkRule,
    NetworkRuleKind,
};
use anyhow::{anyhow, bail, Context, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;

pub const DNS_PROXY_IP: &str = "10.200.0.1";
const DNS_PROXY_BIND: &str = "10.200.0.1:53";
const DNS_UPSTREAM: &str = "1.1.1.1:53";
const NFT_TABLE: &str = "sandboxd";
const NFT_POLICY_CHAIN: &str = "sandbox_policy";
const RULE_COMMENT_PREFIX: &str = "workdir:";

static DNS_READY: AtomicBool = AtomicBool::new(false);
static DNS_STARTED: AtomicBool = AtomicBool::new(false);
static DOMAIN_REGISTRY: OnceLock<Mutex<Vec<DomainRegistration>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DomainMode {
    Allow,
    Deny,
}

#[derive(Debug, Clone)]
struct DomainRegistration {
    sandbox_id: String,
    source_ip: Ipv4Addr,
    pattern: String,
    set_name: String,
    mode: DomainMode,
}

fn registry() -> &'static Mutex<Vec<DomainRegistration>> {
    DOMAIN_REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn dns_proxy_ready() -> bool {
    DNS_READY.load(Ordering::SeqCst)
}

pub fn start_dns_proxy() -> Result<()> {
    if DNS_READY.load(Ordering::SeqCst) {
        return Ok(());
    }
    if DNS_STARTED.swap(true, Ordering::SeqCst) {
        if DNS_READY.load(Ordering::SeqCst) {
            return Ok(());
        }
        bail!("egress DNS proxy was already attempted but is not ready");
    }
    let sock = std::net::UdpSocket::bind(DNS_PROXY_BIND)
        .with_context(|| format!("bind sandbox DNS proxy on {DNS_PROXY_BIND}"))?;
    sock.set_nonblocking(true)?;
    let sock = UdpSocket::from_std(sock)?;
    DNS_READY.store(true, Ordering::SeqCst);
    tokio::spawn(async move {
        if let Err(e) = serve_dns(sock).await {
            DNS_READY.store(false, Ordering::SeqCst);
            tracing::error!(error = %e, "sandbox egress DNS proxy stopped");
        }
    });
    tracing::info!(bind = DNS_PROXY_BIND, upstream = DNS_UPSTREAM, "sandbox egress DNS proxy listening");
    Ok(())
}

async fn serve_dns(sock: UdpSocket) -> Result<()> {
    let mut buf = vec![0u8; 1500];
    loop {
        let (n, peer) = sock.recv_from(&mut buf).await?;
        let packet = buf[..n].to_vec();
        let response = handle_dns_packet(peer, &packet)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, peer = %peer, "DNS policy handling failed");
                dns_error_response(&packet, 2).unwrap_or_default()
            });
        if !response.is_empty() {
            let _ = sock.send_to(&response, peer).await;
        }
    }
}

async fn handle_dns_packet(peer: SocketAddr, packet: &[u8]) -> Result<Vec<u8>> {
    let question = parse_question(packet).ok_or_else(|| anyhow!("invalid DNS question"))?;
    let source_ip = match peer.ip() {
        IpAddr::V4(ip) => ip,
        IpAddr::V6(_) => return dns_error_response(packet, 5),
    };
    let regs = {
        let regs = registry().lock().unwrap();
        regs.iter()
            .filter(|r| r.source_ip == source_ip && domain_matches(&r.pattern, &question.name))
            .cloned()
            .collect::<Vec<_>>()
    };

    if regs.is_empty() {
        if has_allowlist_policy(source_ip) {
            return dns_error_response(packet, 3);
        }
        return forward_dns(packet).await;
    }

    let upstream = forward_dns(packet).await?;
    let addrs = parse_a_records(&upstream);
    for reg in &regs {
        for addr in &addrs {
            let _ = add_nft_domain_element(&reg.set_name, *addr, question.ttl_hint()).await;
        }
    }
    if regs.iter().any(|r| r.mode == DomainMode::Deny) {
        dns_error_response(packet, 3)
    } else {
        Ok(upstream)
    }
}

fn has_allowlist_policy(source_ip: Ipv4Addr) -> bool {
    registry()
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.source_ip == source_ip && r.mode == DomainMode::Allow)
}

async fn forward_dns(packet: &[u8]) -> Result<Vec<u8>> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.send_to(packet, DNS_UPSTREAM).await?;
    let mut buf = vec![0u8; 1500];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .context("DNS upstream timeout")??;
    Ok(buf[..n].to_vec())
}

#[derive(Debug)]
struct DnsQuestion {
    name: String,
    end: usize,
}

impl DnsQuestion {
    fn ttl_hint(&self) -> u32 {
        300
    }
}

fn parse_question(packet: &[u8]) -> Option<DnsQuestion> {
    if packet.len() < 12 || u16::from_be_bytes([packet[4], packet[5]]) == 0 {
        return None;
    }
    let mut pos = 12usize;
    let mut labels = Vec::new();
    loop {
        let len = *packet.get(pos)? as usize;
        pos += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 || len > 63 || pos + len > packet.len() {
            return None;
        }
        labels.push(std::str::from_utf8(&packet[pos..pos + len]).ok()?.to_ascii_lowercase());
        pos += len;
    }
    if pos + 4 > packet.len() {
        return None;
    }
    Some(DnsQuestion {
        name: labels.join("."),
        end: pos + 4,
    })
}

fn dns_error_response(packet: &[u8], rcode: u8) -> Result<Vec<u8>> {
    let question = parse_question(packet).ok_or_else(|| anyhow!("invalid DNS question"))?;
    let mut out = Vec::with_capacity(question.end);
    out.extend_from_slice(&packet[..2]);
    out.extend_from_slice(&[0x81, 0x80 | (rcode & 0x0f)]);
    out.extend_from_slice(&[0x00, 0x01]); // one question
    out.extend_from_slice(&[0x00, 0x00]); // no answers
    out.extend_from_slice(&[0x00, 0x00]); // no authority
    out.extend_from_slice(&[0x00, 0x00]); // no additional
    out.extend_from_slice(&packet[12..question.end]);
    Ok(out)
}

fn skip_dns_name(packet: &[u8], pos: &mut usize) -> Option<()> {
    loop {
        let len = *packet.get(*pos)?;
        *pos += 1;
        if len == 0 {
            return Some(());
        }
        if len & 0xc0 == 0xc0 {
            *pos += 1;
            return (*pos <= packet.len()).then_some(());
        }
        let n = len as usize;
        if n > 63 || *pos + n > packet.len() {
            return None;
        }
        *pos += n;
    }
}

fn parse_a_records(packet: &[u8]) -> Vec<Ipv4Addr> {
    let Some(question) = parse_question(packet) else {
        return Vec::new();
    };
    if packet.len() < 12 {
        return Vec::new();
    }
    let answer_count = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    let mut pos = question.end;
    let mut out = Vec::new();
    for _ in 0..answer_count {
        if skip_dns_name(packet, &mut pos).is_none() || pos + 10 > packet.len() {
            break;
        }
        let rr_type = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
        let rr_class = u16::from_be_bytes([packet[pos + 2], packet[pos + 3]]);
        let rdlen = u16::from_be_bytes([packet[pos + 8], packet[pos + 9]]) as usize;
        pos += 10;
        if pos + rdlen > packet.len() {
            break;
        }
        if rr_type == 1 && rr_class == 1 && rdlen == 4 {
            out.push(Ipv4Addr::new(
                packet[pos],
                packet[pos + 1],
                packet[pos + 2],
                packet[pos + 3],
            ));
        }
        pos += rdlen;
    }
    out
}

async fn add_nft_domain_element(set_name: &str, ip: Ipv4Addr, ttl_seconds: u32) -> Result<()> {
    let ttl = ttl_seconds.clamp(30, 3600);
    run_nft_args(&[
        "add",
        "element",
        "inet",
        NFT_TABLE,
        set_name,
        "{",
        &format!("{ip} timeout {ttl}s"),
        "}",
    ])
    .await
}

pub async fn apply_policy(sandbox_id: &str, guest_ip: &str, policy: &NetworkPolicy) -> Result<()> {
    policy.validate().map_err(|e| anyhow!(e))?;
    clear_policy(sandbox_id).await.ok();
    unregister_domain_rules(sandbox_id);
    if matches!(policy.egress, EgressMode::Default) {
        return Ok(());
    }
    if policy.uses_domain_rules() && !dns_proxy_ready() {
        bail!("network policy uses domain rules, but sandbox DNS proxy is not ready");
    }
    let source_ip: Ipv4Addr = guest_ip
        .parse()
        .with_context(|| format!("parse guest IP {guest_ip}"))?;
    let script = render_policy_script(sandbox_id, source_ip, policy)?;
    run_nft_script(&script).await?;
    register_domain_rules(sandbox_id, source_ip, policy);
    Ok(())
}

pub async fn clear_policy(sandbox_id: &str) -> Result<()> {
    unregister_domain_rules(sandbox_id);
    let safe = nft_name(sandbox_id);
    delete_policy_jumps(sandbox_id).await.ok();
    let sets = list_domain_sets(&safe).await.unwrap_or_default();
    let _ = run_nft_args(&["flush", "chain", "inet", NFT_TABLE, &chain_name(&safe)]).await;
    let _ = run_nft_args(&["delete", "chain", "inet", NFT_TABLE, &chain_name(&safe)]).await;
    for set in sets {
        let _ = run_nft_args(&["delete", "set", "inet", NFT_TABLE, &set]).await;
    }
    Ok(())
}

pub fn render_policy_script(
    sandbox_id: &str,
    source_ip: Ipv4Addr,
    policy: &NetworkPolicy,
) -> Result<String> {
    let safe = nft_name(sandbox_id);
    let chain = chain_name(&safe);
    let mut lines = Vec::new();
    lines.push(format!("add chain inet {NFT_TABLE} {chain}"));
    for (idx, rule) in policy.rules_for_mode().iter().enumerate() {
        if rule.is_domain() {
            lines.push(format!(
                "add set inet {NFT_TABLE} {} {{ type ipv4_addr; flags timeout; }}",
                domain_set_name(&safe, idx)
            ));
        }
    }
    lines.push(format!(
        "add rule inet {NFT_TABLE} {NFT_POLICY_CHAIN} ip saddr {source_ip} jump {chain} comment \"{RULE_COMMENT_PREFIX}{sandbox_id}\""
    ));

    match policy.egress {
        EgressMode::Default => {}
        EgressMode::None => {
            lines.push(format!("add rule inet {NFT_TABLE} {chain} drop"));
        }
        EgressMode::Allowlist => {
            add_dns_control_rules(&mut lines, &chain, policy);
            for (idx, rule) in policy.allow.iter().enumerate() {
                add_rule_lines(&mut lines, &chain, idx, rule, "accept")?;
            }
            lines.push(format!("add rule inet {NFT_TABLE} {chain} drop"));
        }
        EgressMode::Denylist => {
            add_dns_control_rules(&mut lines, &chain, policy);
            for (idx, rule) in policy.deny.iter().enumerate() {
                add_rule_lines(&mut lines, &chain, idx, rule, "drop")?;
            }
            lines.push(format!("add rule inet {NFT_TABLE} {chain} accept"));
        }
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn add_dns_control_rules(lines: &mut Vec<String>, chain: &str, policy: &NetworkPolicy) {
    if !policy.uses_domain_rules() {
        return;
    }
    lines.push(format!(
        "add rule inet {NFT_TABLE} {chain} ip daddr {DNS_PROXY_IP} udp dport 53 accept"
    ));
    lines.push(format!(
        "add rule inet {NFT_TABLE} {chain} ip daddr {DNS_PROXY_IP} tcp dport 53 accept"
    ));
    lines.push(format!("add rule inet {NFT_TABLE} {chain} udp dport 53 drop"));
    lines.push(format!("add rule inet {NFT_TABLE} {chain} tcp dport 53 drop"));
}

fn add_rule_lines(
    lines: &mut Vec<String>,
    chain: &str,
    idx: usize,
    rule: &NetworkRule,
    verdict: &str,
) -> Result<()> {
    let safe = chain.trim_start_matches("wd_");
    let dest = match rule.kind {
        NetworkRuleKind::Cidr => {
            parse_ipv4_cidr(&rule.value).ok_or_else(|| anyhow!("invalid CIDR {}", rule.value))?;
            rule.value.clone()
        }
        NetworkRuleKind::Domain => format!("@{}", domain_set_name(safe, idx)),
    };
    let dest_expr = format!("ip daddr {dest}");
    match (rule.protocol, rule.ports.is_empty()) {
        (None, true) => {
            lines.push(format!(
                "add rule inet {NFT_TABLE} {chain} {dest_expr} {verdict}"
            ));
        }
        (Some(proto), true) => {
            lines.push(format!(
                "add rule inet {NFT_TABLE} {chain} {dest_expr} meta l4proto {} {verdict}",
                protocol_name(proto)
            ));
        }
        (Some(proto), false) => {
            lines.push(format!(
                "add rule inet {NFT_TABLE} {chain} {dest_expr} {} dport {} {verdict}",
                protocol_name(proto),
                ports_expr(&rule.ports)
            ));
        }
        (None, false) => {
            lines.push(format!(
                "add rule inet {NFT_TABLE} {chain} {dest_expr} tcp dport {} {verdict}",
                ports_expr(&rule.ports)
            ));
            lines.push(format!(
                "add rule inet {NFT_TABLE} {chain} {dest_expr} udp dport {} {verdict}",
                ports_expr(&rule.ports)
            ));
        }
    }
    Ok(())
}

fn protocol_name(proto: NetworkProtocol) -> &'static str {
    match proto {
        NetworkProtocol::Tcp => "tcp",
        NetworkProtocol::Udp => "udp",
    }
}

fn ports_expr(ports: &[u16]) -> String {
    if ports.len() == 1 {
        ports[0].to_string()
    } else {
        format!(
            "{{ {} }}",
            ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn register_domain_rules(sandbox_id: &str, source_ip: Ipv4Addr, policy: &NetworkPolicy) {
    if !policy.uses_domain_rules() {
        return;
    }
    let safe = nft_name(sandbox_id);
    let mode = match policy.egress {
        EgressMode::Allowlist => DomainMode::Allow,
        EgressMode::Denylist => DomainMode::Deny,
        EgressMode::Default | EgressMode::None => return,
    };
    let mut regs = registry().lock().unwrap();
    regs.retain(|r| r.sandbox_id != sandbox_id);
    for (idx, rule) in policy.rules_for_mode().iter().enumerate() {
        if rule.is_domain() {
            regs.push(DomainRegistration {
                sandbox_id: sandbox_id.to_string(),
                source_ip,
                pattern: rule.value.clone(),
                set_name: domain_set_name(&safe, idx),
                mode,
            });
        }
    }
}

fn unregister_domain_rules(sandbox_id: &str) {
    registry()
        .lock()
        .unwrap()
        .retain(|r| r.sandbox_id != sandbox_id);
}

async fn run_nft_script(script: &str) -> Result<()> {
    let mut child = tokio::process::Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn nft")?;
    child
        .stdin
        .as_mut()
        .context("nft stdin")?
        .write_all(script.as_bytes())
        .await?;
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "nft apply failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn run_nft_args(args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new("nft")
        .args(args)
        .output()
        .await
        .context("spawn nft")?;
    if !out.status.success() {
        bail!(
            "nft {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

async fn delete_policy_jumps(sandbox_id: &str) -> Result<()> {
    let out = tokio::process::Command::new("nft")
        .args(["-a", "list", "chain", "inet", NFT_TABLE, NFT_POLICY_CHAIN])
        .output()
        .await
        .context("list nft policy chain")?;
    if !out.status.success() {
        return Ok(());
    }
    let needle = format!("comment \"{RULE_COMMENT_PREFIX}{sandbox_id}\"");
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if !line.contains(&needle) {
            continue;
        }
        if let Some(handle) = line.split("# handle ").nth(1).and_then(|s| s.split_whitespace().next())
        {
            let _ = run_nft_args(&[
                "delete",
                "rule",
                "inet",
                NFT_TABLE,
                NFT_POLICY_CHAIN,
                "handle",
                handle,
            ])
            .await;
        }
    }
    Ok(())
}

async fn list_domain_sets(safe: &str) -> Result<Vec<String>> {
    let out = tokio::process::Command::new("nft")
        .args(["list", "table", "inet", NFT_TABLE])
        .output()
        .await
        .context("list nft table")?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let prefix = format!("set wd_{safe}_d");
    let mut sets = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if line.starts_with(&prefix) {
            if let Some(name) = line.split_whitespace().nth(1) {
                sets.push(name.to_string());
            }
        }
    }
    Ok(sets)
}

fn nft_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn chain_name(safe: &str) -> String {
    format!("wd_{safe}")
}

fn domain_set_name(safe: &str, idx: usize) -> String {
    format!("wd_{safe}_d{idx}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NetworkRuleKind;

    #[test]
    fn renders_none_policy_as_drop_chain() {
        let policy = NetworkPolicy {
            egress: EgressMode::None,
            allow: vec![],
            deny: vec![],
        };
        let script =
            render_policy_script("sb_test", "10.200.0.2".parse().unwrap(), &policy).unwrap();
        assert!(script.contains("add chain inet sandboxd wd_sb_test"));
        assert!(script.contains("ip saddr 10.200.0.2 jump wd_sb_test"));
        assert!(script.contains("add rule inet sandboxd wd_sb_test drop"));
    }

    #[test]
    fn renders_allowlist_domain_sets_and_dns_guard() {
        let policy = NetworkPolicy {
            egress: EgressMode::Allowlist,
            allow: vec![NetworkRule {
                kind: NetworkRuleKind::Domain,
                value: "*.example.com".into(),
                protocol: Some(NetworkProtocol::Tcp),
                ports: vec![443],
            }],
            deny: vec![],
        };
        let script =
            render_policy_script("sb_test", "10.200.0.2".parse().unwrap(), &policy).unwrap();
        assert!(script.contains("add set inet sandboxd wd_sb_test_d0"));
        assert!(script.contains("ip daddr 10.200.0.1 udp dport 53 accept"));
        assert!(script.contains("ip daddr @wd_sb_test_d0 tcp dport 443 accept"));
        assert!(script.trim_end().ends_with("drop"));
    }

    #[test]
    fn parses_a_records_from_compressed_dns_response() {
        let packet = [
            0x12, 0x34, 0x81, 0x80, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x07,
            b'e', b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00, 0x00,
            0x01, 0x00, 0x01, 0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3c,
            0x00, 0x04, 93, 184, 216, 34,
        ];
        assert_eq!(parse_a_records(&packet), vec![Ipv4Addr::new(93, 184, 216, 34)]);
    }
}
