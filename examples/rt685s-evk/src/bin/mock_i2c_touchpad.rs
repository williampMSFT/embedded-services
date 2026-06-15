#![no_std]
#![no_main]
#![warn(warnings)] // TODO remove before checkin
#![no_std]
#![no_main]

use defmt::info;
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_imxrt::i2c::slave::{Address, Command, I2cSlave};
use embassy_imxrt::i2c::{self, Async};
use embassy_imxrt::{bind_interrupts, peripherals};
use panic_probe as _;

const SLAVE_ADDR: Option<Address> = Address::new(0x15);
const BUFLEN: usize = 8;

bind_interrupts!(struct Irqs {
    FLEXCOMM2 => i2c::InterruptHandler<peripherals::FLEXCOMM2>;
});

#[embassy_executor::task]
async fn slave_service(mut i2c: I2cSlave<'static, Async>) {
    loop {
        let mut buf: [u8; BUFLEN] = [0xAA; BUFLEN];

        for (i, e) in buf.iter_mut().enumerate() {
            *e = i as u8;
        }

        info!("Listening for i2c traffic");

        match i2c.listen().await.unwrap() {
            Command::Probe { addr } => {
                info!("Probe @ {:?}, nothing to do", defmt::Debug2Format(&addr));
            }
            Command::Read { addr } => {
                info!("Read @ {:?}", defmt::Debug2Format(&addr));
                loop {
                    let resp = i2c.respond_to_read(&buf).await.unwrap();
                    if resp.is_terminal() {
                        info!(
                            "Response complete read with {} bytes ({:?})",
                            resp.bytes(),
                            defmt::Debug2Format(&resp)
                        );
                        break;
                    }
                    info!("Response to read got {} bytes, more bytes to fill", resp.bytes());
                }
            }
            Command::Write { addr } => {
                info!("Write @ {:?}", defmt::Debug2Format(&addr));
                loop {
                    let resp = i2c.respond_to_write(&mut buf).await.unwrap();
                    if resp.is_terminal() {
                        info!(
                            "Response complete write with {} bytes ({:?})",
                            resp.bytes(),
                            defmt::Debug2Format(&resp)
                        );
                        break;
                    }
                    info!("Response to write got {} bytes, more bytes pending", resp.bytes());
                }
            }
            other => {
                info!("Unhandled command variant: {:?}", defmt::Debug2Format(&other));
            }
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_imxrt::init(Default::default());

    // NOTE: Tested with a raspberry pi 5 as master controller connected FC2 to i2c on Pi5
    //       Test program here: https://github.com/jerrysxie/pi5-i2c-test
    info!("i2cs example - I2c::new");
    // TODO @Felipe if I specify the slave address here, why do I also receive it as an argument in the trait?
    let i2c = I2cSlave::new_async(p.FLEXCOMM2, p.PIO0_18, p.PIO0_17, Irqs, SLAVE_ADDR.unwrap(), p.DMA0_CH4).unwrap();

    // TODO figure out how to spin up a GPIO pin for the interrupt line
    // TODO actually use hid-i2c service; the above is a hack from embassy-imxrt examples
    spawner.spawn(slave_service(i2c).unwrap());
}

// use embedded_mcu_hal::{
//     nvram::Nvram,
//     time::{Datetime, DatetimeFields, Month},
// };
// use embedded_services::info;
// use static_cell::StaticCell;
// use time_alarm_service_interface::{
//     AcpiDaylightSavingsTimeStatus, AcpiTimeZone, AcpiTimeZoneOffset, AcpiTimestamp, TimeAlarmService,
// };
// use {defmt_rtt as _, panic_probe as _};

// // Type aliases to make it easier to use the service and relay handler types without needing to write out all the generic parameters every time.
// // This is especially helpful for the relay handler, which has a lot of generic parameters due to the traits it needs to implement.
// //
// type TimeAlarmServiceType = time_alarm_service::Service<'static>;
// type TimeAlarmServiceRelayHandlerType = time_alarm_service_relay::TimeAlarmServiceRelayHandler<TimeAlarmServiceType>;

// // STUB HAL TYPES TO MAKE EXAMPLE COMPILE - THESE NEED TO BE IMPLEMENTED IN EMBASSY-IMXRT HAL
// //
// struct StubI2cTarget;
// impl embedded_mcu_hal::i2c::target::ErrorType  for StubI2cTarget {
//     type Error = core::convert::Infallible;
// }
// impl embedded_mcu_hal::i2c::target::asynch::I2c for StubI2cTarget {
//     async fn listen(&mut self) -> Result<embedded_mcu_hal::i2c::target::Request, Self::Error> {
//         todo!()
//     }

//     async fn respond_to_read(&mut self, _data: &[u8]) -> Result<embedded_mcu_hal::i2c::target::ReadStatus, Self::Error> {
//         todo!()
//     }

//     async fn respond_to_write(&mut self, _data: &mut [u8]) -> Result<embedded_mcu_hal::i2c::target::WriteStatus, Self::Error> {
//         todo!()
//     }

//     async fn recover(&mut self) -> Result<(), Self::Error> {
//         todo!()
//     }
// }

// struct StubOutputPin;
// impl embedded_hal::digital::ErrorType for StubOutputPin {
//     type Error = core::convert::Infallible;
// }
// impl embedded_hal::digital::OutputPin for StubOutputPin {
//     fn set_high(&mut self) -> Result<(), Self::Error> {
//         todo!()
//     }
//     fn set_low(&mut self) -> Result<(), Self::Error> {
//         todo!()
//     }
// }

// // END STUB HAL TYPES

// #[embassy_executor::main]
// async fn main(spawner: embassy_executor::Spawner) {
//     let p = embassy_imxrt::init(Default::default());

//     // TODO figure out what this binds to on hardware
//     let i2c = I2cSlave::new_async(p.FLEXCOMM2, p.PIO0_18, p.PIO0_17, Irqs, SLAVE_ADDR.unwrap(), p.DMA0_CH4).unwrap();
//     spawner.spawn(slave_service(i2c).unwrap());

//     // static RTC: StaticCell<embassy_imxrt::rtc::Rtc> = StaticCell::new();
//     // let rtc = RTC.init(embassy_imxrt::rtc::Rtc::new(p.RTC));
//     // let (dt_clock, rtc_nvram) = rtc.split();

//     // let [tz, ac_expiration, ac_policy, dc_expiration, dc_policy, ..] = rtc_nvram.storage();

//     // embedded_services::init().await;
//     // info!("services initialized");

//     // let time_service = odp_service_common::spawn_service!(
//     //     spawner,
//     //     TimeAlarmServiceType,
//     //     |resources| TimeAlarmServiceType::new(resources,
//     //         dt_clock,
//     //         tz,
//     //         ac_expiration,
//     //         ac_policy,
//     //         dc_expiration,
//     //         dc_policy
//     //     )
//     // )
//     // .expect("Failed to spawn time alarm service");

//     // use time_alarm_service_relay::hid::{TimeAlarmHidRelay};
//     // use hid_i2c_service::*;

//     // let hid_tad_handler = time_alarm_service_relay::hid::TimeAlarmHidRelay::new(time_service);

//     // // NOTE: here's where the "aggregate HID devices" macro is currently missing.  Compare with time_alarm.rs where we do this:
//     // //
//     // //     impl_odp_mctp_relay_handler!(
//     // //         EspiRelayHandler;
//     // //         TimeAlarm, 0x0B, crate::TimeAlarmServiceRelayHandlerType;
//     // //     );
//     // //
//     // //  We will eventually write a macro that looks something like this:
//     // //
//     // //     impl_hid_aggregate_device!(
//     // //         MyAggregateDevice;
//     // //         time_alarm_service_relay::hid::TimeAlarmHidRelay,
//     // //         battery_service_relay::hid::BatteryHidRelay,
//     // //         ...
//     // //     );
//     // //
//     // // which will emit a type MyAggregateDevice that implements HidDevice and takes as construction parameters one instance
//     // // of each of the handlers, in the order they were specified.
//     // //
//     // // For a concrete example of this pattern, see what we're doing with MCTP here: https://github.com/OpenDevicePartnership/odp-embedded-controller/blob/d6fb3ce5d9ae52ca51d6ef7b87518c6c4cb3c809/platform/platform-common/src/lib.rs#L20
//     // //
//     // // This depends on writing the 'hid support library', though, so for now we're just going to directly use the time-alarm device for testing
//     // // (pending getting actual hardware to test on, implementing I2C traits for embassy, etc).

//     // let hidi2csvc = odp_service_common::spawn_service!(
//     //     spawner,
//     //     hid_i2c_service::Service<'static, StubI2cTarget, StubOutputPin, TimeAlarmHidRelay<TimeAlarmServiceType, embassy_sync::blocking_mutex::raw::NoopRawMutex>>,
//     //     |resources| hid_i2c_service::Service::new(resources,
//     //         hid_i2c_service::InitParams {
//     //             bus: StubI2cTarget{},
//     //             attn_pin: StubOutputPin{},
//     //             hid_device: hid_tad_handler,
//     //             vendor_id: VendorId(0x1234), // TODO pick a real vendor ID
//     //             product_id: ProductId(0x5678), // TODO pick a real product ID
//     //             version_id: VersionId(0x0001), // TODO pick a real version number
//     //             device_response_timeout: embassy_time::Duration::from_secs(1), // TODO figure out what a reasonable timeout is here
//     //             data_read_timeout: embassy_time::Duration::from_secs(1), // TODO figure out what a reasonable timeout is here
//     //         })
//     // );

//     // loop {
//     //     embassy_time::Timer::after(embassy_time::Duration::from_secs(10)).await;
//     //     info!("Current time from service: {:?}", time_service.get_real_time().unwrap());
//     // }
// }
