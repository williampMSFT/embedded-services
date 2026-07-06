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
use zerocopy::IntoBytes;
use embedded_services::relay::hid::*;

use embedded_services::warn;

const SLAVE_ADDR: Option<Address> = Address::new(0x15);

// This is adapted from the example mouse HID descriptor packaged with the DT.exe tool / https://learn.microsoft.com/en-us/windows-hardware/design/component-guidelines/mouse-collection-report-descriptor
const REPORTID_MOUSE: u8 = 1;
const MOUSE_HID_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01,        // Usage Page (Generic Desktop Ctrls)
    0x09, 0x02,        // Usage (Mouse)
    0xA1, 0x01,        // Collection (Application)
    0x85, REPORTID_MOUSE,            //   REPORT_ID (Touch pad) **** THIS IS ADAPTED FROM SAMPLE TOUCHPAD DESCRIPTOR, THE MOUSE EXAMPLE OMITTED IT BECAUSSE IT ONLY HAD 1 REPORT
    0x09, 0x01,        //   Usage (Pointer)
    0xA1, 0x00,        //   Collection (Physical)
    0x05, 0x09,        //     Usage Page (Button)
    0x19, 0x01,        //     Usage Minimum (0x01)
    0x29, 0x03,        //     Usage Maximum (0x03)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x95, 0x03,        //     Report Count (3)
    0x75, 0x01,        //     Report Size (1)
    0x81, 0x02,        //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x01,        //     Report Count (1)
    0x75, 0x05,        //     Report Size (5)
    0x81, 0x03,        //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01,        //     Usage Page (Generic Desktop Ctrls)
    0x09, 0x30,        //     Usage (X)
    0x09, 0x31,        //     Usage (Y)
    0x15, 0x81,        //     Logical Minimum (-127)
    0x25, 0x7F,        //     Logical Maximum (127)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x06,        //     Input (Data,Var,Rel,No Wrap,Linear,Preferred State,No Null Position)
    0xC0,              //   End Collection
    0xC0,              // End Collection
];

const MOUSE_BUTTON_1: u8 = 0x01;
#[allow(dead_code)]
const MOUSE_BUTTON_2: u8 = 0x02;
#[allow(dead_code)]
const MOUSE_BUTTON_3: u8 = 0x04; 

#[repr(C)]
#[derive(Debug, Default, defmt::Format, zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable)]
struct MouseReport {
    buttons: u8, // 3 bits used for buttons, 5 bits padding
    x: i8,
    y: i8,
}

struct MockMouseService {
    // Signal a click
    channel: embassy_sync::channel::Channel<embedded_services::GlobalRawMutex, MouseReport, 3>,
}

impl MockMouseService {
    pub async fn send_click(&self) {
        // Mouse down
        let send_result = self.channel.try_send(MouseReport {
            buttons: MOUSE_BUTTON_1,
            x: 0,
            y: 0,
        });

        if let Err(e) = send_result {
            warn!("Failed to send mouse down report: {:?}", e);
        }

        embassy_time::Timer::after(embassy_time::Duration::from_millis(15)).await;

        // Mouse up
        let send_result = self.channel.try_send(MouseReport {
            buttons: 0,
            x: 0,
            y: 0,
        });

        if let Err(e) = send_result {
            warn!("Failed to send mouse up report: {:?}", e);
        }
    }

    #[allow(dead_code)]
    pub fn move_mouse(&self) {
        let send_result = self.channel.try_send(MouseReport {
            buttons: MOUSE_BUTTON_1,
            x: 10,
            y: 10,
        });

        if let Err(e) = send_result {
            warn!("Failed to send mouse move report: {:?}", e);
        }
    }

    pub fn receiver(&self) -> embassy_sync::channel::Receiver<'_, embedded_services::GlobalRawMutex, MouseReport, 3> {
        self.channel.receiver()
    }
}

// TODO if this pattern is going to be common, maybe write a generic struct to do it
struct MockMouseHidRelay<'s> {
    service: &'s MockMouseService,
    descriptor: HidReportDescriptor,
}

impl<'s> MockMouseHidRelay<'s> {
    pub fn new(service: &'s MockMouseService) -> Self {
        Self {
            service,
            descriptor: HidReportDescriptor::new_static(MOUSE_HID_REPORT_DESCRIPTOR),
        }
    }
}

impl embedded_services::relay::hid::HidDevice for MockMouseHidRelay<'_> {
    type InputReportMaxSize = typenum::U3;
    type OutputReportMaxSize = typenum::U0;
    type FeatureReportMaxSize = typenum::U0;

    const MAX_REPORT_COUNT: u8 = 3;

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
            ReportId(REPORTID_MOUSE) => {
                let report = MouseReport::default();
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
            SetHidReport::Output(r) => info!("Received command to set output report with ID {:?}", r.id()),
            SetHidReport::Feature(r) => info!("Received command to set feature report with ID {:?}", r.id()),
        }
        info!("SET_REPORT NOT IMPLEMENTED"); // TODO implement this if we need it
        Ok(())
    }

    async fn wait_for_input_report(&mut self) {
        self.service.receiver().ready_to_receive().await
    }

    async fn next_input_report(&mut self) -> Result<HidReport<Self::InputReportMaxSize>, HidError> {
        let report = self.service.receiver().receive().await;
        let hid_report = HidReport::new(ReportId(REPORTID_MOUSE), report.as_bytes()).unwrap();
        Ok(hid_report)
    }

    fn has_pending_input_report(&mut self) -> bool {
        !self.service.receiver().is_empty()
    }

    async fn set_power_state(&mut self, state: HidDevicePowerState) -> Result<(), HidError> {
        info!("Received command to set power state to {:?}", state);
        Ok(())
    }

    async fn reset(&mut self) {
        info!("Received reset command");
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

    static MOUSE_SERVICE: StaticCell<MockMouseService> = StaticCell::new();
    let mouse_service = MOUSE_SERVICE.init(MockMouseService {
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

    let _hidsvc = odp_service_common::spawn_service!(
        spawner,
        hidi2c_target_service::Service<'static, I2cSlave<'static, Async>, gpio::Output<'static>, MockMouseHidRelay<'static>>,
        |resources| hidi2c_target_service::Service::new(
            resources,
            i2c,
            attn_pin,
            MockMouseHidRelay::new(mouse_service),
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

    loop {
        info!("clicking mouse");
        mouse_service.send_click().await;
        embassy_time::Timer::after(embassy_time::Duration::from_millis(2000)).await;
    }
}
