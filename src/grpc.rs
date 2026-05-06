use anyhow::Result;
use protos_rust::control::v1::{
    AckMotorCommandEventRequest, AckMotorCommandEventResponse, MotorCommand,
    MotorCommandReasonCode, MotorCommandStatus, PullPendingMotorCommandsRequest,
    PullPendingMotorCommandsResponse,
};
use protos_rust::garden::v2::{InsertSensorDataRequest, InsertSensorDataResponse};
use tonic::client::Grpc;
use tonic::codec::{BufferSettings, Codec};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::codegen::GrpcMethod;
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

const API_URL: &str = env!("API_URL");
const GRPC_CONNECT_TIMEOUT_SECS: u64 = 4;
const GRPC_RPC_TIMEOUT_SECS: u64 = 6;
const GRPC_CHANNEL_BUFFER_SIZE: usize = 8;
const GRPC_CONCURRENCY_LIMIT: usize = 1;
const GRPC_MAX_ENCODING_MESSAGE_SIZE: usize = 512;
const GRPC_MAX_DECODING_MESSAGE_SIZE: usize = 4096;
const GRPC_HTTP2_INITIAL_WINDOW_SIZE: u32 = 16 * 1024;
const GRPC_HTTP2_MAX_HEADER_LIST_SIZE: u32 = 4 * 1024;
const GRPC_CODEC_BUFFER_SIZE: usize = 256;
const GRPC_CODEC_YIELD_THRESHOLD: usize = 1024;

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
    grpc: Grpc<InterceptedService<Channel, AuthInterceptor>>,
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
            .connect_timeout(std::time::Duration::from_secs(GRPC_CONNECT_TIMEOUT_SECS))
            .timeout(std::time::Duration::from_secs(GRPC_RPC_TIMEOUT_SECS))
            .buffer_size(GRPC_CHANNEL_BUFFER_SIZE)
            .concurrency_limit(GRPC_CONCURRENCY_LIMIT)
            .initial_stream_window_size(GRPC_HTTP2_INITIAL_WINDOW_SIZE)
            .initial_connection_window_size(GRPC_HTTP2_INITIAL_WINDOW_SIZE)
            .http2_max_header_list_size(GRPC_HTTP2_MAX_HEADER_LIST_SIZE)
            .connect()
            .await?;

        let metadata: MetadataValue<_> = format!("Bearer {}", token).parse()?;
        let interceptor = AuthInterceptor { token: metadata };
        let service = InterceptedService::new(channel, interceptor);
        let grpc = Grpc::new(service)
            .max_encoding_message_size(GRPC_MAX_ENCODING_MESSAGE_SIZE)
            .max_decoding_message_size(GRPC_MAX_DECODING_MESSAGE_SIZE);

        Ok(Self { grpc })
    }

    pub async fn send_data(&mut self, data: SensorData<'_>) -> Result<()> {
        let resp = self
            .unary::<InsertSensorDataRequest, InsertSensorDataResponse>(
                InsertSensorDataRequest {
                    node_id: data.node_id.to_string(),
                    air_temperature: data.air_temperature,
                    air_pressure: data.air_pressure,
                    air_humidity: data.air_humidity,
                    soil_temperature: data.soil_temperature,
                    soil_humidity: data.soil_humidity,
                    timestamp: data.timestamp,
                },
                "/garden.v2.GardenService/InsertSensorData",
                "garden.v2.GardenService",
                "InsertSensorData",
            )
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
            .unary::<PullPendingMotorCommandsRequest, PullPendingMotorCommandsResponse>(
                PullPendingMotorCommandsRequest {
                    hub_id: hub_id.to_string(),
                    max_commands,
                    lease_duration_ms,
                },
                "/control.v1.ControlService/PullPendingMotorCommands",
                "control.v1.ControlService",
                "PullPendingMotorCommands",
            )
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
            .unary::<AckMotorCommandEventRequest, AckMotorCommandEventResponse>(
                AckMotorCommandEventRequest {
                    command_id: command_id.to_string(),
                    hub_id: hub_id.to_string(),
                    node_id: node_id.to_string(),
                    status: status as i32,
                    reason_code: reason_code as i32,
                    reason_message: reason_message.to_string(),
                },
                "/control.v1.ControlService/AckMotorCommandEvent",
                "control.v1.ControlService",
                "AckMotorCommandEvent",
            )
            .await?
            .into_inner();

        Ok(resp.command)
    }

    async fn unary<Req, Resp>(
        &mut self,
        request: Req,
        path: &'static str,
        service_name: &'static str,
        method_name: &'static str,
    ) -> Result<tonic::Response<Resp>, Status>
    where
        Req: prost::Message + Send + Sync + 'static,
        Resp: prost::Message + Default + Send + Sync + 'static,
    {
        self.grpc
            .ready()
            .await
            .map_err(|e| tonic::Status::unknown(format!("Service was not ready: {e}")))?;

        let codec = small_buffer_codec::<Req, Resp>();
        let path = PathAndQuery::from_static(path);
        let mut req = Request::new(request);
        req.extensions_mut()
            .insert(GrpcMethod::new(service_name, method_name));

        self.grpc.unary(req, path, codec).await
    }
}

fn small_buffer_codec<Req, Resp>() -> SmallBufferProstCodec<Req, Resp>
where
    Req: prost::Message + Send + 'static,
    Resp: prost::Message + Default + Send + 'static,
{
    SmallBufferProstCodec::default()
}

#[derive(Clone, Copy, Debug)]
struct SmallBufferProstCodec<Req, Resp> {
    _marker: core::marker::PhantomData<(Req, Resp)>,
}

impl<Req, Resp> Default for SmallBufferProstCodec<Req, Resp> {
    fn default() -> Self {
        Self {
            _marker: core::marker::PhantomData,
        }
    }
}

impl<Req, Resp> Codec for SmallBufferProstCodec<Req, Resp>
where
    Req: prost::Message + Send + 'static,
    Resp: prost::Message + Default + Send + 'static,
{
    type Encode = Req;
    type Decode = Resp;
    type Encoder = <tonic_prost::ProstCodec<Req, Resp> as Codec>::Encoder;
    type Decoder = <tonic_prost::ProstCodec<Req, Resp> as Codec>::Decoder;

    fn encoder(&mut self) -> Self::Encoder {
        tonic_prost::ProstCodec::<Req, Resp>::raw_encoder(small_buffer_settings())
    }

    fn decoder(&mut self) -> Self::Decoder {
        tonic_prost::ProstCodec::<Req, Resp>::raw_decoder(small_buffer_settings())
    }
}

fn small_buffer_settings() -> BufferSettings {
    BufferSettings::new(GRPC_CODEC_BUFFER_SIZE, GRPC_CODEC_YIELD_THRESHOLD)
}
