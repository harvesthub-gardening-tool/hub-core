use anyhow::Result;
use protos_rust::control::v1::{
    control_service_client::ControlServiceClient, AckMotorCommandEventRequest, MotorCommand,
    MotorCommandReasonCode, MotorCommandStatus, PullPendingMotorCommandsRequest,
};
use protos_rust::garden::v2::{
    garden_service_client::GardenServiceClient, InsertSensorDataRequest,
};
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

const API_URL: &str = env!("API_URL");
const GRPC_CHANNEL_BUFFER_SIZE: usize = 8;
const GRPC_CONCURRENCY_LIMIT: usize = 1;
const GRPC_MAX_ENCODING_MESSAGE_SIZE: usize = 512;
const GRPC_MAX_DECODING_MESSAGE_SIZE: usize = 4096;

#[derive(Clone)]
struct AuthInterceptor {
    token: MetadataValue<tonic::metadata::Ascii>,
}

impl Interceptor for AuthInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        req.metadata_mut()
            .insert("authorization", self.token.clone());
        Ok(req)
    }
}

pub struct HubClient {
    garden_client: GardenServiceClient<InterceptedService<Channel, AuthInterceptor>>,
    control_client: ControlServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

pub struct SensorData<'a> {
    pub node_id: &'a str,
    pub air_temperature: f64,
    pub air_pressure: f64,
    pub air_humidity: f64,
    pub soil_temperature: f64,
    pub soil_humidity: f64,
    pub timestamp: i64,
}

impl HubClient {
    pub async fn connect_with_token(token: &str) -> Result<Self> {
        let channel = Endpoint::from_static(API_URL)
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(20))
            .buffer_size(GRPC_CHANNEL_BUFFER_SIZE)
            .concurrency_limit(GRPC_CONCURRENCY_LIMIT)
            .connect()
            .await?;

        let metadata: MetadataValue<_> = format!("Bearer {}", token).parse()?;
        let interceptor = AuthInterceptor { token: metadata };
        let service = InterceptedService::new(channel, interceptor);
        let garden_client = GardenServiceClient::new(service.clone())
            .max_encoding_message_size(GRPC_MAX_ENCODING_MESSAGE_SIZE)
            .max_decoding_message_size(GRPC_MAX_DECODING_MESSAGE_SIZE);
        let control_client = ControlServiceClient::new(service)
            .max_encoding_message_size(GRPC_MAX_ENCODING_MESSAGE_SIZE)
            .max_decoding_message_size(GRPC_MAX_DECODING_MESSAGE_SIZE);

        Ok(Self {
            garden_client,
            control_client,
        })
    }

    pub async fn send_data(&mut self, data: SensorData<'_>) -> Result<()> {
        let resp = self
            .garden_client
            .insert_sensor_data(InsertSensorDataRequest {
                node_id: data.node_id.to_string(),
                air_temperature: data.air_temperature,
                air_pressure: data.air_pressure,
                air_humidity: data.air_humidity,
                soil_temperature: data.soil_temperature,
                soil_humidity: data.soil_humidity,
                timestamp: data.timestamp,
            })
            .await?
            .into_inner();

        if !resp.success {
            anyhow::bail!("InsertSensorData rejected by server: {}", resp.message);
        }
        Ok(())
    }

    pub async fn pull_pending_motor_commands(
        &mut self,
        hub_id: &str,
        max_commands: i32,
        lease_duration_ms: i32,
    ) -> Result<Vec<MotorCommand>> {
        let resp = self
            .control_client
            .pull_pending_motor_commands(PullPendingMotorCommandsRequest {
                hub_id: hub_id.to_string(),
                max_commands,
                lease_duration_ms,
            })
            .await?
            .into_inner();

        Ok(resp.commands)
    }

    pub async fn ack_motor_command_event(
        &mut self,
        command_id: &str,
        hub_id: &str,
        node_id: &str,
        status: MotorCommandStatus,
        reason_code: MotorCommandReasonCode,
        reason_message: &str,
    ) -> Result<Option<MotorCommand>> {
        let resp = self
            .control_client
            .ack_motor_command_event(AckMotorCommandEventRequest {
                command_id: command_id.to_string(),
                hub_id: hub_id.to_string(),
                node_id: node_id.to_string(),
                status: status as i32,
                reason_code: reason_code as i32,
                reason_message: reason_message.to_string(),
            })
            .await?
            .into_inner();

        Ok(resp.command)
    }
}
