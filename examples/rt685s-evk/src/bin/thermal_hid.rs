#![no_std]
#![no_main]

use embassy_imxrt::i2c::slave::{Address, I2cSlave};
use embassy_imxrt::i2c::{self, Async};
use embassy_imxrt::{bind_interrupts, peripherals};
use embedded_services::event::NoopSender;
use embedded_services::info;
use static_cell::StaticCell;
use thermal_service as ts;
use thermal_service_interface::fan::FanService;
use thermal_service_interface::sensor::SensorService;
use {defmt_rtt as _, panic_probe as _};

const SLAVE_ADDR: Option<Address> = Address::new(0x15);
const SAMPLE_BUFFER_LEN: usize = 16;

bind_interrupts!(struct Irqs {
    FLEXCOMM2 => i2c::InterruptHandler<peripherals::FLEXCOMM2>;
});

type MockSensorService = ts::sensor::Service<'static, ts::mock::sensor::MockSensor, NoopSender, SAMPLE_BUFFER_LEN>;
type MockFanService =
    ts::fan::Service<'static, ts::mock::fan::MockFan, MockSensorService, NoopSender, SAMPLE_BUFFER_LEN>;
type ThermalServiceType = ts::Service<'static, MockSensorService, MockFanService>;
type ThermalHidRelayType = thermal_service_relay::hid::ThermalHidRelay<'static, ThermalServiceType>;

#[embassy_executor::main]
async fn main(spawner: embassy_executor::Spawner) {
    let p = embassy_imxrt::init(Default::default());

    embedded_services::init().await;
    info!("Services initialized");

    let sensor_service =
        odp_service_common::spawn_service!(spawner, MockSensorService, |resources| ts::sensor::Service::new(
            resources,
            ts::sensor::InitParams {
                driver: ts::mock::sensor::MockSensor::new(),
                config: ts::mock::sensor::MockSensor::config(),
                event_senders: &mut [],
            },
        ))
        .expect("Failed to spawn mock sensor service");

    let fan_service = odp_service_common::spawn_service!(spawner, MockFanService, |resources| ts::fan::Service::new(
        resources,
        ts::fan::InitParams {
            driver: ts::mock::fan::MockFan::new(),
            config: ts::mock::fan::MockFan::config(),
            sensor_service,
            event_senders: &mut [],
        },
    ))
    .expect("Failed to spawn mock fan service");

    static SENSORS: StaticCell<[MockSensorService; 1]> = StaticCell::new();
    let sensors = SENSORS.init([sensor_service]);
    static FANS: StaticCell<[MockFanService; 1]> = StaticCell::new();
    let fans = FANS.init([fan_service]);
    static THERMAL_RESOURCES: StaticCell<ts::Resources<MockSensorService, MockFanService>> = StaticCell::new();
    let thermal_service = ts::Service::init(
        THERMAL_RESOURCES.init(ts::Resources::default()),
        ts::InitParams { sensors, fans },
    );

    let i2c = I2cSlave::new_async(p.FLEXCOMM2, p.PIO0_18, p.PIO0_17, Irqs, SLAVE_ADDR.unwrap(), p.DMA0_CH4).unwrap();

    use embassy_imxrt::gpio;
    let attn_pin = gpio::Output::new(
        p.PIO0_28,
        gpio::Level::High,
        gpio::DriveMode::OpenDrain,
        gpio::DriveStrength::Normal,
        gpio::SlewRate::Standard,
    );

    let _hidsvc = odp_service_common::spawn_service!(
        spawner,
        hidi2c_target_service::Service<'static, I2cSlave<'static, Async>, gpio::Output<'static>, ThermalHidRelayType>,
        |resources| hidi2c_target_service::Service::new(
            resources,
            i2c,
            attn_pin,
            ThermalHidRelayType::new(thermal_service),
            hidi2c_target_service::HardwareVersionInfo {
                vendor_id: hidi2c_target_service::VendorId::new(0x3333).unwrap(),
                product_id: hidi2c_target_service::ProductId(0x4445),
                version_id: hidi2c_target_service::VersionId(0x0001),
            },
            hidi2c_target_service::TimeoutSettings::default()
        )
    )
    .expect("Failed to spawn HID service");

    loop {
        embassy_time::Timer::after_secs(1).await;
        info!(
            "Mock thermal state: temperature={} C, fan={} RPM",
            sensor_service.temperature().await,
            fan_service.rpm().await
        );
    }
}
