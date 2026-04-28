use rusb::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};
use std::collections::VecDeque;

// Skylanders Portal USB vendor ID.
const PORTAL_VENDOR_ID: u16 = 0x1430;

// Portal USB interface and endpoint.
const PORTAL_INTERFACE: u8 = 0;
const PORTAL_ENDPOINT: u8 = 0x02;

// Portal color command header bytes.
const COLOR_COMMAND: [u8; 3] = [0x0B, 0x14, 0x43];

// Number of samples per FFT frame.
const FRAME_SIZE: usize = 1024;

fn main() {
    // Find the active PulseAudio monitor source and set it as the input.
    let monitor = std::process::Command::new("pactl")
        .args(["list", "short", "sources"])
        .output()
        .expect("Failed to run pactl.")
        .stdout;

    let monitor_name = std::str::from_utf8(&monitor)
        .expect("Invalid pactl output.")
        .lines()
        .find(|line| line.contains(".monitor") && line.contains("RUNNING"))
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("No active monitor source found.");

    // Setting PULSE_SOURCE before cpal initializes directs PulseAudio to the monitor.
    unsafe {
        std::env::set_var("PULSE_SOURCE", monitor_name);
    }

    // Fetch all connected USB devices.
    let devices = rusb::devices().expect("Failed to get USB device list.");

    // Find the Skylanders Portal by vendor ID.
    let portal = devices.iter().find(|device| {
        device.device_descriptor()
            .ok()
            .map(|desc| desc.vendor_id() == PORTAL_VENDOR_ID)
            .unwrap_or(false)
    }).expect("Skylander's portal not found.");

    // Open a handle to communicate with the Portal.
    let handle = portal.open().expect("Failed to open portal.");

    // Detach the kernel driver if one is active.
    // NotFound just means none was attached.
    match handle.detach_kernel_driver(PORTAL_INTERFACE) {
        Ok(_) => {},
        Err(rusb::Error::NotFound) => {},
        Err(e) => panic!("Failed to detach kernel driver: {}", e),
    }

    // Claim the interface so we can write to it.
    handle.claim_interface(PORTAL_INTERFACE).expect("Failed to claim interface.");

    // Get the default audio host and find the pulse input device.
    let host = cpal::default_host();
    let mic = host.input_devices()
        .expect("Could not enumerate input devices.")
        .find(|device| {
            device.name()
                .map(|name| name == "pulse")
                .unwrap_or(false)
        })
        .expect("No pulse device found.");

    // Shared buffer for audio samples between the capture callback and main loop.
    let buffer: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));

    // Clone the Arc so the callback and main loop can each own a reference.
    let buffer_clone = Arc::clone(&buffer);

    // Get the default input config for the mic.
    let config = mic.default_input_config().expect("Failed to get default input config.");

    // Build the input stream, pushing samples into the shared buffer on each callback.
    let stream = mic.build_input_stream(
        &config.into(),
        move |data: &[i16], _| {
            buffer_clone.lock().unwrap().extend(data.iter());
        },
        |err| eprintln!("Stream error: {}", err),
        None,
    ).expect("Failed to build input stream.");

    // Start the audio capture stream.
    stream.play().expect("Failed to start stream.");

    // Drain frames of 1024 samples and print the peak amplitude to confirm audio is flowing.
    loop {
        let frame: Vec<i16> = {
            let mut buf = buffer.lock().unwrap();
            if buf.len() < FRAME_SIZE {
                continue;
            }
            buf.drain(..FRAME_SIZE).collect()
        };

        let peak = frame.iter().map(|s| s.abs()).max().unwrap_or(0);
        println!("Peak amplitude: {}", peak);
    }
}