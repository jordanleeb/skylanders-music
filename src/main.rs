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

// Full frequency bin range to analyze across all portals.
const FREQ_LOWER: usize = 0;
const FREQ_UPPER: usize = 128;

// Exponential smoothing factor for brightness and frequency.
const SMOOTHNESS: f32 = 0.2;

// Multiplier applied to brightness before sending to a portal.
const BRIGHTNESS_MULTIPLIER: f32 = 1.0;

// Number of steps per gradient segment.
const GRADIENT_STEPS: usize = 6400;

// Per-portal smoothing state, kept independent so each reacts to its own frequency band.
struct PortalState {
    mean_brightness: f32,
    mean_frequency: f32,
    max_amplitude: f32,
}

impl PortalState {
    fn new() -> Self {
        Self {
            mean_brightness: 0.0,
            mean_frequency: 0.0,
            max_amplitude: 0.0,
        }
    }
}

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

// Divides FREQ_LOWER..FREQ_UPPER into n contiguous, non-overlapping bands.
// The final band absorbs any remainder so no bins are dropped.
fn make_freq_ranges(n: usize) -> Vec<(usize, usize)> {
    let total = FREQ_UPPER - FREQ_LOWER;
    let band = total / n;
    (0..n).map(|i| {
        let start = FREQ_LOWER + i * band;
        // The last portal takes any remainder bins so none are dropped.
        let end = if i == n - 1 { FREQ_UPPER } else { start + band };
        (start, end)
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

    // Collect all Skylanders Portals by vendor ID.
    let portals: Vec<_> = devices.iter().filter(|device| {
        device.device_descriptor()
            .ok()
            .map(|desc| desc.vendor_id() == PORTAL_VENDOR_ID)
            .unwrap_or(false)
    }).collect();

    assert!(!portals.is_empty(), "No Skylanders portals found.");

    println!("Found {} portal(s).", portals.len());

    // Open a handle for each portal, detach any kernel driver, and claim the interface.
    let handles: Vec<DeviceHandle<GlobalContext>> = portals.iter().map(|portal| {
        let handle = portal.open().expect("Failed to open portal.");
        match handle.detach_kernel_driver(PORTAL_INTERFACE) {
            Ok(_) => {},
            Err(rusb::Error::NotFound) => {},
            Err(e) => panic!("Failed to detach kernel driver: {}", e),
        }
        handle.claim_interface(PORTAL_INTERFACE).expect("Failed to claim interface.");
        handle
    }).collect();

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

    // Each portal tracks its own smoothing state independently.
    let mut states: Vec<PortalState> = handles.iter().map(|_| PortalState::new()).collect();

    // Divide the frequency range evenly across however many portals are connected.
    let freq_ranges = make_freq_ranges(handles.len());

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

        // Update each portal using its assigned frequency band.
        for (i, (freq_start, freq_end)) in freq_ranges.iter().enumerate() {
            let amp_length = freq_end - freq_start;
            let state = &mut states[i];

            // Extract amplitudes from this portal's frequency band.
            let amplitudes: Vec<f32> = fft_input[*freq_start..*freq_end]
                .iter()
                .map(|c| c.norm())
                .collect();

            let current_max = amplitudes.iter().cloned().fold(0.0f32, f32::max);

            if current_max > 0.0 {
                // Track the rolling maximum amplitude for normalization.
                if current_max > state.max_amplitude {
                    state.max_amplitude = current_max;
                }

                let normalized: Vec<f32> = amplitudes.iter()
                    .map(|a| a / state.max_amplitude)
                    .collect();

                // Smooth brightness toward the current normalized peak.
                state.mean_brightness -= (state.mean_brightness - (current_max / state.max_amplitude)) * SMOOTHNESS;

                // Compute the amplitude-weighted mean frequency bin within this band and smooth it.
                let weighted_freq: f32 = (0..amp_length)
                    .map(|j| j as f32 * normalized[j])
                    .sum::<f32>() % amp_length as f32;

                state.mean_frequency -= (state.mean_frequency - weighted_freq) * SMOOTHNESS;
            } else {
                // Decay brightness toward zero when there is no signal in this band.
                state.mean_brightness -= state.mean_brightness * SMOOTHNESS;
            }

            // Map the mean frequency within this band to a color in the gradient.
            let color_index = ((state.mean_frequency / amp_length as f32) * (colors.len() - 1) as f32)
                .round() as usize;
            let color = colors[color_index];

            // Scale the color by brightness and clamp to valid byte range.
            let r = (color[0] * state.mean_brightness * BRIGHTNESS_MULTIPLIER).min(255.0).round() as u8;
            let g = (color[1] * state.mean_brightness * BRIGHTNESS_MULTIPLIER).min(255.0).round() as u8;
            let b = (color[2] * state.mean_brightness * BRIGHTNESS_MULTIPLIER).min(255.0).round() as u8;

            // Send the color to this portal.
            let payload = [COLOR_COMMAND[0], COLOR_COMMAND[1], COLOR_COMMAND[2], r, g, b];
            handles[i].write_bulk(PORTAL_ENDPOINT, &payload, std::time::Duration::from_millis(10))
                .expect("Failed to write color.");
        }
    }
}