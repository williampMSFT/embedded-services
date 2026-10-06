#![no_std]
#![no_main]

//! Combined mock HID-I2C device that presents as both a mouse and an example vendor device ("CTF device") over a single I2C endpoint.

use defmt::{error, info};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_imxrt::i2c::slave::{Address, I2cSlave};
use embassy_imxrt::i2c::{self, Async};
use embassy_imxrt::{bind_interrupts, peripherals};
use panic_probe as _;

use rt685s_evk_example::mocks::mouse::{
    device::{MockMouseService, MockMouseServiceResources},
    relay::MockMouseHidRelay,
};
use static_cell::StaticCell;

use embedded_services::relay::hid::*;

const SLAVE_ADDR: Option<Address> = Address::new(0x15);

// Number of subscriber slots each sub-device's pubsub channel exposes. Both sub-devices are driven
// by a single relay, so one subscriber each is sufficient.
// const KB_SUBS: usize = 1;
const MOUSE_SUBS: usize = 1;

struct CtfDevice {
    descriptor: HidReportDescriptor<'static>,
}

impl CtfDevice {
    fn new() -> Self {
        Self {
            descriptor: HidReportDescriptor::new(CTF_DESCRIPTOR).unwrap(),
        }
    }
}

#[rustfmt::skip]
const CTF_DESCRIPTOR: &[u8] = &[
    0x06, 0xBA, 0xFF,                // UsagePage(Vendor CTF Device[0xFFBA])
    0x09, 0xD0,                      // UsageId(CTF Management Interface[0x00D0])
    0xA1, 0x01,                      // Collection(Application)
    0x85, 0x01,                      //     ReportId(1)
    0x09, 0x01,                      //     UsageId(Flag Report[0x0001])
    0x15, 0x00,                      //     LogicalMinimum(0)
    0x27, 0xFF, 0xFF, 0xFF, 0x7F,    //     LogicalMaximum(2,147,483,647)
    0x95, 0x01,                      //     ReportCount(1)
    0x75, 0x1F,                      //     ReportSize(31)
    0x91, 0x02,                      //     Output(Data, Variable, Absolute, NoWrap, Linear, PreferredState, NoNullPosition, NonVolatile, BitField)
    0x75, 0x01,                      //     ReportSize(1)
    0x91, 0x03,                      //     Output(Constant, Variable, Absolute, NoWrap, Linear, PreferredState, NoNullPosition, NonVolatile, BitField)
    0xC0,                            // EndCollection()
];

impl embedded_services::relay::hid::HidDevice for CtfDevice {
    type InputReportMaxSize = typenum::U0;
    type OutputReportMaxSize = typenum::U4;
    type FeatureReportMaxSize = typenum::U0;
    const MAX_REPORT_COUNT: u8 = 1;
    const MAX_DESCRIPTOR_LEN: usize = CTF_DESCRIPTOR.len();

    fn report_descriptor(&self) -> &HidReportDescriptor<'_> {
        &self.descriptor
    }

    async fn process_get_report<R>(
        &mut self,
        _report_type: GetHidReportType,
        _report_id: ReportId,
        _process_report: impl AsyncFnOnce(GetHidReport<'_>) -> R,
    ) -> Result<R, HidError> {
        todo!()
    }

    async fn set_report(&mut self, report: &SetHidReport<'_>) -> Result<(), HidError> {
        error!(
            "!!!!!!!! RECEIVED REQUEST TO SET REPORT !!!!!!!!!!: value {:?}",
            report.data()
        );
        Ok(())
    }

    async fn wait_for_input_report(&mut self) {
        core::future::pending::<()>().await;
    }

    fn has_pending_input_report(&mut self) -> bool {
        false
    }

    async fn process_next_input_report<R>(
        &mut self,
        _process_report: impl AsyncFnOnce(HidReport<'_>) -> R,
    ) -> Result<R, HidError> {
        todo!() // we don't generate input reports
    }

    async fn set_power_state(&mut self, state: HidDevicePowerState) -> Result<(), HidError> {
        info!("CTF: Setting power state to {:?}", state);
        Ok(())
    }

    async fn reset(&mut self) {
        info!("Resetting CTF device");
    }
}

// Compose the keyboard and mouse sub-devices into a single `HidDevice`. The macro generates a
// `MockCompositeHidRelay` type that owns both sub-devices, serves a combined report descriptor,
// and routes every report to the owning sub-device, including report ID translation.
//
embedded_services::impl_hid_aggregate_device!(MockCompositeHidRelay: CtfDevice, MockMouseHidRelay<'static, MockMouseService<'static, MOUSE_SUBS>>);

bind_interrupts!(struct Irqs {
    FLEXCOMM2 => i2c::InterruptHandler<peripherals::FLEXCOMM2>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_imxrt::init(Default::default());
    info!("HID-I2C CTF example starting...");
    let i2c = I2cSlave::new_async(p.FLEXCOMM2, p.PIO0_18, p.PIO0_17, Irqs, SLAVE_ADDR.unwrap(), p.DMA0_CH4).unwrap();

    use embassy_imxrt::gpio;
    let attn_pin = gpio::Output::new(
        p.PIO0_28,
        gpio::Level::High,
        gpio::DriveMode::OpenDrain,
        gpio::DriveStrength::Normal,
        gpio::SlewRate::Standard,
    );

    // Set up the mouse sub-device's pubsub channel.
    static MOUSE_RESOURCES: StaticCell<MockMouseServiceResources<MOUSE_SUBS>> = StaticCell::new();
    let (mouse_service, mouse_runner) = MockMouseService::new(MOUSE_RESOURCES.init(Default::default()));
    static MOUSE_SERVICE: StaticCell<MockMouseService<'static, MOUSE_SUBS>> = StaticCell::new();

    static COMPOSITE_RESOURCES: StaticCell<MockCompositeHidRelayResources> = StaticCell::new();
    let composite = MockCompositeHidRelay::new(
        COMPOSITE_RESOURCES.init(MockCompositeHidRelayResources::new()),
        CtfDevice::new(),
        MockMouseHidRelay::new(MOUSE_SERVICE.init(mouse_service)),
    )
    .expect("Failed to combine HID report descriptors");

    let _hidsvc = odp_service_common::spawn_service!(
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

    loop {
        info!("clicking mouse");
        mouse_runner.send_click().await;
        embassy_time::Timer::after(embassy_time::Duration::from_millis(5000)).await;
    }
}
