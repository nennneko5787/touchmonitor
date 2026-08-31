//! Minimal dependency-free Bonjour/mDNS advertisement for USB networking.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

pub struct Advertisement { stop: Arc<AtomicBool>, thread: Option<JoinHandle<()>> }

pub fn advertise(port: u16) -> io::Result<Advertisement> {
    let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    let ip = local_ipv4(&probe).unwrap_or(Ipv4Addr::new(127, 0, 0, 1));
    // Do not bind 5353: Windows commonly already has an mDNS responder
    // (Bonjour/mDNSResponder) listening there. An ephemeral UDP source port
    // is valid for unsolicited mDNS announcements and coexists with it.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_multicast_ttl_v4(255)?;
    socket.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        let packet = response_packet(port, ip);
        let mut last = std::time::Instant::now() - Duration::from_secs(31);
        while !stop_thread.load(Ordering::Relaxed) {
            if last.elapsed() >= Duration::from_secs(5) {
                let _ = socket.send_to(&packet, SocketAddrV4::new(GROUP, 5353));
                last = std::time::Instant::now();
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    Ok(Advertisement { stop, thread: Some(thread) })
}

fn local_ipv4(socket: &UdpSocket) -> Option<Ipv4Addr> {
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()? { std::net::SocketAddr::V4(a) => Some(*a.ip()), _ => None }
}

fn name(s: &str, out: &mut Vec<u8>) {
    for part in s.trim_end_matches('.').split('.') { out.push(part.len() as u8); out.extend_from_slice(part.as_bytes()); }
    out.push(0);
}

fn rr(out: &mut Vec<u8>, n: &str, typ: u16, data: &[u8]) {
    name(n, out); out.extend_from_slice(&typ.to_be_bytes()); out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&120u32.to_be_bytes()); out.extend_from_slice(&(data.len() as u16).to_be_bytes()); out.extend_from_slice(data);
}

fn response_packet(port: u16, ip: Ipv4Addr) -> Vec<u8> {
    let service = "_touchmonitor._tcp.local."; let instance = "TouchMonitor._touchmonitor._tcp.local."; let host = "touchmonitor.local.";
    let mut p = vec![0,0, 0x84,0, 0,4, 0,0, 0,0, 0,0];
    let mut d = Vec::new(); name(instance, &mut d); rr(&mut p, service, 12, &d);
    d.clear(); d.extend_from_slice(&[0,0, 0,0]); d.extend_from_slice(&port.to_be_bytes()); name(host, &mut d); rr(&mut p, instance, 33, &d);
    rr(&mut p, instance, 16, &[0]); rr(&mut p, host, 1, &ip.octets()); p
}

impl Drop for Advertisement {
    fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); if let Some(t) = self.thread.take() { let _ = t.join(); } }
}
