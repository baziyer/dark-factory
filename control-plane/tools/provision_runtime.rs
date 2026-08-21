#[tokio::main]
async fn main() {
    match dark_factory_control_plane::provision_runtime_from_env().await {
        Ok(database_url) => println!("{database_url}"),
        Err(_) => {
            eprintln!("control-plane runtime provisioning failed");
            std::process::exit(1);
        }
    }
}
