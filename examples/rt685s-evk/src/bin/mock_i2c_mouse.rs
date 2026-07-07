#![no_std]
#![no_main]
#![warn(warnings)] // TODO remove before checkin

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_imxrt::i2c::slave::{Address, I2cSlave};
use embassy_imxrt::i2c::{self, Async};
use embassy_imxrt::{bind_interrupts, peripherals};
use embassy_sync::zerocopy_channel;
use embedded_services::GlobalRawMutex;
use embedded_services::relay::hid::*;
use panic_probe as _;
use static_cell::StaticCell;
use zerocopy::IntoBytes;

const SLAVE_ADDR: Option<Address> = Address::new(0x15);

// This is adapted from the example mouse HID descriptor packaged with the DT.exe tool / https://learn.microsoft.com/en-us/windows-hardware/design/component-guidelines/mouse-collection-report-descriptor
const REPORTID_MOUSE: u8 = 1;

#[rustfmt::skip]
const MOUSE_HID_REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop Ctrls)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x85, REPORTID_MOUSE, //   REPORT_ID (Touch pad) **** THIS IS ADAPTED FROM SAMPLE TOUCHPAD DESCRIPTOR, THE MOUSE EXAMPLE OMITTED IT BECAUSSE IT ONLY HAD 1 REPORT
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (0x01)
    0x29, 0x03, //     Usage Maximum (0x03)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x95, 0x03, //     Report Count (3)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x95, 0x01, //     Report Count (1)
    0x75, 0x05, //     Report Size (5)
    0x81, 0x03, //     Input (Const,Var,Abs,No Wrap,Linear,Preferred State,No Null Position)
    0x05, 0x01, //     Usage Page (Generic Desktop Ctrls)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x15, 0x81, //     Logical Minimum (-127)
    0x25, 0x7F, //     Logical Maximum (127)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x06, //     Input (Data,Var,Rel,No Wrap,Linear,Preferred State,No Null Position)
    0xC0, //   End Collection
    0xC0, // End Collection
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

// Number of in-flight input reports the zero-copy channel can hold. Depth >= 2 lets the producer
// stage the next report while the consumer is still handing the current one to the host.
const MOUSE_CHANNEL_DEPTH: usize = 4;

// Zero-copy channel aliases. Unlike `embassy_sync::channel::Channel`, the payload lives in a
// caller-provided ring buffer and is written/read *in place* via `&mut MouseReport`, so a large
// report is never memcpy'd between the producer, the channel, and the I2C write path.
type MouseChannel = zerocopy_channel::Channel<'static, GlobalRawMutex, MouseReport>;
type MouseSender = zerocopy_channel::Sender<'static, GlobalRawMutex, MouseReport>;
type MouseReceiver = zerocopy_channel::Receiver<'static, GlobalRawMutex, MouseReport>;

/// Producer side of the mock. Owns the channel `Sender` and fills report slots in place.
struct MockMouseProducer {
    sender: MouseSender,
}

impl MockMouseProducer {
    /// Acquire the next free slot in the ring buffer and populate it in place, then publish it.
    async fn send(&mut self, report: MouseReport) {
        // `send()` waits for a free slot and yields `&mut MouseReport` pointing straight into the
        // channel's ring buffer. Writing through it avoids copying the payload into the channel.
        let slot = self.sender.send().await;
        *slot = report;
        // Publish the slot to the consumer. Nothing is copied here either.
        self.sender.send_done();
    }

    pub async fn send_click(&mut self) {
        // Mouse down
        self.send(MouseReport {
            buttons: MOUSE_BUTTON_1,
            x: 0,
            y: 0,
        })
        .await;

        embassy_time::Timer::after(embassy_time::Duration::from_millis(15)).await;

        // Mouse up
        self.send(MouseReport { buttons: 0, x: 0, y: 0 }).await;
    }

    #[allow(dead_code)]
    pub async fn move_mouse(&mut self) {
        self.send(MouseReport {
            buttons: MOUSE_BUTTON_1,
            x: 10,
            y: 10,
        })
        .await;
    }
}

/// Consumer/relay side of the mock. Owns the channel `Receiver` and hands the host borrows that
/// point directly into the ring buffer — no intermediate copy of the report payload.
struct MockMouseHidRelay {
    receiver: MouseReceiver,
    descriptor: HidReportDescriptor,
}

impl MockMouseHidRelay {
    pub fn new(receiver: MouseReceiver) -> Self {
        Self {
            receiver,
            descriptor: HidReportDescriptor::new_static(MOUSE_HID_REPORT_DESCRIPTOR),
        }
    }
}

