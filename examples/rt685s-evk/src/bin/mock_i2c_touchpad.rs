#![no_std]
#![no_main]
#![warn(warnings)] // TODO remove before checkin

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_imxrt::i2c::slave::{Address, Command, I2cSlave};
use embassy_imxrt::i2c::{self, Async};
use embassy_imxrt::{bind_interrupts, peripherals};
use static_cell::StaticCell;
use panic_probe as _;
use zerocopy::IntoBytes;
use embedded_services::relay::hid::*;

const SLAVE_ADDR: Option<Address> = Address::new(0x15);

// This is adapted from the example mouse HID descriptor packaged with the DT.exe tool
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
const MOUSE_BUTTON_2: u8 = 0x02;
const MOUSE_BUTTON_3: u8 = 0x04; 

#[repr(C)]
#[derive(Debug, Default, defmt::Format, zerocopy::FromBytes, zerocopy::IntoBytes, zerocopy::Immutable)]
struct MouseReport {
    buttons: u8, // 3 bits used for buttons, 5 bits padding
    x: i8,
    y: i8,
}


struct MockTouchpadService {
    // Signal a click
    channel: embassy_sync::channel::Channel<embedded_services::GlobalRawMutex, MouseReport, 3>,
}

impl MockTouchpadService {
    pub fn send_click(&self) {
        self.channel.try_send(MouseReport {
            buttons: MOUSE_BUTTON_1,
            x: 0,
            y: 0,
        }).unwrap()
    }

    pub fn receiver(&self) -> embassy_sync::channel::Receiver<'_, embedded_services::GlobalRawMutex, MouseReport, 3> {
        self.channel.receiver()
    }
}

// TODO if this pattern is going to be common, maybe write a generic struct to do it
struct TouchpadNotificationHidReceiver<'a> {
    receiver: embassy_sync::channel::Receiver<'a, embedded_services::GlobalRawMutex, MouseReport, 3>,
}

impl<'a, MaxSize: generic_array::ArrayLength> ReportReceiver<MaxSize> for TouchpadNotificationHidReceiver<'a> {
    async fn ready_to_receive(&self) {
        self.receiver.ready_to_receive().await
    }

    async fn receive(&self) -> HidResult<HidReport<MaxSize>> {
        let report = self.receiver.receive().await;
        let hid_report = HidReport::new(ReportId(REPORTID_MOUSE), report.as_bytes()).unwrap();
        HidResult::Ok(hid_report)
    }

    fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

struct MockTouchpadHidRelay<'s> {
    service: &'s MockTouchpadService,
    descriptor: HidReportDescriptor,
}

impl<'s> MockTouchpadHidRelay<'s> {
    pub fn new(service: &'s MockTouchpadService) -> Self {
        Self {
            service,
            descriptor: HidReportDescriptor::new_static(MOUSE_HID_REPORT_DESCRIPTOR),
        }
    }
}

impl embedded_services::relay::hid::HidDevice for MockTouchpadHidRelay<'_> {
    type InputReportMaxSize = typenum::U4; // TODO figure out real number
    type OutputReportMaxSize = typenum::U0; // TODO figure out real number
    type FeatureReportMaxSize = typenum::U0; // TODO figure out real number

    /// The type that will surface HID reports as they become available.
    type ReportReceiver<'a> = TouchpadNotificationHidReceiver<'a > where Self: 'a;

    const MAX_REPORT_COUNT: u8 = 10; // TODO figure out real number

    fn report_descriptor(&self) -> &HidReportDescriptor {
        &self.descriptor
    }

    async fn get_report(
        &mut self,
        report_id: ReportId,
    ) -> HidResult<GetHidReport<Self::InputReportMaxSize, Self::FeatureReportMaxSize>> {
        info!("Received command to get report with ID {:?}", report_id);
        match report_id {
            ReportId(REPORTID_MOUSE) => {
                let report = MouseReport::default();
                HidResult::Ok(GetHidReport::Input(HidReport::<Self::InputReportMaxSize>::new(
                    report_id,
                    report.as_bytes()
                ).unwrap()))
            }
            _ => {
                info!("Report ID {:?} not recognized", report_id);
                HidResult::TriggerReset
            }
        }
    }

    async fn set_report(
        &mut self,
        report: &SetHidReport<Self::OutputReportMaxSize, Self::FeatureReportMaxSize>,
    ) -> HidResult<()> {
        match report {
            SetHidReport::Output(r) => info!("Received command to set output report with ID {:?}", r.id()),
            SetHidReport::Feature(r) => info!("Received command to set feature report with ID {:?}", r.id()),
        }
        info!("SET_REPORT NOT IMPLEMENTED"); // TODO implement this if we need it
        HidResult::Ok(())
    }

    fn receiver(&mut self) -> Self::ReportReceiver<'_> {
        TouchpadNotificationHidReceiver{receiver: self.service.receiver()} // TODO need to have a thread to consume from the actual service and pass through to this channel, or implement a consumer/editor
    }

    async fn set_power_state(&mut self, state: HidDevicePowerState) -> HidResult<()> {
        info!("Received command to set power state to {:?}", state);
        HidResult::Ok(())
    }

    async fn host_reset(&mut self) {
        info!("Received reset command");
    }
}


