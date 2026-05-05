# Skylanders Music - Development Plan

## Project Goal
A Rust program that reads live microphone audio, analyzes it via FFT,
and drives one or more Skylanders Portal RGB lights in real time,
with each portal reacting to its own slice of the frequency spectrum.

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
- [x] Set up a `cpal` input stream at 44100 Hz, mono, i16
- [x] Feed samples into a shared buffer (Mutex<VecDeque<i16>>)
- [x] Consume 1024-sample frames from the main loop

## Phase 3 - FFT & Analysis
- [x] Run `rustfft` rFFT on each 1024-sample frame
- [x] Extract amplitudes from the complex output
- [x] Track rolling max amplitude for normalization
- [x] Compute mean frequency (weighted dot product) and mean brightness
- [x] Apply exponential smoothing (factor ~0.2)

## Phase 4 - Color Mapping
- [x] Build RGB gradient: red -> green -> blue -> red (19200 steps total)
- [x] Map mean frequency to gradient index
- [x] Scale color channels by brightness
- [x] Send final color to Portal each frame

## Phase 5 - Multi-Portal Support
- [x] Collect all portals matching vendor ID 0x1430 (filter instead of find)
- [x] Assert at least 1 portal is connected at startup; print the count found
- [x] Open, detach, and claim each portal handle in a loop
- [x] Introduce `PortalState` struct to track independent smoothing state per portal
  - Fields: `mean_brightness`, `mean_frequency`, `max_amplitude`
- [x] Add `make_freq_ranges(n)` to divide `FREQ_LOWER..FREQ_UPPER` into n
      contiguous bands; the final band absorbs any remainder so no bins are dropped
- [x] Run one FFT per frame, then iterate portals to compute and send each color
  - 1 portal: full frequency range, behavior identical to Phase 4
  - 2 portals: low half and high half
  - N portals: spectrum divided into N roughly equal bands