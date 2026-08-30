//! TCP streaming server. Sits on the fast USB-Ethernet link to the iPad.
//!
//! One TCP connection per client:
//!   * a writer loop captures + encodes + streams H.264 frames,
//!   * a reader thread receives multitouch messages and injects them into Windows.

use crate::capture::{CaptureError, MonitorCapture, MonitorCaptureConfig};
use crate::encoder::H264Encoder;
use crate::protocol;
use crate::touch::{ScreenMapping, TouchInjector};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

pub fn run_server(
    bind: &str,
    port: u16,
    monitor_index: usize,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind((bind, port))?;
    println!("touchmonitor-server listening on {bind}:{port}");
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

        std::thread::spawn(move || {
            if let Err(e) = handle_client(stream, monitor_index, fps, bitrate_kbps) {
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
) -> Result<(), Box<dyn std::error::Error>> {
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

    let mut encoder =
        H264Encoder::new(bounds.width as usize, bounds.height as usize, fps, bitrate_kbps)?;
    let mapping = ScreenMapping {
        width: bounds.width,
        height: bounds.height,
        hmonitor: bounds.hmonitor.0 as usize,
    };
    let injector = TouchInjector::new(&mapping);

    // Split the TCP stream for concurrent read/write.
    let mut write_stream = stream.try_clone()?;
    let mut reader = stream;

    // Reader thread: touch messages in, inject into Windows.
    let _reader_thread = std::thread::spawn(move || {
        let mut injector = injector;
        loop {
            let (msg, payload) = match protocol::read_message(&mut reader) {
                Ok(m) => m,
                Err(_) => break, // client closed the stream
            };
            println!("recv: type={msg} len={}", payload.len());
            match msg {
                protocol::MSG_TOUCH => {
                    if let Some(events) = protocol::decode_touch(&payload) {
                        let first = events.first().map(|e| format!("id={} a={} x={:.2} y={:.2}", e.0, e.1, e.2, e.3)).unwrap_or_default();
                        println!("touch: {} events [{}]", events.len(), first);
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
    let mut out = Vec::new();
    loop {
        match capture.next_frame_timeout(Duration::from_millis(200)) {
            Ok(frame) => {
                let encoded = encoder.encode_frame(&frame.bgra)?;
                out.clear();
                let payload = protocol::make_video_payload(
                    encoded.keyframe,
                    frame.width,
                    frame.height,
                    &encoded.data,
                );
                protocol::write_message(&mut out, protocol::MSG_VIDEO, &payload);
                write_stream.write_all(&out)?;
                write_stream.flush()?;
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
