//! Service registration through the Windows DNS-SD API.

use std::ffi::c_void;
use std::net::{Ipv4Addr, UdpSocket};
use std::ptr;
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::Dns::*;

unsafe extern "system" fn registration_complete(status: u32, _context: *const c_void, _instance: *const DNS_SERVICE_INSTANCE) {
    if status != 0 { eprintln!("Bonjour registration callback failed: {status}"); }
}

pub struct Advertisement {
    request: DNS_SERVICE_REGISTER_REQUEST,
    instance_name: Vec<u16>,
    instance: Box<DNS_SERVICE_INSTANCE>,
    ip4: Box<u32>,
    cancel: DNS_SERVICE_CANCEL,
}

pub fn advertise(port: u16) -> Result<Advertisement, Box<dyn std::error::Error>> {
    let instance_name: Vec<u16> = "TouchMonitor._touchmonitor._tcp.local.\0".encode_utf16().collect();
    let ip = local_ipv4().unwrap_or(Ipv4Addr::new(127, 0, 0, 1));
    // IP4_ADDRESS is stored in network byte order. `from_ne_bytes` keeps the
    // octets in the same order in memory on Windows, whereas `from_be_bytes`
    // would make the DNS-SD A record point at the reversed address.
    let mut ip4 = Box::new(u32::from_ne_bytes(ip.octets()));
    let instance = DNS_SERVICE_INSTANCE {
        pszInstanceName: PWSTR(instance_name.as_ptr() as *mut u16),
        // NULL makes Bonjour use the system's already-advertised host name.
        // Supplying a custom *.local name creates an SRV record without an A
        // record in Windows' DNS-SD implementation.
        pszHostName: PWSTR::null(),
        ip4Address: &mut *ip4,
        ip6Address: ptr::null_mut(),
        wPort: port,
        wPriority: 0,
        wWeight: 0,
        dwPropertyCount: 0,
        keys: ptr::null_mut(),
        values: ptr::null_mut(),
        dwInterfaceIndex: 0,
    };
    let mut boxed_instance = Box::new(instance);
    let mut cancel = DNS_SERVICE_CANCEL::default();
    let request = DNS_SERVICE_REGISTER_REQUEST {
        Version: 1,
        InterfaceIndex: 0,
        pServiceInstance: &mut *boxed_instance,
        pRegisterCompletionCallback: Some(registration_complete),
        pQueryContext: ptr::null_mut::<c_void>(),
        hCredentials: HANDLE::default(),
        unicastEnabled: BOOL(0),
    };
    let status = unsafe { DnsServiceRegister(&request, Some(&mut cancel)) };
    if status != 9506u32 { // DNS_REQUEST_PENDING
        return Err(format!("DnsServiceRegister failed: {status}").into());
    }
    println!("Bonjour advertisement: TouchMonitor._touchmonitor._tcp.local. -> system host ({ip}:{port})");
    Ok(Advertisement { request, instance_name, instance: boxed_instance, ip4, cancel })
}

fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()? { std::net::SocketAddr::V4(a) => Some(*a.ip()), _ => None }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        unsafe { let _ = DnsServiceDeRegister(&self.request, Some(&mut self.cancel)); }
    }
}