bind_interrupts!(struct Irqs {
    FLEXCOMM2 => i2c::InterruptHandler<peripherals::FLEXCOMM2>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_imxrt::init(Default::default());
    info!("HID-I2C mock touchpad example starting...");
    // TODO @Felipe if I specify the slave address here, why do I also receive it as an argument in the trait?
    let i2c = I2cSlave::new_async(p.FLEXCOMM2, p.PIO0_18, p.PIO0_17, Irqs, SLAVE_ADDR.unwrap(), p.DMA0_CH4).unwrap();

    // GPIO on P0_28.
    use embassy_imxrt::gpio;
    let mut interrupt_pin = gpio::Output::new(
        p.PIO0_28,
        gpio::Level::Low,
        gpio::DriveMode::PushPull, // TODO I'm not confident this is correct; figure out what the right settings are for the interrupt line
        gpio::DriveStrength::Normal,
        gpio::SlewRate::Standard,
    );

    static TOUCHPAD_SERVICE: StaticCell<MockTouchpadService> = StaticCell::new();
    let touchpad_service = TOUCHPAD_SERVICE.init(MockTouchpadService {
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
        hidi2c_target_service::Service<'static, I2cSlave<'static, Async>, gpio::Output<'static>, MockTouchpadHidRelay<'static>>,
        |resources| hidi2c_target_service::Service::new(
            resources,
            hidi2c_target_service::InitParams {
                bus: i2c,
                attn_pin: interrupt_pin,
                hid_device: MockTouchpadHidRelay::new(touchpad_service),
                vendor_id: hidi2c_target_service::VendorId(0x1234), // TODO pick a real vendor ID
                product_id: hidi2c_target_service::ProductId(0x5678), // TODO pick a real product ID
                version_id: hidi2c_target_service::VersionId(0x0001), // TODO pick a real version number
                device_response_timeout: embassy_time::Duration::from_secs(1), // TODO figure out what a reasonable timeout is here
                data_read_timeout: embassy_time::Duration::from_secs(1), // TODO figure out what a reasonable timeout is here
            }
        )
    );

    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(5)).await;
        info!("clicking touchpad");
        touchpad_service.send_click();
    }
}





// Below is an attempt at using the touchpad HID descriptor provided on the hardware integration site. TBD if it's necessary for this demo - it's a lot more complicated than I'd like...







// // This descriptor is taken directly from https://learn.microsoft.com/en-us/windows-hardware/design/component-guidelines/touchpad-sample-report-descriptors
// // Report IDs are arbitrary for this example.
// //
// const REPORTID_TOUCHPAD: u8 = 1;

