use tower::ServiceBuilder;
use vercel_runtime::{Error, axum::VercelLayer};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let service = ServiceBuilder::new()
        .layer(VercelLayer::new())
        .service(dark_factory_control_plane::production_app_from_env());
    vercel_runtime::run(service).await
}
