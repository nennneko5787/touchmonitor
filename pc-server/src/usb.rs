//! USB transport using Apple's usbmuxd protocol through libusbmuxd's `iproxy`.
//!
//! `iproxy HOST:DEVICE` exposes an iOS localhost TCP listener through usbmuxd.
//! The PC then connects to that local proxy. Video stays on the same reliable
//! TCP stream, because usbmuxd is a TCP transport and cannot carry the existing
//! UDP video path.

use crate::stream;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const PROXY_PORT: u16 = 5667;

pub fn run_server(
    device_port: u16,
    monitor_index: usize,
    fps: u32,
    bitrate_kbps: u32,
    keyframe_interval: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut proxy = start_iproxy(PROXY_PORT, device_port)?;
    println!("USB transport: usbmuxd/iproxy 127.0.0.1:{PROXY_PORT} -> device localhost:{device_port}");
    println!("USB transport: waiting for TouchMonitor iOS listener...");

    let result = loop {
        if let Some(status) = proxy.try_wait()? {
            break Err(format!("iproxy exited before the iOS connection arrived: {status}").into());
        }

        match TcpStream::connect(("127.0.0.1", PROXY_PORT)) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                println!("USB transport: iOS connection established");
                break stream::handle_client_usb(
                    stream,
                    monitor_index,
                    fps,
                    bitrate_kbps,
                    keyframe_interval,
                );
            }
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    };

    let _ = proxy.kill();
    let _ = proxy.wait();
    result
}

fn start_iproxy(host_port: u16, device_port: u16) -> Result<Child, Box<dyn std::error::Error>> {
    let mapping = format!("{host_port}:{device_port}");
    let child = Command::new("iproxy.exe")
        .arg(mapping)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| {
            format!(
                "could not start iproxy.exe: {error}; install libusbmuxd/iproxy and ensure it is on PATH"
            )
        })?;
    Ok(child)
}
