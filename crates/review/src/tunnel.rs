//! Tailscale Funnel status for review links. Funnel gives this Mac a public `https://` address that
//! forwards to the review server, so reviewers off the local network can open a link.
//!
//! FilmCraft only reads: the desktop app runs `tailscale status --json` and
//! `tailscale funnel status --json` and this module reads their output. It never signs in, turns
//! Funnel on or changes the tailnet; [`steps`] says what the person does instead. Both outputs are
//! treated as untrusted text: unknown shapes give "not set up", never an error or a panic.

use serde_json::Value;

/// The public ports Funnel allows when the tailnet does not list them.
pub const DEFAULT_FUNNEL_PORTS: [u16; 3] = [443, 8443, 10000];
/// Largest command output read.
pub const MAX_OUTPUT: usize = 1 << 20;

/// What the app got from the `tailscale` command line.
#[derive(Clone, Debug)]
pub struct ProbeOutput {
    /// The `tailscale` program used (None: not found).
    pub tool: Option<String>,
    /// `tailscale status --json`, or why it failed.
    pub status: Result<String, String>,
    /// `tailscale funnel status --json`, or why it failed.
    pub serve: Result<String, String>,
}

impl ProbeOutput {
    /// No `tailscale` program on this machine.
    pub fn not_installed() -> Self {
        let e = || Err("tailscale is not installed".to_string());
        Self { tool: None, status: e(), serve: e() }
    }
}

/// This machine's Tailscale state, as far as review links care.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunnelStatus {
    pub installed: bool,
    /// `Running`, `NeedsLogin`, `Stopped`…, or `unreachable` when the daemon did not answer.
    pub state: String,
    /// The daemon's error when it did not answer.
    pub error: String,
    /// `name.tailnet.ts.net` (no trailing dot); empty when unknown.
    pub dns_name: String,
    /// The tailnet gives this machine HTTPS certificates.
    pub https_certs: bool,
    /// The tailnet lets this machine use Funnel.
    pub funnel_allowed: bool,
    /// The public ports Funnel may use here.
    pub funnel_ports: Vec<u16>,
    /// The public port that funnels to the review server, if any.
    pub serving: Option<u16>,
    /// A public port that forwards to the review server inside the tailnet only (serve, not funnel).
    pub tailnet_only: Option<u16>,
    /// Public ports already used by something else.
    pub taken: Vec<u16>,
}

impl FunnelStatus {
    pub fn running(&self) -> bool {
        self.state == "Running"
    }

    /// The first Funnel port nothing else uses.
    pub fn free_port(&self) -> Option<u16> {
        self.funnel_ports.iter().copied().find(|p| !self.taken.contains(p))
    }

    /// The public base of review links through Funnel (`https://name.ts.net[:port]`), once it serves.
    pub fn base_url(&self) -> Option<String> {
        let port = self.serving?;
        (!self.dns_name.is_empty()).then(|| base_url(&self.dns_name, port))
    }
}

/// `https://<dns name>` for port 443, `https://<dns name>:<port>` otherwise.
pub fn base_url(dns_name: &str, port: u16) -> String {
    if port == 443 { format!("https://{dns_name}") } else { format!("https://{dns_name}:{port}") }
}

/// Read the two command outputs. `local_port` is the review server's port.
pub fn read(out: &ProbeOutput, local_port: u16) -> FunnelStatus {
    let mut st = FunnelStatus { installed: out.tool.is_some(), funnel_ports: DEFAULT_FUNNEL_PORTS.to_vec(), ..Default::default() };
    if !st.installed {
        st.state = "notInstalled".into();
        return st;
    }
    match &out.status {
        Err(e) => {
            st.state = "unreachable".into();
            st.error = short(e);
            return st;
        }
        Ok(text) => read_status(&mut st, text),
    }
    if let Ok(text) = &out.serve {
        read_serve(&mut st, text, local_port);
    }
    st
}

fn short(e: &str) -> String {
    let e = e.trim();
    if e.chars().count() > 300 { format!("{}…", e.chars().take(300).collect::<String>()) } else { e.to_string() }
}

