#![no_std]
#![no_main]

//! Combined mock HID-I2C device that presents as both a mouse and a keyboard over a single I2C endpoint.

use defmt::{info, warn};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_imxrt::i2c::slave::{Address, I2cSlave};
use embassy_imxrt::i2c::{self, Async};
use embassy_imxrt::{bind_interrupts, peripherals};
use panic_probe as _;
use rt685s_evk_example::mocks::keyboard::{KeyCode, MockKeyboardHidRelay, MockKeyboardService};
use rt685s_evk_example::mocks::mouse::{MockMouseHidRelay, MockMouseResources, MockMouseService};
use static_cell::StaticCell;

const SLAVE_ADDR: Option<Address> = Address::new(0x15);

// Compose the keyboard and mouse sub-devices into a single `HidDevice`. The macro generates a
// `MockCompositeHidRelay` type that owns both sub-devices, serves a combined report descriptor,
// and routes every report to the owning sub-device, including report ID translation.
//
embedded_services::impl_hid_aggregate_device!(MockCompositeHidRelay: MockKeyboardHidRelay<'static>, MockMouseHidRelay<'static>);

bind_interrupts!(struct Irqs {
    FLEXCOMM2 => i2c::InterruptHandler<peripherals::FLEXCOMM2>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_imxrt::init(Default::default());
    info!("HID-I2C mock mouse+keyboard example starting...");
    let i2c = I2cSlave::new_async(p.FLEXCOMM2, p.PIO0_18, p.PIO0_17, Irqs, SLAVE_ADDR.unwrap(), p.DMA0_CH4).unwrap();

    use embassy_imxrt::gpio;
    let attn_pin = gpio::Output::new(
        p.PIO0_28,
        gpio::Level::High,
        gpio::DriveMode::OpenDrain,
        gpio::DriveStrength::Normal,
        gpio::SlewRate::Standard,
    );

    // Set up the mouse sub-device's zero-copy channel.
    static MOUSE_RESOURCES: StaticCell<MockMouseResources> = StaticCell::new();
    let mouse_resources = MOUSE_RESOURCES.init(MockMouseResources::default());
    let (mouse_service, mut mouse_runner) = MockMouseService::new(mouse_resources);

    // Set up the keyboard sub-device's service.
    static KEYBOARD_SERVICE: StaticCell<MockKeyboardService> = StaticCell::new();
    let keyboard_service = KEYBOARD_SERVICE.init(MockKeyboardService::new());

    static COMPOSITE_RESOURCES: StaticCell<MockCompositeHidRelayResources> = StaticCell::new();
    let composite = MockCompositeHidRelay::new(
        COMPOSITE_RESOURCES.init(MockCompositeHidRelayResources::new()),
        MockKeyboardHidRelay::new(keyboard_service),
        MockMouseHidRelay::new(mouse_service),
    )
    .expect("Failed to combine HID report descriptors");

    let mut hidsvc = odp_service_common::spawn_service!(
        spawner,
        hidi2c_target_service::Service<
            'static,
            I2cSlave<'static, Async>,
            gpio::Output<'static>,
            MockCompositeHidRelay<'static>,
        >,
        |resources| hidi2c_target_service::Service::new(
            resources,
            i2c,
            attn_pin,
            composite,
            hidi2c_target_service::HardwareVersionInfo {
                vendor_id: hidi2c_target_service::VendorId::new(0x1234).unwrap(),
                product_id: hidi2c_target_service::ProductId(0x5678),
                version_id: hidi2c_target_service::VersionId(0x0001),
            },
            hidi2c_target_service::TimeoutSettings::default()
        )
    )
    .expect("Failed to spawn HID service");

    info!("Waiting 10s before starting to send inputs");
    embassy_time::Timer::after(embassy_time::Duration::from_secs(10)).await;

    let mut i = 0u32;
    loop {
        info!("clicking mouse");
        mouse_runner.send_click().await;
        embassy_time::Timer::after(embassy_time::Duration::from_millis(1000)).await;

        info!("pressing key");
        keyboard_service.click_key(KeyCode::NumLock).await;
        embassy_time::Timer::after(embassy_time::Duration::from_millis(1000)).await;

        i += 1;
        if i.is_multiple_of(5) {
            warn!("Manually triggering reset of HID service after 5 rounds to test reset handling");
            hidsvc.reset();
        }
    }
}