// const REPORTID_MAX_COUNT: u8 = 2;
// const REPORTID_PTPHQA: u8 = 3;
// const REPORTID_FEATURE: u8 = 4;
// const REPORTID_FUNCTION_SWITCH: u8 = 5;
// const REPORTID_MOUSE: u8 = 6;
// const TOUCHPAD_HID_REPORT_DESCRIPTOR: &[u8] = &[
//     //TOUCH PAD input TLC
//     0x05, 0x0d,                         // USAGE_PAGE (Digitizers)
//     0x09, 0x05,                         // USAGE (Touch Pad)
//     0xa1, 0x01,                         // COLLECTION (Application)
//     0x85, REPORTID_TOUCHPAD,            //   REPORT_ID (Touch pad)
//     0x09, 0x22,                         //   USAGE (Finger)
//     0xa1, 0x02,                         //   COLLECTION (Logical)
//     0x15, 0x00,                         //       LOGICAL_MINIMUM (0)
//     0x25, 0x01,                         //       LOGICAL_MAXIMUM (1)
//     0x09, 0x47,                         //       USAGE (Confidence)
//     0x09, 0x42,                         //       USAGE (Tip switch)
//     0x95, 0x02,                         //       REPORT_COUNT (2)
//     0x75, 0x01,                         //       REPORT_SIZE (1)
//     0x81, 0x02,                         //       INPUT (Data,Var,Abs)
//     0x95, 0x01,                         //       REPORT_COUNT (1)
//     0x75, 0x02,                         //       REPORT_SIZE (2)
//     0x25, 0x02,                         //       LOGICAL_MAXIMUM (2)
//     0x09, 0x51,                         //       USAGE (Contact Identifier)
//     0x81, 0x02,                         //       INPUT (Data,Var,Abs)
//     0x75, 0x01,                         //       REPORT_SIZE (1)
//     0x95, 0x04,                         //       REPORT_COUNT (4)
//     0x81, 0x03,                         //       INPUT (Cnst,Var,Abs)
//     0x05, 0x01,                         //       USAGE_PAGE (Generic Desk..
//     0x15, 0x00,                         //       LOGICAL_MINIMUM (0)
//     0x26, 0xff, 0x0f,                   //       LOGICAL_MAXIMUM (4095)
//     0x75, 0x10,                         //       REPORT_SIZE (16)
//     0x55, 0x0e,                         //       UNIT_EXPONENT (-2)
//     0x65, 0x13,                         //       UNIT(Inch,EngLinear)
//     0x09, 0x30,                         //       USAGE (X)
//     0x35, 0x00,                         //       PHYSICAL_MINIMUM (0)
//     0x46, 0x90, 0x01,                   //       PHYSICAL_MAXIMUM (400)
//     0x95, 0x01,                         //       REPORT_COUNT (1)
//     0x81, 0x02,                         //       INPUT (Data,Var,Abs)
//     0x46, 0x13, 0x01,                   //       PHYSICAL_MAXIMUM (275)
//     0x09, 0x31,                         //       USAGE (Y)
//     0x81, 0x02,                         //       INPUT (Data,Var,Abs)
//     0xc0,                               //    END_COLLECTION
//     0x55, 0x0C,                         //    UNIT_EXPONENT (-4)
//     0x66, 0x01, 0x10,                   //    UNIT (Seconds)
//     0x47, 0xff, 0xff, 0x00, 0x00,      //     PHYSICAL_MAXIMUM (65535)
//     0x27, 0xff, 0xff, 0x00, 0x00,         //  LOGICAL_MAXIMUM (65535)
//     0x75, 0x10,                           //  REPORT_SIZE (16)
//     0x95, 0x01,                           //  REPORT_COUNT (1)
//     0x05, 0x0d,                         //    USAGE_PAGE (Digitizers)
//     0x09, 0x56,                         //    USAGE (Scan Time)
//     0x81, 0x02,                           //  INPUT (Data,Var,Abs)
//     0x09, 0x54,                         //    USAGE (Contact count)
//     0x25, 0x7f,                           //  LOGICAL_MAXIMUM (127)
//     0x95, 0x01,                         //    REPORT_COUNT (1)
//     0x75, 0x08,                         //    REPORT_SIZE (8)
//     0x81, 0x02,                         //    INPUT (Data,Var,Abs)
//     0x05, 0x09,                         //    USAGE_PAGE (Button)
//     0x09, 0x01,                         //    USAGE_(Button 1)
//     0x09, 0x02,                         //    USAGE_(Button 2)
//     0x09, 0x03,                         //    USAGE_(Button 3)
//     0x25, 0x01,                         //    LOGICAL_MAXIMUM (1)
//     0x75, 0x01,                         //    REPORT_SIZE (1)
//     0x95, 0x03,                         //    REPORT_COUNT (3)
//     0x81, 0x02,                         //    INPUT (Data,Var,Abs)
//     0x95, 0x05,                          //   REPORT_COUNT (5)
//     0x81, 0x03,                         //    INPUT (Cnst,Var,Abs)
//     0x05, 0x0d,                         //    USAGE_PAGE (Digitizer)
//     0x85, REPORTID_MAX_COUNT,            //   REPORT_ID (Feature)
//     0x09, 0x55,                         //    USAGE (Contact Count Maximum)
//     0x09, 0x59,                         //    USAGE (Pad TYpe)
//     0x75, 0x04,                         //    REPORT_SIZE (4)
//     0x95, 0x02,                         //    REPORT_COUNT (2)
//     0x25, 0x0f,                         //    LOGICAL_MAXIMUM (15)
//     0xb1, 0x02,                         //    FEATURE (Data,Var,Abs)
//     0x06, 0x00, 0xff,                   //    USAGE_PAGE (Vendor Defined)
//     0x85, REPORTID_PTPHQA,               //    REPORT_ID (PTPHQA)
//     0x09, 0xC5,                         //    USAGE (Vendor Usage 0xC5)
//     0x15, 0x00,                         //    LOGICAL_MINIMUM (0)
//     0x26, 0xff, 0x00,                   //    LOGICAL_MAXIMUM (0xff)
//     0x75, 0x08,                         //    REPORT_SIZE (8)
//     0x96, 0x00, 0x01,                   //    REPORT_COUNT (0x100 (256))
//     0xb1, 0x02,                         //    FEATURE (Data,Var,Abs)
//     0xc0,                               // END_COLLECTION
//     //CONFIG TLC
//     0x05, 0x0d,                         //    USAGE_PAGE (Digitizer)
//     0x09, 0x0E,                         //    USAGE (Configuration)
//     0xa1, 0x01,                         //   COLLECTION (Application)
//     0x85, REPORTID_FEATURE,             //   REPORT_ID (Feature)
//     0x09, 0x22,                         //   USAGE (Finger)
//     0xa1, 0x02,                         //   COLLECTION (logical)
//     0x09, 0x52,                         //    USAGE (Input Mode)
//     0x15, 0x00,                         //    LOGICAL_MINIMUM (0)
//     0x25, 0x0a,                         //    LOGICAL_MAXIMUM (10)
//     0x75, 0x08,                         //    REPORT_SIZE (8)
//     0x95, 0x01,                         //    REPORT_COUNT (1)
//     0xb1, 0x02,                         //    FEATURE (Data,Var,Abs
//     0xc0,                               //   END_COLLECTION
//     0x09, 0x22,                         //   USAGE (Finger)
//     0xa1, 0x00,                         //   COLLECTION (physical)
//     0x85, REPORTID_FUNCTION_SWITCH,     //     REPORT_ID (Feature)
//     0x09, 0x57,                         //     USAGE(Surface switch)
//     0x09, 0x58,                         //     USAGE(Button switch)
//     0x75, 0x01,                         //     REPORT_SIZE (1)
//     0x95, 0x02,                         //     REPORT_COUNT (2)
//     0x25, 0x01,                         //     LOGICAL_MAXIMUM (1)
//     0xb1, 0x02,                         //     FEATURE (Data,Var,Abs)
//     0x95, 0x06,                         //     REPORT_COUNT (6)
//     0xb1, 0x03,                         //     FEATURE (Cnst,Var,Abs)
//     0xc0,                               //   END_COLLECTION
//     0xc0,                               // END_COLLECTION
//     //MOUSE TLC
//     0x05, 0x01,                         // USAGE_PAGE (Generic Desktop)
//     0x09, 0x02,                         // USAGE (Mouse)
//     0xa1, 0x01,                         // COLLECTION (Application)
//     0x85, REPORTID_MOUSE,               //   REPORT_ID (Mouse)
//     0x09, 0x01,                         //   USAGE (Pointer)
//     0xa1, 0x00,                         //   COLLECTION (Physical)
//     0x05, 0x09,                         //     USAGE_PAGE (Button)
//     0x19, 0x01,                         //     USAGE_MINIMUM (Button 1)
//     0x29, 0x02,                         //     USAGE_MAXIMUM (Button 2)
//     0x25, 0x01,                         //     LOGICAL_MAXIMUM (1)
//     0x75, 0x01,                         //     REPORT_SIZE (1)
//     0x95, 0x02,                         //     REPORT_COUNT (2)
//     0x81, 0x02,                         //     INPUT (Data,Var,Abs)
//     0x95, 0x06,                         //     REPORT_COUNT (6)
//     0x81, 0x03,                         //     INPUT (Cnst,Var,Abs)
//     0x05, 0x01,                         //     USAGE_PAGE (Generic Desktop)
//     0x09, 0x30,                         //     USAGE (X)
//     0x09, 0x31,                         //     USAGE (Y)
//     0x75, 0x10,                         //     REPORT_SIZE (16)
//     0x95, 0x02,                         //     REPORT_COUNT (2)
//     0x25, 0x0a,                          //    LOGICAL_MAXIMUM (10)
//     0x81, 0x06,                         //     INPUT (Data,Var,Rel)
//     0xc0,                               //   END_COLLECTION
//     0xc0,                                //END_COLLECTION
// ];