fn parse(text: &str) -> Option<Value> {
    if text.len() > MAX_OUTPUT {
        return None;
    }
    serde_json::from_str(text).ok()
}

fn read_status(st: &mut FunnelStatus, text: &str) {
    let Some(v) = parse(text) else {
        st.state = "unreachable".into();
        st.error = "tailscale status did not return JSON".into();
        return;
    };
    st.state = v.get("BackendState").and_then(Value::as_str).filter(|s| s.len() <= 40).unwrap_or("unknown").to_string();
    let me = v.get("Self");
    let dns = me.and_then(|s| s.get("DNSName")).and_then(Value::as_str).unwrap_or("").trim_end_matches('.');
    if is_dns_name(dns) {
        st.dns_name = dns.to_ascii_lowercase();
    }
    let Some(caps) = me.and_then(|s| s.get("CapMap")).and_then(Value::as_object) else { return };
    for key in caps.keys() {
        match key.as_str() {
            "https" => st.https_certs = true,
            "funnel" | "https://tailscale.com/cap/funnel" => st.funnel_allowed = true,
            k => {
                if let Some(list) = k.strip_prefix("https://tailscale.com/cap/funnel-ports?ports=") {
                    let ports: Vec<u16> = list.split(',').filter_map(|p| p.trim().parse().ok()).filter(|p| *p > 0).take(16).collect();
                    if !ports.is_empty() {
                        st.funnel_ports = ports;
                    }
                }
            }
        }
    }
}

/// A host name made of DNS labels (it goes into a link, so nothing else is accepted).
fn is_dns_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.')
            .all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

fn read_serve(st: &mut FunnelStatus, text: &str, local_port: u16) {
    let Some(v) = parse(text) else { return };
    let funnel_on = |hostport: &str| v.get("AllowFunnel").and_then(|a| a.get(hostport)).and_then(Value::as_bool).unwrap_or(false);
    let mut ours = Vec::new();
    if let Some(web) = v.get("Web").and_then(Value::as_object) {
        for (hostport, site) in web {
            let Some((host, port)) = hostport.rsplit_once(':') else { continue };
            let Ok(port) = port.parse::<u16>() else { continue };
            if !st.dns_name.is_empty() && !host.eq_ignore_ascii_case(&st.dns_name) {
                continue;
            }
            let to_us =
                site.get("Handlers").and_then(|h| h.get("/")).and_then(|h| h.get("Proxy")).and_then(Value::as_str).is_some_and(|t| proxies_to(t, local_port));
            if to_us {
                ours.push(port);
                if funnel_on(hostport) {
                    st.serving.get_or_insert(port);
                } else {
                    st.tailnet_only.get_or_insert(port);
                }
            }
        }
    }
    if let Some(tcp) = v.get("TCP").and_then(Value::as_object) {
        st.taken = tcp.keys().filter_map(|k| k.parse::<u16>().ok()).filter(|p| !ours.contains(p)).take(64).collect();
        st.taken.sort_unstable();
    }
}

/// Whether a serve target (`http://127.0.0.1:8787`, `localhost:8787`, `8787`…) is this port on
/// this machine.
pub fn proxies_to(target: &str, port: u16) -> bool {
    let t = target.trim();
    let t = t.strip_prefix("http://").unwrap_or(t);
    let t = t.split('/').next().unwrap_or("");
    let (host, p) = match t.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => ("127.0.0.1", t),
    };
    p.parse::<u16>().is_ok_and(|p| p == port) && matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "0.0.0.0")
}

/// Quote a path for a shell command shown to the person.
fn shell_quote(s: &str) -> String {
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)) { s.to_string() } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// The `tailscale` command prefix to show, with `--socket` when one is configured.
pub fn command(socket: &str) -> String {
    if socket.is_empty() { "tailscale".into() } else { format!("tailscale --socket={}", shell_quote(socket)) }
}

