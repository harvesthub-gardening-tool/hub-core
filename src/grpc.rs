use anyhow::Result;
use protos_rust::auth::v1::{auth_service_client::AuthServiceClient, LoginRequest};
use protos_rust::garden::v1::{
    garden_service_client::GardenServiceClient,
    InsertSensorDataRequest,
};
use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

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
    pub async fn new(server_url: &str, email: &str, password: &str) -> Result<Self> {
        let channel = Endpoint::from_shared(server_url.to_string())?
            .connect()
            .await?;

        let mut auth_client = AuthServiceClient::new(channel.clone());

        let login_resp = auth_client
            .login(LoginRequest {
                email: email.to_string(),
                password: password.to_string(),
            })
            .await?
            .into_inner();

        let token: MetadataValue<_> = format!("Bearer {}", login_resp.token).parse()?;
        let interceptor = AuthInterceptor { token };

        let service = InterceptedService::new(channel, interceptor);
        let garden_client = GardenServiceClient::new(service);

        Ok(Self { garden_client })
    }

    pub async fn send_data(
        &mut self,
        node_id: &str,
        temperature: f64,
        humidity: f64,
        soil_moisture: f64,
        timestamp: i64,
    ) -> Result<()> {
        self.garden_client
            .insert_sensor_data(InsertSensorDataRequest {
                node_id: node_id.to_string(),
                temperature,
                humidity,
                soil_moisture,
                timestamp,
            })
            .await?;

        Ok(())
    }
}