// // TODO Figure out shape of mouse struct
// // The below is a variant of the above with comments mapped from the linux hid intro
// // 0x05, 0x01,                         // USAGE_PAGE (Generic Desktop)
// // 0x09, 0x02,                         // USAGE (Mouse)
// // 0xa1, 0x01,                         // COLLECTION (Application)
// // 0x85, REPORTID_MOUSE,               //   REPORT_ID (Mouse)                     //TODO FIGURE OUT MAPPING HERE
// // 0x09, 0x01,                         //   USAGE (Pointer)
// // 0xa1, 0x00,                         //   COLLECTION (Physical)
// // 0x05, 0x09,                         //     USAGE_PAGE (Button)

// //////// WHAT FOLLOWS IS A BUTTON

// // 0x19, 0x01,                         //     USAGE_MINIMUM (Button 1)
// // 0x29, 0x02,                         //     USAGE_MAXIMUM (Button 2)

// //////// FIRST BUTTON NUMBER 1, LAST BUTTON NUMBER 2

// // 0x25, 0x01,                         //     LOGICAL_MAXIMUM (1)

// /////// EACH BUTTON CAN SEND VALUES 0-1 (binary)

// // 0x75, 0x01,                         //     REPORT_SIZE (1)

// /////// EACH BUTTON IS SENT AS 1 BIT