impl embedded_services::relay::hid::HidDevice for MockMouseHidRelay {
    type InputReportMaxSize = typenum::U3;
    type OutputReportMaxSize = typenum::U0;
    type FeatureReportMaxSize = typenum::U0;

    const MAX_REPORT_COUNT: u8 = 3;

    fn report_descriptor(&self) -> &HidReportDescriptor {
        &self.descriptor
    }

    async fn process_get_report<R>(
        &mut self,
        _report_type: GetHidReportType,
        report_id: ReportId,
        process_report: impl AsyncFnOnce(GetHidReport<'_>) -> R,
    ) -> Result<R, HidError> {
        info!("Received command to get report with ID {:?}", report_id);
        match report_id {
            ReportId(REPORTID_MOUSE) => {
                // The report only needs to live for the duration of `process_report`, so we can build
                // it on the stack instead of keeping a field around to back the borrow.
                let report = MouseReport::default();
                Ok(process_report(GetHidReport::Input(HidReport::new(report_id, report.as_bytes()))).await)
            }
            _ => {
                info!("Report ID {:?} not recognized", report_id);
                Err(HidError::TriggerReset)
            }
        }
    }

    async fn set_report(&mut self, report: &SetHidReport<'_>) -> Result<(), HidError> {
        match report {
            SetHidReport::Output(r) => info!("Received command to set output report with ID {:?}", r.id()),
            SetHidReport::Feature(r) => info!("Received command to set feature report with ID {:?}", r.id()),
        }
        info!("SET_REPORT NOT IMPLEMENTED");
        Ok(())
    }

    async fn wait_for_input_report(&mut self) {
        // `receive()` only peeks at the front slot - it doesn't treat the sample as consumed until `receive_done()` is called.
        // Therefore, we can do this to wait until a report is ready, and then immediately drop it without losing the report.
        let _ = self.receiver.receive().await;
    }

    async fn process_next_input_report<R>(
        &mut self,
        process_report: impl AsyncFnOnce(HidReport<'_>) -> R,
    ) -> Result<R, HidError> {
        // Borrow the next report out of the channel and lend it to the transport for the duration of `process_report`.
        // This is probably unnecessary for mice because of how small the reports are, but it demonstrates the technique.
        // For a mouse it may make more sense to use a traditional Channel and just copy the 3 bytes around.
        let slot = self.receiver.receive().await;
        let result = process_report(HidReport::new(ReportId(REPORTID_MOUSE), slot.as_bytes())).await;

        // The transport is done with the borrow, so return the slot to the producer.  This is required by zerocopy_channel.
        self.receiver.receive_done();
        Ok(result)
    }

    fn has_pending_input_report(&mut self) -> bool {
        !self.receiver.is_empty()
    }

    async fn set_power_state(&mut self, state: HidDevicePowerState) -> Result<(), HidError> {
        info!("Received command to set power state to {:?}", state);
        Ok(())
    }

    async fn reset(&mut self) {
        info!("Received reset command");
        self.receiver.clear();
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

    static MOUSE_BUF: StaticCell<[MouseReport; MOUSE_CHANNEL_DEPTH]> = StaticCell::new();
    static MOUSE_CHANNEL: StaticCell<MouseChannel> = StaticCell::new();
    let mouse_buf = MOUSE_BUF.init(core::array::from_fn(|_| MouseReport::default()));
    let mouse_channel = MOUSE_CHANNEL.init(MouseChannel::new(mouse_buf));
    // Split the channel into a producer (kept here in `main`) and a consumer (moved into the relay).
    let (sender, receiver) = mouse_channel.split();
    let mut producer = MockMouseProducer { sender };

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
        hidi2c_target_service::Service<'static, I2cSlave<'static, Async>, gpio::Output<'static>, MockMouseHidRelay>,
        |resources| hidi2c_target_service::Service::new(
            resources,
            i2c,
            attn_pin,
            MockMouseHidRelay::new(receiver),
            hidi2c_target_service::HardwareVersionInfo {
                vendor_id: hidi2c_target_service::VendorId::new(0x1234).unwrap(), // TODO pick a real vendor ID
                product_id: hidi2c_target_service::ProductId(0x5678),             // TODO pick a real product ID
                version_id: hidi2c_target_service::VersionId(0x0001),             // TODO pick a real version number
            },
            hidi2c_target_service::TimeoutSettings::default()
        )
    )
    .expect("Failed to spawn HID service");

    info!("Waiting 10s before starting to send inputs");
    embassy_time::Timer::after(embassy_time::Duration::from_secs(10)).await;

    loop {
        info!("clicking mouse");
        producer.send_click().await;
        embassy_time::Timer::after(embassy_time::Duration::from_millis(2000)).await;
    }
}
