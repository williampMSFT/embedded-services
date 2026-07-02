#![no_std]
#![no_main]
#![warn(warnings)] // TODO remove before checkin

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_imxrt::i2c::slave::{Address, I2cSlave};
use embassy_imxrt::i2c::{self, Async};
use embassy_imxrt::{bind_interrupts, peripherals};
use static_cell::StaticCell;
use panic_probe as _;
use zerocopy::{IntoBytes, FromBytes};
use embedded_services::relay::hid::*;

use embedded_services::warn;

const SLAVE_ADDR: Option<Address> = Address::new(0x15);

// This is adapted from the example keyboard HID descriptor packaged with the DT.exe tool / https://learn.microsoft.com/en-us/windows-hardware/design/component-guidelines/keyboard-collection-report-descriptor
const REPORTID_KEYBOARD: u8 = 0; // If we don't specify a report ID in our descriptor, the transport service will use report ID 0, which is normally not a valid report ID.
const KEYBOARD_HID_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01,                    // USAGE_PAGE (Generic Desktop)
    0x09, 0x06,                    // USAGE (Keyboard)
    0xa1, 0x01,                    // COLLECTION (Application)
    // 0x85, REPORTID_KEYBOARD,            //   REPORT_ID (keyboard) // Enable this if we need to support more than one report of any type; if you do, set REPORTID_KEYBOARD to be nonzero.
    0x05, 0x07,                    //   USAGE_PAGE (Keyboard)
    0x19, 0xe0,                    //   USAGE_MINIMUM (Keyboard LeftControl)
    0x29, 0xe7,                    //   USAGE_MAXIMUM (Keyboard Right GUI)
    0x15, 0x00,                    //   LOGICAL_MINIMUM (0)
    0x25, 0x01,                    //   LOGICAL_MAXIMUM (1)
    0x75, 0x01,                    //   REPORT_SIZE (1)
    0x95, 0x08,                    //   REPORT_COUNT (8)
    0x81, 0x02,                    //   INPUT (Data,Var,Abs)
    0x95, 0x01,                    //   REPORT_COUNT (1)
    0x75, 0x08,                    //   REPORT_SIZE (8)
    0x81, 0x03,                    //   INPUT (Cnst,Var,Abs)
    0x95, 0x05,                    //   REPORT_COUNT (5)
    0x75, 0x01,                    //   REPORT_SIZE (1)
    0x05, 0x08,                    //   USAGE_PAGE (LEDs)
    0x19, 0x01,                    //   USAGE_MINIMUM (Num Lock)
    0x29, 0x05,                    //   USAGE_MAXIMUM (Kana)
    0x91, 0x02,                    //   OUTPUT (Data,Var,Abs)
    0x95, 0x01,                    //   REPORT_COUNT (1)
    0x75, 0x03,                    //   REPORT_SIZE (3)
    0x91, 0x03,                    //   OUTPUT (Cnst,Var,Abs)
    0x95, 0x06,                    //   REPORT_COUNT (6)
    0x75, 0x08,                    //   REPORT_SIZE (8)
    0x15, 0x00,                    //   LOGICAL_MINIMUM (0)
    0x25, 0x65,                    //   LOGICAL_MAXIMUM (101)
    0x05, 0x07,                    //   USAGE_PAGE (Keyboard)
    0x19, 0x00,                    //   USAGE_MINIMUM (Reserved (no event indicated))
    0x29, 0x65,                    //   USAGE_MAXIMUM (Keyboard Application)
    0x81, 0x00,                    //   INPUT (Data,Ary,Abs)
    0xc0                           // END_COLLECTION
];

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default, defmt::Format, zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable)]
pub struct KeyboardInputReport {
    /// Left Ctrl .. Right GUI
    pub modifiers: u8,

    /// Reserved byte required by boot keyboard format
    pub reserved: u8,

    /// Up to 6 simultaneous key usages
    pub keys: [u8; 6],
}

#[allow(dead_code)]
#[repr(u8)]
enum KeyCode {
    NumLock = 0x53,
    A = 0x04,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, defmt::Format, zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable)]
pub struct KeyboardOutputReport {
    /// LED state bits
    pub leds: u8,
}

