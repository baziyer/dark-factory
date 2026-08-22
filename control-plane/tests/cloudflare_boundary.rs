use std::{fs, path::Path};

fn project_file(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

#[test]
fn production_runtime_is_cloudflare_only() {
    let manifest = project_file("Cargo.toml");

    assert!(manifest.contains("worker = { version = \"=0.8.5\""));
    for removed_dependency in ["vercel_runtime", "sqlx", "reqwest", "url ="] {
        assert!(
            !manifest.contains(removed_dependency),
            "removed production dependency remains: {removed_dependency}"
        );
    }

    for removed_path in [
        "api/broker.rs",
        "tools/runtime_bootstrap.rs",
        "scripts/bootstrap-production.sh",
        "src/neon.rs",
        "vercel.json",
        ".vercelignore",
        ".env.example",
    ] {
        assert!(
            !Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(removed_path)
                .exists(),
            "removed deployment path remains: {removed_path}"
        );
    }
}

#[test]
fn wrangler_keeps_the_unconfigured_worker_private_and_inert() {
    let wrangler = project_file("wrangler.toml");

    assert!(wrangler.contains("workers_dev = false"));
    assert!(wrangler.contains("preview_urls = false"));
    assert!(wrangler.contains("class_name = \"MaintainerDeliveryJournal\""));
    assert!(wrangler.contains("type = \"durable-object\""));
    assert!(wrangler.contains("storage = \"sqlite\""));
    for required_secret in [
        "DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET",
        "DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION",
        "DARK_FACTORY_MAINTAINER_APP_ID",
    ] {
        assert!(
            wrangler.contains(required_secret),
            "missing required deployment binding: {required_secret}"
        );
    }
    assert!(!wrangler.contains("route ="));
    assert!(!wrangler.contains("routes ="));
}

#[test]
fn durable_object_is_sharded_by_app_and_delivery_identity() {
    let journal = project_file("src/journal.rs");

    assert!(journal.contains("pub struct MaintainerDeliveryJournal"));
    assert!(journal.contains("DARK_FACTORY_MAINTAINER_DELIVERIES"));
    assert!(journal.contains("delivery_shard_name"));
    assert!(journal.contains("app_id"));
    assert!(journal.contains("delivery_id"));
    assert!(journal.contains("sha256"));
    assert!(!journal.contains("DATABASE_URL"));
    assert!(!journal.contains("neon_superuser"));
}
