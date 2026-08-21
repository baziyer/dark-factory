use std::process::Command;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use dark_factory_control_plane::{
    ProvisionError, production_app_from_env, provision_runtime_from_env,
};
use tower::ServiceExt as _;

const CHILD_ENV: &str = "DARK_FACTORY_PRODUCTION_ENV_TEST_CHILD";
const EXPECTED_ENV: &str = "DARK_FACTORY_PRODUCTION_ENV_TEST_EXPECTED";
const OWNER_ENV: &str = "DARK_FACTORY_PRODUCTION_ENV_TEST_OWNER";

#[test]
fn only_production_metadata_activates_and_managed_postgres_aliases_have_no_authority() {
    for (vercel_environment, owner_environment, expected) in [
        (Some("production"), false, "unavailable"),
        (Some("production"), true, "inactive"),
        (Some("preview"), false, "inactive"),
        (Some("development"), false, "inactive"),
        (Some("unexpected"), false, "inactive"),
        (None, false, "inactive"),
    ] {
        let mut command = child_command(expected);
        if owner_environment {
            command
                .env(OWNER_ENV, "1")
                .env(
                    "DATABASE_URL",
                    "postgresql://exact-owner:exact-owner-password@127.0.0.1:1/exact-database?sslmode=require",
                )
                .env("PGAPPNAME", "poison-application")
                .env("PGDATABASE", "poison-database")
                .env("PGHOST", "poison.invalid")
                .env("PGHOSTADDR", "203.0.113.1")
                .env("PGPASSFILE", "/definitely/not/a/passfile")
                .env("PGPASSWORD", "poison-password")
                .env("PGPORT", "65535")
                .env("PGSSLMODE", "disable")
                .env("PGUSER", "poison-user")
                .env("DATABASE_URL_UNPOOLED", "poison-unpooled")
                .env("NEON_PROJECT_ID", "poison-project")
                .env("POSTGRES_USER", "poison-user")
                .env("POSTGRES_URL", "poison-url")
                .env("POSTGRES_DATABASE", "poison-database")
                .env("POSTGRES_PRISMA_URL", "poison-prisma")
                .env("POSTGRES_URL_NON_POOLING", "poison-non-pooling")
                .env("POSTGRES_URL_NO_SSL", "poison-no-ssl")
                .env("POSTGRES_PASSWORD", "poison-password")
                .env("POSTGRES_HOST", "poison-host");
        } else {
            command.env_remove(OWNER_ENV);
        }
        if let Some(environment) = vercel_environment {
            command.env("VERCEL_ENV", environment);
        } else {
            command.env_remove("VERCEL_ENV");
        }

        assert_child(command, &format!("VERCEL_ENV={vercel_environment:?}"));
    }
}

#[test]
fn every_managed_owner_alias_individually_keeps_production_inactive() {
    for name in managed_owner_aliases() {
        let mut command = child_command("inactive");
        command
            .env("VERCEL_ENV", "production")
            .env_remove(OWNER_ENV)
            .env(name, "poison-owner-authority");
        assert_child(command, name);
    }
}

fn child_command(expected: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "environment_child_observes_platform_and_database_boundaries",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .env(EXPECTED_ENV, expected)
        .env(
            "DARK_FACTORY_BROKER_DATABASE_URL",
            "postgresql://exact-user:exact-password@127.0.0.1:1/exact-database?sslmode=require",
        )
        .env(
            "DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET",
            "0123456789abcdef0123456789abcdef",
        )
        .env(
            "DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION",
            "maintainer-v1",
        )
        .env("DARK_FACTORY_MAINTAINER_APP_ID", "5678")
        .env_remove(OWNER_ENV);
    for name in managed_owner_aliases() {
        command.env_remove(name);
    }
    command
}

fn managed_owner_aliases() -> &'static [&'static str] {
    &[
        "DATABASE_URL",
        "DATABASE_URL_UNPOOLED",
        "NEON_PROJECT_ID",
        "PGAPPNAME",
        "PGDATABASE",
        "PGHOST",
        "PGHOSTADDR",
        "PGPASSFILE",
        "PGPASSWORD",
        "PGPORT",
        "PGSSLMODE",
        "PGUSER",
        "PGOPTIONS",
        "PGSSLCERT",
        "PGSSLKEY",
        "PGSSLROOTCERT",
        "POSTGRES_USER",
        "POSTGRES_URL",
        "POSTGRES_DATABASE",
        "POSTGRES_PRISMA_URL",
        "POSTGRES_URL_NON_POOLING",
        "POSTGRES_URL_NO_SSL",
        "POSTGRES_PASSWORD",
        "POSTGRES_HOST",
    ]
}

fn assert_child(mut command: Command, context: &str) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "child failed for {context}:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[tokio::test]
async fn environment_child_observes_platform_and_database_boundaries() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }

    let expected = std::env::var(EXPECTED_ENV).unwrap();
    let response = production_app_from_env()
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(
        body.as_ref(),
        format!(
            r#"{{"status":"{expected}","maintainer_webhook":"inactive","product_webhook":"inactive","operator_api":"inactive"}}"#
        )
        .as_bytes()
    );

    if std::env::var_os(OWNER_ENV).is_some() {
        // Bootstrap accepts the provider's aliases but takes its authority
        // from the explicit owner URL; the function itself stays inactive.
        assert_eq!(
            provision_runtime_from_env().await,
            Err(ProvisionError::Database)
        );
    }
}