struct MockKeyboardService {
    // Signal a click
    channel: embassy_sync::channel::Channel<embedded_services::GlobalRawMutex, KeyboardInputReport, 5>,
}

impl MockKeyboardService {
    pub async fn click_key(&self, key_code: u8) {
        // key down
        let send_result = self.channel.try_send(KeyboardInputReport {
            modifiers: 0,
            reserved: 0,
            keys: [key_code, 0, 0, 0, 0, 0],
        });

        if let Err(e) = send_result {
            warn!("Failed to send key down report: {:?}", e);
        }

        embassy_time::Timer::after(embassy_time::Duration::from_millis(15)).await;

        // key up
        let send_result = self.channel.try_send(KeyboardInputReport::default());
        if let Err(e) = send_result {
            warn!("Failed to send key up report: {:?}", e);
        }
    }

    pub fn receiver(&self) -> embassy_sync::channel::Receiver<'_, embedded_services::GlobalRawMutex, KeyboardInputReport, 5> {
        self.channel.receiver()
    }
}

// TODO if this pattern is going to be common, maybe write a generic struct to do it
struct KeyboardNotificationHidReceiver<'a> {
    receiver: embassy_sync::channel::Receiver<'a, embedded_services::GlobalRawMutex, KeyboardInputReport, 5>,
}

impl<'a, MaxSize: generic_array::ArrayLength> ReportReceiver<MaxSize> for KeyboardNotificationHidReceiver<'a> {
    async fn ready_to_receive(&self) {
        self.receiver.ready_to_receive().await
    }

    async fn receive(&self) -> Result<HidReport<MaxSize>, HidError> {
        let report = self.receiver.receive().await;
        let hid_report = HidReport::new(ReportId(REPORTID_KEYBOARD), report.as_bytes()).unwrap();
        Ok(hid_report)
    }

    fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

struct MockKeyboardHidRelay<'s> {
    service: &'s MockKeyboardService,
    descriptor: HidReportDescriptor,
}

impl<'s> MockKeyboardHidRelay<'s> {
    pub fn new(service: &'s MockKeyboardService) -> Self {
        Self {
            service,
            descriptor: HidReportDescriptor::new_static(KEYBOARD_HID_REPORT_DESCRIPTOR),
        }
    }
}

impl embedded_services::relay::hid::HidDevice for MockKeyboardHidRelay<'_> {
    type InputReportMaxSize = typenum::U8;
    type OutputReportMaxSize = typenum::U1;
    type FeatureReportMaxSize = typenum::U0;

    /// The type that will surface HID reports as they become available.
    type ReportReceiver<'a> = KeyboardNotificationHidReceiver<'a > where Self: 'a;

    const MAX_REPORT_COUNT: u8 = 2;

    fn report_descriptor(&self) -> &HidReportDescriptor {
        &self.descriptor
    }

    async fn get_report(
        &mut self,
        _report_type: GetHidReportType,
        report_id: ReportId,
    ) -> Result<GetHidReport<Self::InputReportMaxSize, Self::FeatureReportMaxSize>, HidError> {
        info!("Received command to get report with ID {:?}", report_id);
        match report_id {
            ReportId(REPORTID_KEYBOARD) => {
                let report = KeyboardInputReport::default();
                Ok(GetHidReport::Input(HidReport::<Self::InputReportMaxSize>::new(
                    report_id,
                    report.as_bytes()
                ).unwrap()))
            }
            _ => {
                info!("Report ID {:?} not recognized", report_id);
                Err(HidError::TriggerReset)
            }
        }
    }

    async fn set_report(
        &mut self,
        report: &SetHidReport<Self::OutputReportMaxSize, Self::FeatureReportMaxSize>,
    ) -> Result<(), HidError> {
        match report {
            SetHidReport::Output(r) => {
                match r.id() {
                    ReportId(REPORTID_KEYBOARD) => {
                        let output_report = KeyboardOutputReport::read_from_bytes(r.data()).unwrap();
                        info!("Received keyboard output report: {:?}", output_report);
                    }
                    _ => {
                        info!("Report ID {:?} not recognized", r.id());
                        return Err(HidError::TriggerReset);
                    }
                }
            },
            SetHidReport::Feature(r) => info!("Received command to set feature report with ID {:?}", r.id()),
        }
        Ok(())
    }

    fn receiver(&mut self) -> Self::ReportReceiver<'_> {
        KeyboardNotificationHidReceiver{receiver: self.service.receiver()}
    }

    async fn set_power_state(&mut self, state: HidDevicePowerState) -> Result<(), HidError> {
        info!("Received command to set power state to {:?}", state);
        Ok(())
    }

    async fn reset(&mut self) {
        info!("Received reset command");
        self.service.receiver().clear();
    }
}


