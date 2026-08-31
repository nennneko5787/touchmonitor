//! touchmonitor-server
//!
//! PC side of the "TouchMonitor" remote-desktop-over-USB app.
//!
//!  1. Captures the desktop using Windows Graphics Capture (DXGI / GPU).
//!  2. H.264-encodes it with the hardware-preferred Media Foundation MFT.
//!  3. Streams the encoded frames over a TCP connection (which rides on the fast
//!     USB Ethernet / tethering link to an iPad).
//!  4. Receives multitouch events from the iPad on the same TCP connection and
//!     injects them into Windows with SendTouchInput.

mod capture;
mod encoder_mft;
mod protocol;
mod service;
mod stream;
mod touch;
mod udp;

const BUILD_COMMIT: &str = env!("TOUCHMONITOR_COMMIT");

fn main() {
    println!("TouchMonitor server build {BUILD_COMMIT}");
    let mut port: u16 = 5666;
    let mut monitor_index: usize = 0;
    let mut bitrate_kbps: u32 = 8000;
    let mut fps: u32 = 60;
    let mut keyframe_interval: u32 = fps;
    let mut show_monitors = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => { let _ = args.next(); }
            "--port" | "-p" => {
                port = args.next().and_then(|s| s.parse().ok()).unwrap_or(port)
            }
            "--monitor" | "-m" => {
                monitor_index = args.next().and_then(|s| s.parse().ok()).unwrap_or(monitor_index)
            }
            "--bitrate" | "-b" => {
                bitrate_kbps = args.next().and_then(|s| s.parse().ok()).unwrap_or(bitrate_kbps)
            }
            "--fps" => fps = args.next().and_then(|s| s.parse().ok()).unwrap_or(fps),
            "--keyframe-interval" => keyframe_interval = args.next().and_then(|s| s.parse().ok()).unwrap_or(keyframe_interval),
            "--list-monitors" => show_monitors = true,
            "--help" | "-h" => {
                print_help();
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                print_help();
                return;
            }
        }
    }

    if show_monitors {
        match capture::list_monitors() {
            Ok(monitors) => {
                for (i, m) in monitors.iter().enumerate() {
                    println!("monitor {i}: {}x{}", m.width, m.height);
                }
            }
            Err(e) => eprintln!("failed to enumerate monitors: {e}"),
        }
        return;
    }

    if let Err(e) = stream::run_server("0.0.0.0", port, monitor_index, fps, bitrate_kbps, keyframe_interval) {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "touchmonitor-server build {BUILD_COMMIT}\n\n\
         usage: touchmonitor-server [options]\n\n\
         options:\n\
         \x20 -p, --port <n>       TCP port (default: 5666)\n\
         \x20 -m, --monitor <n>    monitor index to stream (default: 0)\n\
         \x20 -b, --bitrate <n>    H.264 bitrate in kbps (default: 8000)\n\
         \x20     --fps <n>        encode frame rate (default: 60)\n\
         \x20     --list-monitors  list connected monitors with their sizes\n\
         \x20 -h, --help           show this help"
    );
}
