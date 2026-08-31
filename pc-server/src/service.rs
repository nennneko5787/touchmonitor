//! Bonjour registration through Apple's Windows Bonjour helper.
//!
//! Windows' built-in DnsServiceRegister can publish the SRV record without a
//! usable A record. The installed `dns-sd -R` utility publishes the complete
//! service record set through mDNSResponder.

use std::process::{Child, Command, Stdio};

pub struct Advertisement {
    process: Child,
}

pub fn advertise(port: u16) -> Result<Advertisement, Box<dyn std::error::Error>> {
    let port_text = port.to_string();
    let mut process = Command::new("dns-sd.exe")
        .args(["-R", "TouchMonitor", "_touchmonitor._tcp", "local", &port_text])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start Bonjour dns-sd.exe: {error}; install Apple Bonjour Service"))?;

    std::thread::sleep(std::time::Duration::from_millis(150));
    if let Some(status) = process.try_wait()? {
        return Err(format!("Bonjour dns-sd.exe exited during registration: {status}").into());
    }

    println!("Bonjour advertisement: TouchMonitor._touchmonitor._tcp.local. (port {port})");
    Ok(Advertisement { process })
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
