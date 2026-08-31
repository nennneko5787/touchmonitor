//! Best-effort low-latency video transport. Lost chunks discard only one frame.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::thread;

const MAGIC: &[u8; 4] = b"TMV1";
const HEADER: usize = 21;
const PAYLOAD: usize = 1200;

#[derive(Clone)]
pub struct VideoUdp { socket: Arc<UdpSocket>, clients: Arc<Mutex<HashMap<IpAddr, SocketAddr>>>, stop: Arc<AtomicBool> }

impl VideoUdp {
    pub fn bind(port: u16) -> io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind(("0.0.0.0", port))?);
        socket.set_nonblocking(true)?;
        let clients = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&socket); let c = Arc::clone(&clients); let done = Arc::clone(&stop);
        thread::spawn(move || {
            let mut buf = [0u8; 64];
            while !done.load(Ordering::Relaxed) {
                if let Ok((n, from)) = s.recv_from(&mut buf) {
                    if n >= 6 && &buf[..4] == b"TMREG" { c.lock().unwrap().insert(from.ip(), from); }
                }
                thread::sleep(std::time::Duration::from_millis(2));
            }
        });
        Ok(Self { socket, clients, stop })
    }

    pub fn port(&self) -> io::Result<u16> { Ok(self.socket.local_addr()?.port()) }

    pub fn send_frame(&self, peer_ip: IpAddr, frame_id: u32, keyframe: bool, width: u32, height: u32, data: &[u8]) {
        let Some(to) = self.clients.lock().unwrap().get(&peer_ip).copied() else { return };
        let total = ((data.len() + PAYLOAD - 1) / PAYLOAD).min(u16::MAX as usize) as u16;
        for index in 0..total {
            let start = index as usize * PAYLOAD; let end = (start + PAYLOAD).min(data.len());
            let mut packet = Vec::with_capacity(HEADER + end - start);
            packet.extend_from_slice(MAGIC); packet.extend_from_slice(&frame_id.to_le_bytes());
            packet.extend_from_slice(&index.to_le_bytes()); packet.extend_from_slice(&total.to_le_bytes());
            packet.push(keyframe as u8); packet.extend_from_slice(&width.to_le_bytes()); packet.extend_from_slice(&height.to_le_bytes());
            packet.extend_from_slice(&data[start..end]); let _ = self.socket.send_to(&packet, to);
        }
    }
}

impl Drop for VideoUdp { fn drop(&mut self) { self.stop.store(true, Ordering::Relaxed); } }
