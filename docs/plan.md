# Skylanders Music - Development Plan

## Project Goal
A Rust program that reads live microphone audio, analyzes it via FFT,
and drives the Skylanders Portal's RGB light in real time.

## Tech Stack
- `rusb` : USB communication with the Portal
- `cpal` : cross-platform audio capture
- `rustfft` : FFT for frequency analysis

## Phase 1 - USB Communication
- [x] Project created, pushed to GitHub
- [x] Find the Portal by vendor ID (0x1430) using `rusb::devices()`
- [x] Open the device handle and detach kernel driver on interface 0
- [x] Claim interface 0
- [x] Send a test RGB color via bulk write to endpoint 0x02
  - Payload: `[0x0B, 0x14, 0x43, r, g, b]`

## Phase 2 - Audio Capture
- [ ] Set up a `cpal` input stream at 44100 Hz, mono, i16
- [ ] Feed samples into a shared buffer (Mutex<VecDeque<i16>>)
- [ ] Consume 1024-sample frames from the main loop

## Phase 3 - FFT & Analysis
- [ ] Run `rustfft` rFFT on each 1024-sample frame
- [ ] Extract amplitudes from the complex output
- [ ] Track rolling max amplitude for normalization
- [ ] Compute mean frequency (weighted dot product) and mean brightness
- [ ] Apply exponential smoothing (factor ~0.2)

## Phase 4 - Color Mapping
- [ ] Build RGB gradient: red -> green -> blue -> red (19200 steps total)
- [ ] Map mean frequency to gradient index
- [ ] Scale color channels by brightness
- [ ] Send final color to Portal each frame