bind_interrupts!(struct Irqs {
    FLEXCOMM2 => i2c::InterruptHandler<peripherals::FLEXCOMM2>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_imxrt::init(Default::default());
    info!("HID-I2C mock mouse example starting...");
    // TODO @Felipe if I specify the slave address here, why do I also receive it as an argument in the trait?
    let i2c = I2cSlave::new_async(p.FLEXCOMM2, p.PIO0_18, p.PIO0_17, Irqs, SLAVE_ADDR.unwrap(), p.DMA0_CH4).unwrap();

    // GPIO on P0_28.
    use embassy_imxrt::gpio;
    let attn_pin = gpio::Output::new(
        p.PIO0_28,
        gpio::Level::High,
        gpio::DriveMode::OpenDrain,
        gpio::DriveStrength::Normal,
        gpio::SlewRate::Standard,
    );

    static KEYBOARD_SERVICE: StaticCell<MockKeyboardService> = StaticCell::new();
    let keyboard_service = KEYBOARD_SERVICE.init(MockKeyboardService {
        channel: embassy_sync::channel::Channel::new(),
    });

    // NOTE: here's where the "aggregate HID devices" macro is currently missing.  Compare with time_alarm.rs where we do this:
    //
    //     impl_odp_mctp_relay_handler!(
    //         EspiRelayHandler;
    //         TimeAlarm, 0x0B, crate::TimeAlarmServiceRelayHandlerType;
    //     );
    //
    //  We will eventually write a macro that looks something like this:
    //
    //     impl_hid_aggregate_device!(
    //         MyAggregateDevice;
    //         time_alarm_service_relay::hid::TimeAlarmHidRelay,
    //         battery_service_relay::hid::BatteryHidRelay,
    //         ...
    //     );
    //
    // which will emit a type MyAggregateDevice that implements HidDevice and takes as construction parameters one instance
    // of each of the handlers, in the order they were specified.
    //
    // For a concrete example of this pattern, see what we're doing with MCTP here: https://github.com/OpenDevicePartnership/odp-embedded-controller/blob/d6fb3ce5d9ae52ca51d6ef7b87518c6c4cb3c809/platform/platform-common/src/lib.rs#L20
    //
    // This depends on writing the 'hid support library', though, so for now we're just going to directly use the time-alarm device for testing
    // (pending getting actual hardware to test on, implementing I2C traits for embassy, etc).

    let mut hidsvc = odp_service_common::spawn_service!(
        spawner,
        hidi2c_target_service::Service<'static, I2cSlave<'static, Async>, gpio::Output<'static>, MockKeyboardHidRelay<'static>>,
        |resources| hidi2c_target_service::Service::new(
            resources,
            i2c,
            attn_pin,
            MockKeyboardHidRelay::new(keyboard_service),
            hidi2c_target_service::HardwareVersionInfo {
                vendor_id: hidi2c_target_service::VendorId::new(0x1234).unwrap(), // TODO pick a real vendor ID
                product_id: hidi2c_target_service::ProductId(0x5678), // TODO pick a real product ID
                version_id: hidi2c_target_service::VersionId(0x0001), // TODO pick a real version number
            },
            hidi2c_target_service::TimeoutSettings::default()
        )
    ).expect("Failed to spawn HID service");

    info!("Waiting 10s before starting to send inputs");
    embassy_time::Timer::after(embassy_time::Duration::from_secs(10)).await;

    let mut i = 0;
    loop {
        info!("pressing key");
        keyboard_service.click_key(KeyCode::NumLock as u8).await;
        embassy_time::Timer::after(embassy_time::Duration::from_millis(2000)).await;
        i += 1;
        if i % 5 == 0 {
            defmt::warn!("Manually triggering reset of HID service after 5 clicks to test reset handling");
            hidsvc.reset();
        }
    }
}



