//! "Network test": checks everything that decides whether friends can connect and how good it
//! looks, then suggests a quality setting. Speed uses Cloudflare's public speed-test endpoint.
use std::net::{IpAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

pub struct Line {
    pub ok: bool,
    pub text: String,
}

pub struct Report {
    pub lines: Vec<Line>,
    pub upload_mbps: Option<f64>,
    pub download_mbps: Option<f64>,
    /// Suggested max bitrate for hosting (Mbps), from the upload speed.
    pub suggest_mbps: Option<u32>,
}

fn line(lines: &mut Vec<Line>, ok: bool, text: impl Into<String>) {
    lines.push(Line { ok, text: text.into() });
}

/// Public mapping seen by two different STUN servers from one socket.
fn nat_check() -> (Option<std::net::SocketAddr>, Option<std::net::SocketAddr>) {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return (None, None) };
    let ask = |server: &str| {
        let addr = server.to_socket_addrs().ok()?.find(|a| a.is_ipv4())?;
        let txid: [u8; 12] = rand::random();
        sock.send_to(&crate::stun::request(&txid), addr).ok()?;
        let _ = sock.set_read_timeout(Some(Duration::from_millis(1500)));
        let mut buf = [0u8; 512];
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            let (n, _) = sock.recv_from(&mut buf).ok()?;
            if let Some((t, a)) = crate::stun::parse(&buf[..n]) {
                if t == txid {
                    return Some(a);
                }
            }
        }
        None
    };
    (ask("stun.l.google.com:19302"), ask("stun.cloudflare.com:3478"))
}

fn speed_down() -> Option<f64> {
    let t = Instant::now();
    let mut resp = ureq::get("https://speed.cloudflare.com/__down?bytes=25000000").call().ok()?;
    let n = std::io::copy(&mut resp.body_mut().as_reader(), &mut std::io::sink()).ok()?;
    Some(n as f64 * 8.0 / t.elapsed().as_secs_f64() / 1e6)
}

fn speed_up() -> Option<f64> {
    let data = vec![0u8; 15_000_000];
    let t = Instant::now();
    ureq::post("https://speed.cloudflare.com/__up").send(&data[..]).ok()?;
    Some(data.len() as f64 * 8.0 / t.elapsed().as_secs_f64() / 1e6)
}

/// Runs every check (takes ~10-20 s). `friends_online` = connected to the friend network.
pub fn run(friends_online: bool) -> Report {
    let mut l = Vec::new();
    line(&mut l, friends_online, if friends_online { "Friend network: connected" } else { "Friend network: not connected (check your internet)" });

    match nat_check() {
        (Some(a), Some(b)) if a.port() == b.port() => {
            line(&mut l, true, format!("Router: direct connections should work (public address {})", a.ip()))
        }
        (Some(_), Some(_)) => line(&mut l, false, "Router: strict (symmetric NAT). Direct connections may fail; enable UPnP or forward a port"),
        _ => line(&mut l, false, "Router: couldn't reach STUN servers (UDP may be blocked)"),
    }

    let mut opts = igd_next::SearchOptions::default();
    opts.timeout = Some(Duration::from_secs(4));
    match igd_next::search_gateway(opts).ok().and_then(|g| g.get_external_ip().ok()) {
        Some(ip) if crate::proto::is_private(ip) => line(&mut l, false, format!("UPnP: router found, but your ISP uses CGNAT ({ip}); friends on strict networks may not reach you")),
        Some(ip) => line(&mut l, true, format!("UPnP: available (public IP {ip}), gives friends an extra way in")),
        None => line(&mut l, false, "UPnP: not available (optional; turning it on in the router helps strict networks)"),
    }

    match crate::host::local_ip(true) {
        Some(IpAddr::V6(v6)) if !crate::proto::is_private(v6.into()) => line(&mut l, true, "IPv6: available"),
        _ => line(&mut l, false, "IPv6: not available (optional)"),
    }

    // Language-independent: ask which programs the "Kinlink" rules allow, compare to this exe.
    let allowed = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "Get-NetFirewallRule -DisplayName 'Kinlink','Parsec Clone' -ErrorAction SilentlyContinue | Get-NetFirewallApplicationFilter | ForEach-Object { $_.Program }"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase())
        .unwrap_or_default();
    let exe = std::env::current_exe().map(|p| p.display().to_string().to_lowercase()).unwrap_or_default();
    match (allowed.lines().any(|p| p.trim() == exe) || crate::pkg::packaged(), allowed.trim().is_empty()) {
        (true, _) if crate::pkg::packaged() => line(&mut l, true, "Windows Firewall: allowed by the Store package"),
        (true, _) => line(&mut l, true, "Windows Firewall: this app is allowed"),
        (false, false) => line(&mut l, false, "Windows Firewall: an older copy of the app is allowed, not this one (use the button in Settings)"),
        (false, true) => line(&mut l, false, "Windows Firewall: not allowed yet (use the button in Settings; may block friends)"),
    }

    let vanguard = std::process::Command::new("sc").args(["query", "vgk"]).output().is_ok_and(|o| o.status.success());
    if vanguard {
        line(&mut l, false, "Riot Vanguard installed: Alt+Tab and the Win key stay on your PC while you control others");
    }
    let vigem = vigem_client::Client::connect().is_ok();
    line(&mut l, vigem, if vigem { "Controller driver (ViGEmBus): installed" } else { "Controller driver (ViGEmBus): missing, friends' controllers won't work on this PC" });

    let download_mbps = speed_down();
    let upload_mbps = speed_up();
    match (download_mbps, upload_mbps) {
        (Some(d), Some(u)) => line(&mut l, u >= 10.0, format!("Internet speed: {d:.0} Mbps down / {u:.0} Mbps up")),
        _ => line(&mut l, false, "Internet speed: couldn't measure"),
    }
    // Hosting needs upload; keep ~30% headroom (and split it if several friends join).
    let suggest_mbps = upload_mbps.map(|u| ((u * 0.7) as u32).clamp(3, 60));
    Report { lines: l, upload_mbps, download_mbps, suggest_mbps }
}
