#![no_std]

use embedded_services::{error, trace};

pub struct BatteryServiceRelayHandler<'hw, const N: usize> {
    service: battery_service::Service<'hw, N>,
}

impl<'hw, const N: usize> BatteryServiceRelayHandler<'hw, N> {
    pub fn new(service: battery_service::Service<'hw, N>) -> Self {
        Self { service }
    }
}

impl<const N: usize> embedded_services::relay::mctp::RelayServiceHandlerTypes for BatteryServiceRelayHandler<'_, N> {
    type RequestType = battery_service_messages::AcpiBatteryRequest;
    type ResultType = battery_service_messages::AcpiBatteryResult;
}

impl<const N: usize> embedded_services::relay::mctp::RelayServiceHandler for BatteryServiceRelayHandler<'_, N> {
    async fn process_request(&self, request: Self::RequestType) -> Self::ResultType {
        trace!("Battery service: ACPI cmd recvd");
        let response = self.service.process_acpi_cmd(request).await;
        if let Err(e) = response {
            error!("Battery service command failed: {:?}", e)
        }
        response
    }
}
