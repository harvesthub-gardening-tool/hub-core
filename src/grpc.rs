use anyhow::Result;
use protos_rust::garden::v2::{
    garden_service_client::GardenServiceClient, InsertSensorDataRequest,
};
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

const API_URL: &str = env!("API_URL");

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
}

impl HubClient {
    pub async fn connect_with_token(token: &str) -> Result<Self> {
        let channel = Endpoint::from_static(API_URL)
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(20))
            .connect()
            .await?;

        let metadata: MetadataValue<_> = format!("Bearer {}", token).parse()?;
        let interceptor = AuthInterceptor { token: metadata };
        let service = InterceptedService::new(channel, interceptor);
        let garden_client = GardenServiceClient::new(service);

        Ok(Self { garden_client })
    }

    pub async fn send_data(
        &mut self,
        node_id: &str,
        air_temperature: f64,
        air_pressure: f64,
        air_humidity: f64,
        soil_temperature: f64,
        soil_humidity: f64,
        timestamp: i64,
    ) -> Result<()> {
        let resp = self
            .garden_client
            .insert_sensor_data(InsertSensorDataRequest {
                node_id: node_id.to_string(),
                air_temperature,
                air_pressure,
                air_humidity,
                soil_temperature,
                soil_humidity,
                timestamp,
            })
            .await?
            .into_inner();

        if !resp.success {
            anyhow::bail!("InsertSensorData rejected by server: {}", resp.message);
        }
        Ok(())
    }
}
