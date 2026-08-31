//! Bonjour registration through Apple's Windows `dnssd.dll`.

use std::ffi::{c_char, c_void};
use std::mem::transmute;
use std::net::{Ipv4Addr, UdpSocket};
use windows::core::{s, w, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type ServiceRef = *mut c_void;
type RecordRef = *mut c_void;
type DnsError = i32;
type RegisterFn = unsafe extern "system" fn(*mut ServiceRef, u32, u32, *const c_char, *const c_char, *const c_char, *const c_char, u16, u16, *const c_void, Option<unsafe extern "system" fn()>, *mut c_void) -> DnsError;
type CreateConnectionFn = unsafe extern "system" fn(*mut ServiceRef) -> DnsError;
type RegisterRecordFn = unsafe extern "system" fn(ServiceRef, *mut RecordRef, u32, *const c_char, u16, u16, u16, *const c_void, u32, Option<unsafe extern "system" fn()>, *mut c_void) -> DnsError;
type DeallocateFn = unsafe extern "system" fn(ServiceRef);

const NO_ERROR: DnsError = 0;
const CLASS_IN: u16 = 1;
const TYPE_A: u16 = 1;

pub struct Advertisement {
    module: HMODULE,
    service: ServiceRef,
    records: ServiceRef,
    deallocate: DeallocateFn,
}

pub fn advertise(port: u16) -> Result<Advertisement, Box<dyn std::error::Error>> {
    let module = unsafe { LoadLibraryW(w!("dnssd.dll"))? };
    match unsafe { register(module, port) } {
        Ok((service, records, deallocate)) => {
            println!("Bonjour advertisement via dnssd.dll: TouchMonitor._touchmonitor._tcp.local. (port {port})");
            Ok(Advertisement { module, service, records, deallocate })
        }
        Err(error) => {
            Err(error.into())
        }
    }
}

unsafe fn register(module: HMODULE, port: u16) -> Result<(ServiceRef, ServiceRef, DeallocateFn), String> {
    let symbol = |name: PCSTR| -> Result<*const c_void, String> {
        GetProcAddress(module, name).map(|proc| proc as *const c_void)
            .ok_or_else(|| "required Bonjour function is missing from dnssd.dll".to_owned())
    };
    let register: RegisterFn = transmute(symbol(s!("DNSServiceRegister"))?);
    let create_connection: CreateConnectionFn = transmute(symbol(s!("DNSServiceCreateConnection"))?);
    let register_record: RegisterRecordFn = transmute(symbol(s!("DNSServiceRegisterRecord"))?);
    let deallocate: DeallocateFn = transmute(symbol(s!("DNSServiceRefDeallocate"))?);
    let ip = local_ipv4().ok_or("could not determine the local IPv4 address")?;
    let computer_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "touchmonitor".to_owned());
    let host = format!("{computer_name}.local.");
    let name = b"TouchMonitor\0";
    let regtype = b"_touchmonitor._tcp\0";
    let domain = b"local\0";
    let host_bytes = host.as_bytes();
    let mut service: ServiceRef = std::ptr::null_mut();
    let error = register(&mut service, 0, 0, name.as_ptr() as _, regtype.as_ptr() as _, domain.as_ptr() as _, host_bytes.as_ptr() as _, port.to_be(), 0, std::ptr::null(), None, std::ptr::null_mut());
    if error != NO_ERROR { return Err(format!("DNSServiceRegister failed: {error}")); }
    let mut records: ServiceRef = std::ptr::null_mut();
    let error = create_connection(&mut records);
    if error != NO_ERROR { deallocate(service); return Err(format!("DNSServiceCreateConnection failed: {error}")); }
    let address = ip.octets();
    let mut record: RecordRef = std::ptr::null_mut();
    let error = register_record(records, &mut record, 0, host_bytes.as_ptr() as _, TYPE_A, CLASS_IN, address.len() as u16, address.as_ptr() as _, 120, None, std::ptr::null_mut());
    if error != NO_ERROR {
        deallocate(records);
        deallocate(service);
        return Err(format!("DNSServiceRegisterRecord(A) failed: {error}"));
    }
    Ok((service, records, deallocate))
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
        unsafe {
            (self.deallocate)(self.records);
            (self.deallocate)(self.service);
        }
    }
}