/// What the person still has to do, in order, for links to work through Funnel. `current_base` is
/// the configured `publicBaseUrl`.
pub fn steps(st: &FunnelStatus, local_port: u16, socket: &str, current_base: &str) -> Vec<String> {
    let ts = command(socket);
    let mut out = Vec::new();
    if !st.installed {
        out.push("Install Tailscale (the Tailscale app, or `brew install tailscale`) and sign in.".into());
        return out;
    }
    match st.state.as_str() {
        "Running" => {}
        "NeedsLogin" | "NeedsMachineAuth" => out.push(format!("Sign in to Tailscale: open the Tailscale app, or run `{ts} up`.")),
        "unreachable" => out.push(format!(
            "Start Tailscale (the app, or `sudo brew services start tailscale`). If this Mac runs its own tailscaled on another socket, set it with `review.funnel {{\"socket\": \"<path>\"}}`. ({})",
            st.error
        )),
        other => out.push(format!("Tailscale is {other}: start it and sign in (`{ts} up`).")),
    }
    if !st.running() {
        return out;
    }
    if !st.https_certs {
        out.push("In the Tailscale admin console, DNS page: turn on MagicDNS and HTTPS Certificates.".into());
    }
    if !st.funnel_allowed {
        out.push(
            "In the admin console, Access controls: give this machine the funnel attribute, e.g. \"nodeAttrs\": [{\"target\": [\"autogroup:member\"], \"attr\": [\"funnel\"]}]."
                .into(),
        );
    }
    match (st.serving, st.free_port()) {
        (Some(_), _) => {}
        (None, Some(p)) => {
            let https = if p == 443 { String::new() } else { format!(" --https={p}") };
            let note = match st.tailnet_only {
                Some(t) => format!(" (port {t} already forwards to it inside the tailnet only)"),
                None => String::new(),
            };
            out.push(format!("Turn on Funnel for the review server{note}: `{ts} funnel --bg{https} {local_port}`."));
        }
        (None, None) => out.push(format!(
            "Every Funnel port ({}) is used by another service: free one, or run the review server through another machine.",
            st.funnel_ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
        )),
    }
    if let Some(base) = st.base_url() {
        if current_base.trim_end_matches('/') != base {
            out.push(format!("Make links use it: `review.funnel {{\"use\": true}}` (sets publicBaseUrl to {base})."));
        }
    } else if st.serving.is_some() {
        out.push("Tailscale did not report this machine's DNS name; turn on MagicDNS.".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{"BackendState":"Running","Self":{"DNSName":"cutting-room.tail0000.ts.net.","CapMap":{"https":null,"funnel":null,"https://tailscale.com/cap/funnel-ports?ports=443,8443,10000":null}}}"#;

    fn out(status: &str, serve: &str) -> ProbeOutput {
        ProbeOutput { tool: Some("tailscale".into()), status: Ok(status.into()), serve: Ok(serve.into()) }
    }

    #[test]
    fn a_ready_machine_without_funnel_gets_the_command_for_a_free_port() {
        let serve = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"cutting-room.tail0000.ts.net:443":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:3000"}}}},"AllowFunnel":{"cutting-room.tail0000.ts.net:443":true}}"#;
        let st = read(&out(STATUS, serve), 8787);
        assert!(st.running() && st.https_certs && st.funnel_allowed);
        assert_eq!(st.dns_name, "cutting-room.tail0000.ts.net");
        assert_eq!((st.serving, st.taken.clone(), st.free_port()), (None, vec![443], Some(8443)));
        let s = steps(&st, 8787, "", "");
        assert_eq!(s, vec!["Turn on Funnel for the review server: `tailscale funnel --bg --https=8443 8787`.".to_string()]);
    }

    #[test]
    fn a_funnel_to_the_review_port_gives_the_public_base() {
        let serve = r#"{"TCP":{"443":{"HTTPS":true},"10000":{"HTTPS":true}},"Web":{"cutting-room.tail0000.ts.net:10000":{"Handlers":{"/":{"Proxy":"http://127.0.0.1:8787"}}}},"AllowFunnel":{"cutting-room.tail0000.ts.net:10000":true}}"#;
        let st = read(&out(STATUS, serve), 8787);
        assert_eq!((st.serving, st.taken.clone()), (Some(10000), vec![443]));
        assert_eq!(st.base_url().as_deref(), Some("https://cutting-room.tail0000.ts.net:10000"));
        assert_eq!(steps(&st, 8787, "", "").len(), 1, "only: make links use it");
        assert!(steps(&st, 8787, "", "https://cutting-room.tail0000.ts.net:10000/").is_empty());
    }

    #[test]
    fn serve_without_funnel_is_tailnet_only() {
        let serve = r#"{"TCP":{"443":{"HTTPS":true}},"Web":{"cutting-room.tail0000.ts.net:443":{"Handlers":{"/":{"Proxy":"localhost:8787"}}}}}"#;
        let st = read(&out(STATUS, serve), 8787);
        assert_eq!((st.serving, st.tailnet_only, st.free_port()), (None, Some(443), Some(443)));
        assert!(steps(&st, 8787, "/tmp/a b.sock", "")[0].contains("tailscale --socket='/tmp/a b.sock' funnel --bg 8787"));
    }

    #[test]
    fn missing_signed_out_and_unreachable_tailscale_say_what_to_do() {
        let none = read(&ProbeOutput::not_installed(), 8787);
        assert_eq!(none.state, "notInstalled");
        assert!(steps(&none, 8787, "", "")[0].starts_with("Install Tailscale"));
        let out_ = read(&out(r#"{"BackendState":"NeedsLogin","Self":{}}"#, "{}"), 8787);
        assert_eq!(steps(&out_, 8787, "", "").len(), 1);
        let down = read(&ProbeOutput { tool: Some("tailscale".into()), status: Err("dial unix: no such file".into()), serve: Err("x".into()) }, 8787);
        assert_eq!(down.state, "unreachable");
        assert!(steps(&down, 8787, "", "")[0].contains("no such file"));
        let bare = read(&out(r#"{"BackendState":"Running","Self":{"DNSName":"m.t.ts.net."}}"#, "{}"), 8787);
        let s = steps(&bare, 8787, "", "");
        assert!(s[0].contains("HTTPS Certificates") && s[1].contains("funnel attribute") && s[2].contains("funnel --bg 8787"), "{s:?}");
    }

    #[test]
    fn hostile_output_never_breaks_the_status() {
        let names = ["evil.com/x.", "a..b.", "-a.b.", "a b.c.", "<script>.ts.net."];
        for n in names {
            let st = read(&out(&format!(r#"{{"BackendState":"Running","Self":{{"DNSName":"{n}"}}}}"#), "{}"), 8787);
            assert_eq!(st.dns_name, "", "{n}");
            assert_eq!(st.base_url(), None);
        }
        for junk in ["", "null", "[]", "{", "\u{0}", r#"{"Web":{"x":1,"y:z":{"Handlers":5}},"TCP":{"99999":1,"-1":2},"AllowFunnel":7}"#] {
            let st = read(&out(junk, junk), 8787);
            assert_eq!(st.serving, None);
        }
        let big = format!("{{\"BackendState\":\"{}\"}}", "x".repeat(MAX_OUTPUT));
        assert_eq!(read(&out(&big, "{}"), 8787).state, "unreachable");
        let ports = read(&out(r#"{"BackendState":"Running","Self":{"CapMap":{"https://tailscale.com/cap/funnel-ports?ports=0,x,65536,8443":1}}}"#, "{}"), 8787);
        assert_eq!(ports.funnel_ports, vec![8443]);
    }

    #[test]
    fn serve_targets_must_be_this_port_on_this_machine() {
        for t in ["http://127.0.0.1:8787", "http://localhost:8787/", "127.0.0.1:8787", "8787", "http://[::1]:8787"] {
            assert!(proxies_to(t, 8787), "{t}");
        }
        for t in ["http://10.0.0.2:8787", "http://127.0.0.1:87870", "https+insecure://localhost:8787", "8788", "", "http://evil:8787"] {
            assert!(!proxies_to(t, 8787), "{t}");
        }
    }
}
