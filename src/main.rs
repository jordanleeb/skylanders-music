use rusb::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};
use std::collections::VecDeque;
use rustfft::{FftPlanner, num_complex::Complex};

// Skylanders Portal USB vendor ID.
const PORTAL_VENDOR_ID: u16 = 0x1430;

// Portal USB interface and endpoint.
const PORTAL_INTERFACE: u8 = 0;
const PORTAL_ENDPOINT: u8 = 0x02;

// Portal color command header bytes.
const COLOR_COMMAND: [u8; 3] = [0x0B, 0x14, 0x43];

// Number of samples per FFT frame.
const FRAME_SIZE: usize = 1024;

// Frequency bin range to analyze.
const FREQ_LOWER: usize = 0;
const FREQ_UPPER: usize = 128;

// Exponential smoothing factor for brightness and frequency.
const SMOOTHNESS: f32 = 0.2;

// Multiplier applied to brightness before sending to the Portal.
const BRIGHTNESS_MULTIPLIER: f32 = 1.0;

// Number of steps per gradient segment.
const GRADIENT_STEPS: usize = 6400;

// Builds a linear RGB gradient between two colors over n steps.
fn make_gradient(start: [f32; 3], end: [f32; 3], n: usize) -> Vec<[f32; 3]> {
    (0..n).map(|i| {
        let t = i as f32 / (n - 1) as f32;
        [
            start[0] + (end[0] - start[0]) * t,
            start[1] + (end[1] - start[1]) * t,
            start[2] + (end[2] - start[2]) * t,
        ]
    }).collect()
}

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

    // Build the FFT planner and plan a forward FFT of size FRAME_SIZE.
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(FRAME_SIZE);

    // Build a looping RGB gradient: red to green, green to blue, blue to red.
    let mut colors: Vec<[f32; 3]> = Vec::new();
    colors.extend(make_gradient([255.0, 0.0, 0.0], [0.0, 254.0, 0.0], GRADIENT_STEPS));
    colors.extend(make_gradient([0.0, 255.0, 0.0], [0.0, 0.0, 254.0], GRADIENT_STEPS));
    colors.extend(make_gradient([0.0, 0.0, 255.0], [255.0, 0.0, 0.0], GRADIENT_STEPS));

    let amp_length = FREQ_UPPER - FREQ_LOWER;
    let mut mean_brightness: f32 = 0.0;
    let mut mean_frequency: f32 = 0.0;
    let mut max_amplitude: f32 = 0.0;

    loop {
        // Wait until enough samples are available in the buffer.
        let frame: Vec<i16> = {
            let mut buf = buffer.lock().unwrap();
            if buf.len() < FRAME_SIZE {
                continue;
            }
            buf.drain(..FRAME_SIZE).collect()
        };

        // Convert samples to complex numbers for FFT input.
        let mut fft_input: Vec<Complex<f32>> = frame.iter()
            .map(|s| Complex { re: *s as f32, im: 0.0 })
            .collect();

        // Run the FFT in place.
        fft.process(&mut fft_input);

        // Extract amplitudes from the frequency bin range of interest.
        let amplitudes: Vec<f32> = fft_input[FREQ_LOWER..FREQ_UPPER]
            .iter()
            .map(|c| c.norm())
            .collect();

        let current_max = amplitudes.iter().cloned().fold(0.0f32, f32::max);

        if current_max > 0.0 {
            // Track the rolling maximum amplitude for normalization.
            if current_max > max_amplitude {
                max_amplitude = current_max;
            }

            let normalized: Vec<f32> = amplitudes.iter()
                .map(|a| a / max_amplitude)
                .collect();

            // Smooth brightness toward the current normalized peak.
            mean_brightness -= (mean_brightness - (current_max / max_amplitude)) * SMOOTHNESS;

            // Compute the amplitude-weighted mean frequency bin and smooth it.
            let weighted_freq: f32 = (0..amp_length)
                .map(|i| i as f32 * normalized[i])
                .sum::<f32>() % amp_length as f32;

            mean_frequency -= (mean_frequency - weighted_freq) * SMOOTHNESS;
        } else {
            // Decay brightness toward zero when there is no signal.
            mean_brightness -= mean_brightness * SMOOTHNESS;
        }

        // Map mean frequency to a color in the gradient.
        let color_index = ((mean_frequency / amp_length as f32) * (colors.len() - 1) as f32)
            .round() as usize;
        let color = colors[color_index];

        // Scale the color by brightness and clamp to valid byte range.
        let r = (color[0] * mean_brightness * BRIGHTNESS_MULTIPLIER).min(255.0).round() as u8;
        let g = (color[1] * mean_brightness * BRIGHTNESS_MULTIPLIER).min(255.0).round() as u8;
        let b = (color[2] * mean_brightness * BRIGHTNESS_MULTIPLIER).min(255.0).round() as u8;

        // Send the color to the Portal.
        let payload = [COLOR_COMMAND[0], COLOR_COMMAND[1], COLOR_COMMAND[2], r, g, b];
        handle.write_bulk(PORTAL_ENDPOINT, &payload, std::time::Duration::from_millis(10))
            .expect("Failed to write color.");
    }
}