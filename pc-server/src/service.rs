//! Bonjour proxy registration through the installed Windows Bonjour utility.
//!
//! `dns-sd -P` is the Bonjour-supported way to publish a service together
//! with its target host and A record. This is the exact path verified on the
//! target Windows installation.

use std::net::{Ipv4Addr, UdpSocket};
use std::process::{Child, Command, Stdio};

pub struct Advertisement {
    process: Child,
}

pub fn advertise(port: u16) -> Result<Advertisement, Box<dyn std::error::Error>> {
    let ip = local_ipv4().ok_or("could not determine the local IPv4 address")?;
    let computer_name = std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "touchmonitor".to_owned());
    let host = format!("{computer_name}.local");
    let port_text = port.to_string();
    let ip_text = ip.to_string();
    let mut process = Command::new("dns-sd.exe")
        .args(["-P", "TouchMonitor", "_touchmonitor._tcp", "local", &port_text, &host, &ip_text])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start Bonjour dns-sd.exe: {error}; install Apple Bonjour Service"))?;

    std::thread::sleep(std::time::Duration::from_millis(150));
    if let Some(status) = process.try_wait()? {
        return Err(format!("Bonjour dns-sd.exe exited during registration: {status}").into());
    }
    println!("Bonjour proxy advertisement: TouchMonitor._touchmonitor._tcp.local. -> {host} ({ip}:{port})");
    Ok(Advertisement { process })
}

fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()? {
        std::net::SocketAddr::V4(address) => Some(*address.ip()),
        _ => None,
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
