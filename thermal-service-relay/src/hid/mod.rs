//! Relay types for the thermal service over HID.

use embedded_services::relay::hid::{
    GetHidReport, GetHidReportType, HidDevicePowerState, HidError, HidReport, HidReportDescriptor, ReportId,
    SetHidReport,
};
use embedded_services::{error, info};
use thermal_service_interface::ThermalService;
use thermal_service_interface::fan::FanService;
use thermal_service_interface::sensor::SensorService;

use crate::DeciKelvin;

mod serialization;

use serialization::{FanSpeed, InputReportId, OutputReportId};

const DEFAULT_INSTANCE_ID: u8 = 0;

/// Relay adapter that presents the first registered fan and temperature sensor as a HID device.
pub struct ThermalHidRelay<'s, Service: ThermalService> {
    service: Service,
    descriptor: HidReportDescriptor<'static>,
    _phantom: core::marker::PhantomData<&'s ()>,
}

impl<'s, Service: ThermalService> ThermalHidRelay<'s, Service> {
    /// Creates a thermal HID relay backed by `service`.
    pub fn new(service: Service) -> Self {
        Self {
            service,

            // Panic safety: The HID report descriptor is a static constant and should always be valid.
            #[allow(clippy::expect_used)]
            descriptor: HidReportDescriptor::new(serialization::THERMAL_HID_DESCRIPTOR)
                .expect("thermal HID report descriptor is a static constant and should be valid"),

            _phantom: core::marker::PhantomData,
        }
    }

    async fn apply_fan_speed(&self, data: &[u8]) -> Result<(), SetReportError> {
        let fan = self
            .service
            .fan(DEFAULT_INSTANCE_ID)
            .ok_or(SetReportError::MissingFan)?;

        match serialization::deserialize_fan_speed_report(data)? {
            FanSpeed::Automatic => fan.enable_auto_control().await?,
            FanSpeed::Rpm(0) => fan.stop().await?,
            FanSpeed::Rpm(rpm) => fan.set_rpm(rpm).await?,
        }

        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum SetReportError {
    Malformed,
    MissingFan,
    Fan,
}

impl From<serialization::ReportError> for SetReportError {
    fn from(_: serialization::ReportError) -> Self {
        Self::Malformed
    }
}

impl From<thermal_service_interface::fan::Error> for SetReportError {
    fn from(_: thermal_service_interface::fan::Error) -> Self {
        Self::Fan
    }
}

impl<'s, Service: ThermalService> embedded_services::relay::hid::HidDevice for ThermalHidRelay<'s, Service> {
    type InputReportMaxSize = typenum::U4;
    type OutputReportMaxSize = typenum::U4;
    type FeatureReportMaxSize = typenum::U0;

    const MAX_REPORT_COUNT: u8 = 2;
    const MAX_DESCRIPTOR_LEN: usize = serialization::THERMAL_HID_DESCRIPTOR.len();

    fn report_descriptor(&self) -> &HidReportDescriptor<'_> {
        &self.descriptor
    }

    async fn process_get_report<R>(
        &mut self,
        report_type: GetHidReportType,
        report_id: ReportId,
        process_report: impl AsyncFnOnce(GetHidReport<'_>) -> R,
    ) -> Result<R, HidError> {
        let GetHidReportType::Input = report_type else {
            return Err(HidError::TriggerReset);
        };

        let report = match InputReportId::try_from(report_id).map_err(|_| HidError::TriggerReset)? {
            InputReportId::FanSpeed => {
                info!("Received command to get fan speed report");
                let fan = self.service.fan(DEFAULT_INSTANCE_ID).ok_or_else(|| {
                    error!("No fan is registered at thermal service instance 0");
                    HidError::TriggerReset
                })?;
                serialization::serialize_fan_speed_report(fan.rpm().await)
            }
            InputReportId::SensorTemperature => {
                info!("Received command to get sensor temperature report");
                let sensor = self.service.sensor(DEFAULT_INSTANCE_ID).ok_or_else(|| {
                    error!("No sensor is registered at thermal service instance 0");
                    HidError::TriggerReset
                })?;
                serialization::serialize_sensor_temperature_report(
                    DeciKelvin::from_celsius(sensor.temperature().await).0,
                )
            }
        };

        Ok(process_report(GetHidReport::Input(HidReport::new(report_id, &report))).await)
    }

    async fn set_report(&mut self, report: &SetHidReport<'_>) -> Result<(), HidError> {
        let SetHidReport::Output(report) = report else {
            return Err(HidError::TriggerReset);
        };

        match OutputReportId::try_from(report.id()).map_err(|_| HidError::TriggerReset)? {
            OutputReportId::FanSpeed => {
                info!("Received command to set fan speed report");
                if let Err(e) = self.apply_fan_speed(report.data()).await {
                    error!("Failed to apply fan speed report: {:?}", e);
                }
            }
        }

        Ok(())
    }

    async fn wait_for_input_report(&mut self) {
        core::future::pending::<()>().await;
    }

    async fn process_next_input_report<R>(
        &mut self,
        _process_report: impl AsyncFnOnce(HidReport<'_>) -> R,
    ) -> Result<R, HidError> {
        core::future::pending::<()>().await;
        Err(HidError::TriggerReset)
    }

    fn has_pending_input_report(&mut self) -> bool {
        false
    }

    async fn set_power_state(&mut self, state: HidDevicePowerState) -> Result<(), HidError> {
        info!("Received command to set power state to {:?}", state);
        Ok(())
    }

    async fn reset(&mut self) {
        info!("Received reset command");
    }
}
