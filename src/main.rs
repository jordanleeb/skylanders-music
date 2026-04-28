use rusb::*;

// Skylanders Portal USB vendor ID.
const PORTAL_VENDOR_ID: u16 = 0x1430;

// Portal USB interface and endpoint.
const PORTAL_INTERFACE: u8 = 0;
const PORTAL_ENDPOINT: u8 = 0x02;

// Portal color command header bytes.
const COLOR_COMMAND: [u8; 3] = [0x0B, 0x14, 0x43];

fn main() {
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

    // Send a red color to the Portal.
    let (r, g, b) = (0xFF, 0x00, 0x00);
    let payload = [COLOR_COMMAND[0], COLOR_COMMAND[1], COLOR_COMMAND[2], r, g, b];
    handle.write_bulk(PORTAL_ENDPOINT, &payload, std::time::Duration::from_secs(1))
        .expect("Failed to write color.");
}