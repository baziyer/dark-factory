#[tokio::main]
async fn main() {
    if dark_factory_control_plane::migrate_from_env()
        .await
        .is_err()
    {
        eprintln!("control-plane migration failed");
        std::process::exit(1);
    }
}
