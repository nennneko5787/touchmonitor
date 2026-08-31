//! TCP streaming server. Sits on the fast USB-Ethernet link to the iPad.
//!
//! One TCP connection per client:
//!   * a writer loop captures + encodes + streams H.264 frames,
//!   * a reader thread receives multitouch messages and injects them into Windows.

use crate::capture::{CaptureError, MonitorCapture, MonitorCaptureConfig};
use crate::encoder_mft::MftEncoder;
use crate::protocol;
use crate::touch::{ScreenMapping, TouchInjector};
use crate::udp::VideoUdp;
use std::io::Write;
use std::net::{IpAddr, TcpListener, TcpStream};
use std::time::Duration;

pub fn run_server(
    _bind: &str,
    port: u16,
    monitor_index: usize,
    fps: u32,
    bitrate_kbps: u32,
    keyframe_interval: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    // TCP and UDP may share a port number. This lets the Bonjour endpoint be
    // reused for both transports without exposing a second port to the client.
    let video_udp = VideoUdp::bind(port)?;
    let video_port = video_udp.port()?;
    let _advertisement = crate::service::advertise(port)?;
    println!("touchmonitor-server ready; discover via Bonjour _touchmonitor._tcp on the USB link (port {port})");
    println!("streaming monitor #{monitor_index} @ {fps} fps, {bitrate_kbps} kbps (USB link)");

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        if let Err(e) = stream.set_nodelay(true) {
            eprintln!("set_nodelay: {e}");
        }
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        println!("client connected: {peer}");
        let video_udp_for_client = video_udp.clone();

        std::thread::spawn(move || {
            if let Err(e) = handle_client(stream, monitor_index, fps, bitrate_kbps, keyframe_interval, video_udp_for_client, video_port) {
                eprintln!("client {peer} error: {e}");
            }
            println!("client disconnected: {peer}");
        });
    }

    Ok(())
}

fn handle_client(
    stream: TcpStream,
    monitor_index: usize,
    fps: u32,
    bitrate_kbps: u32,
    keyframe_interval: u32,
    video_udp: VideoUdp,
    video_port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    crate::encoder_mft::ensure_com_initialized();
    let cfg = MonitorCaptureConfig {
        buffers: 2,
        include_cursor: true,
    };
    let capture = MonitorCapture::start(monitor_index, cfg)?;
    let bounds = capture.bounds();
    println!(
        "capturing monitor #{monitor_index}: {}x{} at origin ({},{})",
        bounds.width, bounds.height, bounds.left, bounds.top
    );

    println!("initializing H.264 MFT encoder...");
    let mut encoder = MftEncoder::new(bounds.width, bounds.height, fps, bitrate_kbps, keyframe_interval)?;
    println!("H.264 MFT encoder initialized");
    let mapping = ScreenMapping {
        width: bounds.width,
        height: bounds.height,
        hmonitor: bounds.hmonitor.0 as usize,
    };
    let injector = TouchInjector::new(&mapping);

    // Split the TCP stream for concurrent read/write.
    let mut write_stream = stream.try_clone()?;
    let peer_ip: IpAddr = stream.peer_addr()?.ip();
    write_stream.write_all(&protocol::make_hello(video_port))?;
    let mut reader = stream;

// Reader thread: touch messages in, inject into Windows.
     let _reader_thread = std::thread::spawn(move || {
         let mut injector = injector;
         loop {
             let (msg, payload) = match protocol::read_message(&mut reader) {
                 Ok(m) => m,
                 Err(_) => break, // client closed the stream
             };
             // println!("recv: type={msg} len={}", payload.len());
             match msg {
                 protocol::MSG_TOUCH => {
                     if let Some(events) = protocol::decode_touch(&payload) {
                         match injector.apply_and_send(&events, &mapping) {
                             Ok(()) => {}
                             Err(e) => eprintln!("touch inject error: {e:?}"),
                         }
                     } else {
                         println!("touch: decode FAILED (payload len {})", payload.len());
                     }
                 }
                 protocol::MSG_PING => { /* no-op; TCP handles keep-alive */ }
                 _ => break, // unexpected message from the client
             }
         }
     });

// Writer loop: capture, encode, stream.
     let mut frame_count = 0u64;
     let mut sent_reliable_keyframe = false;
     loop {
         match capture.next_frame_timeout(Duration::from_millis(200)) {
             Ok(frame) => {
                frame_count = frame_count.wrapping_add(1);
                let Some(encoded) = encoder.encode_frame(&frame.bgra)? else {
                    continue;
                };
                if frame_count <= 3 {
                    println!("encoded frame #{frame_count}: {} bytes, keyframe={}", encoded.data.len(), encoded.keyframe);
                }
                video_udp.send_frame(peer_ip, frame_count as u32, encoded.keyframe, frame.width, frame.height, &encoded.data);
                // A 1080p IDR spans many UDP datagrams. Send the initial
                // decoder reference image reliably, then keep video on UDP.
                let needs_tcp_keyframe = encoded.keyframe && !sent_reliable_keyframe;
                let needs_tcp_fallback = frame_count <= 2 && !video_udp.has_client(peer_ip);
                if needs_tcp_keyframe || needs_tcp_fallback {
                    let payload = protocol::make_video_payload(encoded.keyframe, frame.width, frame.height, &encoded.data);
                    let mut fallback = Vec::with_capacity(payload.len() + 5);
                    protocol::write_message(&mut fallback, protocol::MSG_VIDEO, &payload);
                    write_stream.write_all(&fallback)?;
                    if encoded.keyframe {
                        sent_reliable_keyframe = true;
                    }
                    println!("sent TCP startup keyframe/fallback: {} bytes", fallback.len());
                }
             }
            Err(CaptureError::Timeout) => {
                // Nothing new to send; keep waiting.
                continue;
            }
            Err(CaptureError::Closed(msg)) => {
                eprintln!("capture closed: {msg}");
                break;
            }
        }
    }

    Ok(())
}
