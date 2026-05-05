use rusb::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};
use std::collections::VecDeque;
use std::thread;
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

// How long to sleep when the buffer does not yet hold a full frame.
// ~1ms keeps CPU usage low while still reacting within two frame-lengths
// at 44100 Hz (FRAME_SIZE / 44100 ≈ 23 ms per frame).
const BUFFER_POLL_SLEEP_US: u64 = 1_000;

// Fractional decay applied to max_amplitude each frame when the current signal
// is below the tracked peak.  A value of 0.995 lets the ceiling fall ~30% over
// ~200 frames (~5 seconds), keeping normalization tight after loud transients
// without causing visible flicker.  Raise toward 1.0 for a slower release;
// lower toward 0.99 for a faster one.
const MAX_AMPLITUDE_DECAY: f32 = 0.995;

// Absolute floor for max_amplitude.  Prevents the ceiling from decaying to zero
// during silence, which would produce a divide-by-zero and make noise-floor
// artefacts appear full-brightness.
const MAX_AMPLITUDE_FLOOR: f32 = 1.0;

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

    // Pre-allocate the FFT scratch buffer once and reuse it every frame,
    // avoiding a heap allocation inside fft.process() on each iteration.
    let mut scratch = vec![Complex { re: 0.0f32, im: 0.0f32 }; fft.get_inplace_scratch_len()];

    // Build a looping RGB gradient: red to green, green to blue, blue to red.
    let mut colors: Vec<[f32; 3]> = Vec::new();
    colors.extend(make_gradient([255.0, 0.0, 0.0], [0.0, 254.0, 0.0], GRADIENT_STEPS));
    colors.extend(make_gradient([0.0, 255.0, 0.0], [0.0, 0.0, 254.0], GRADIENT_STEPS));
    colors.extend(make_gradient([0.0, 0.0, 255.0], [255.0, 0.0, 0.0], GRADIENT_STEPS));

    // Each portal tracks its own smoothing state independently.
    let mut states: Vec<PortalState> = handles.iter().map(|_| PortalState::new()).collect();

    // Divide the frequency range evenly across however many portals are connected.
    let freq_ranges = make_freq_ranges(handles.len());

    // Pre-allocate the FFT input buffer and a scratch amplitudes buffer to avoid
    // per-frame heap allocations inside the hot loop.
    let mut fft_input: Vec<Complex<f32>> = vec![Complex { re: 0.0, im: 0.0 }; FRAME_SIZE];
    let mut amplitudes: Vec<f32> = vec![0.0f32; FREQ_UPPER - FREQ_LOWER];

    loop {
        // Wait until enough samples are available in the buffer.
        // Sleep briefly instead of busy-spinning to avoid starving the audio callback
        // and wasting CPU cycles on mutex contention.
        let frame: Vec<i16> = loop {
            let mut buf = buffer.lock().unwrap();
            if buf.len() >= FRAME_SIZE {
                break buf.drain(..FRAME_SIZE).collect();
            }
            // Drop the lock before sleeping so the audio callback is not blocked.
            drop(buf);
            thread::sleep(std::time::Duration::from_micros(BUFFER_POLL_SLEEP_US));
        };

        // Convert samples into the pre-allocated FFT buffer in place,
        // avoiding a Vec allocation on every frame.
        for (dst, src) in fft_input.iter_mut().zip(frame.iter()) {
            dst.re = *src as f32;
            dst.im = 0.0;
        }

        // Run the FFT in place, reusing the pre-allocated scratch buffer.
        fft.process_with_scratch(&mut fft_input, &mut scratch);

        // Build per-portal color payloads so we can dispatch all USB writes in
        // parallel threads, preventing each portal's write_bulk timeout from
        // stacking additively and introducing N×timeout lag.
        let mut payloads: Vec<[u8; 6]> = Vec::with_capacity(handles.len());

        for (i, (freq_start, freq_end)) in freq_ranges.iter().enumerate() {
            let amp_length = freq_end - freq_start;
            let state = &mut states[i];

            // Compute amplitudes into the pre-allocated slice for this band,
            // avoiding an allocation per portal per frame.
            let amp_slice = &mut amplitudes[..*freq_end - *freq_start];
            for (j, c) in fft_input[*freq_start..*freq_end].iter().enumerate() {
                amp_slice[j] = c.norm();
            }

            let current_max = amp_slice.iter().cloned().fold(0.0f32, f32::max);

            if current_max > 0.0 {
                // Decay the amplitude ceiling each frame so it tracks the signal
                // dynamically.  When a loud transient passes, the ceiling drifts
                // back down toward the current level, restoring full brightness
                // range rather than permanently dimming after a peak.  A floor of
                // MAX_AMPLITUDE_FLOOR prevents the ceiling from collapsing to zero
                // during silence, which would amplify noise-floor artefacts.
                state.max_amplitude = (state.max_amplitude * MAX_AMPLITUDE_DECAY)
                    .max(MAX_AMPLITUDE_FLOOR);

                // Grow the ceiling instantly on any new peak so normalization
                // never clips above 1.0.
                if current_max > state.max_amplitude {
                    state.max_amplitude = current_max;
                }

                // Smooth brightness toward the current normalized peak.
                state.mean_brightness -= (state.mean_brightness - (current_max / state.max_amplitude)) * SMOOTHNESS;

                // Compute the amplitude-weighted mean frequency bin within this band and smooth it.
                let weighted_freq: f32 = (0..amp_length)
                    .map(|j| j as f32 * (amp_slice[j] / state.max_amplitude))
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

            payloads.push([COLOR_COMMAND[0], COLOR_COMMAND[1], COLOR_COMMAND[2], r, g, b]);
        }

        // Send all portal color commands in parallel.
        // Each write_bulk call can block up to its timeout; doing them concurrently
        // keeps total write time bounded by the slowest single portal rather than
        // the sum of all portal timeouts.
        thread::scope(|s| {
            for (handle, payload) in handles.iter().zip(payloads.iter()) {
                s.spawn(|| {
                    handle.write_bulk(PORTAL_ENDPOINT, payload, std::time::Duration::from_millis(10))
                        .expect("Failed to write color.");
                });
            }
        });
    }
}