// // 0x95, 0x02,                         //     REPORT_COUNT (2)

// /////// THERE ARE TWO OF THOSE BITS, MATCHING THE TWO BUTTONS

// // 0x81, 0x02,                         //     INPUT (Data,Var,Abs)

// /////// IT"S ACTUAL DATA, NOT A CONSTANT, AND IT'S AN ABSOLUTE VALUE, NOT A RELATIVE VALUE

// // 0x95, 0x06,                         //     REPORT_COUNT (6)
// // 0x81, 0x03,                         //     INPUT (Cnst,Var,Abs)

// /////// THEN 6 BITS OF PADDING

// // 0x05, 0x01,                         //     USAGE_PAGE (Generic Desktop)
// // 0x09, 0x30,                         //     USAGE (X)
// // 0x09, 0x31,                         //     USAGE (Y)

// ////// MOUSE HAS 2 PHYSICAL POSITIONS, X/Y (NO WHEEL)

// // 0x75, 0x10,                         //     REPORT_SIZE (16)
// // 0x95, 0x02,                         //     REPORT_COUNT (2)

// ////// THERE ARE 2 16-BIT FIELDS (X and Y)

// // 0x25, 0x0a,                          //    LOGICAL_MAXIMUM (10)

// ///// THE MAXIMUM VALUE FOR EACH OF THOSE FIELDS IS 10 - THIS SEEMS LIKE A BUG TO ME BUT WAS TAKEN DIRECTLY FROM THE MSDN PAGE?

// // 0x81, 0x06,                         //     INPUT (Data,Var,Rel)

// ////// THESE VALUES ARE RELATIVE TO PRIOR SAMPLES

// // 0xc0,                               //   END_COLLECTION
// // 0xc0,                                //END_COLLECTION

// // Seems to me like the above boils down to
// const MOUSE_BUTTON_1: u8 = 0x01;
// const MOUSE_BUTTON_2: u8 = 0x02;
// #[repr(C)]
// struct MouseReport {
//     buttons: u8, // 2 bits used for buttons, 6 bits padding
//     x: u16,
//     y: u16,
// }