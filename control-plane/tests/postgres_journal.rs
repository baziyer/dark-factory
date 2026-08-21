use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use dark_factory_control_plane::{BrokerState, app};
use hmac::{Hmac, Mac as _};
use sha2::Sha256;
use sqlx::{Executor as _, PgPool, postgres::PgPoolOptions};
use tower::ServiceExt as _;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

#[tokio::test]
#[ignore = "requires explicit disposable owner and prepared runtime database URLs"]
async fn migrated_postgres_proves_readiness_concurrent_replay_and_conflict() {
    let database_url = std::env::var("DATABASE_URL").expect("disposable DATABASE_URL");
    let runtime_url = std::env::var("DARK_FACTORY_TEST_RUNTIME_DATABASE_URL")
        .expect("prepared disposable DARK_FACTORY_TEST_RUNTIME_DATABASE_URL");
    let owner_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let (owner_is_superuser, owner_can_create_roles): (bool, bool) = sqlx::query_as(
        "SELECT rolsuper, rolcreaterole
         FROM pg_catalog.pg_roles
         WHERE rolname = current_user",
    )
    .fetch_one(&owner_pool)
    .await
    .unwrap();
    assert!(
        !owner_is_superuser && owner_can_create_roles,
        "the disposable proof requires a non-superuser CREATEROLE owner"
    );

    let owner = runtime_router(&database_url)
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(owner.status(), StatusCode::SERVICE_UNAVAILABLE);

    let router = runtime_router(&runtime_url);
    assert_ready(&router, StatusCode::OK).await;

    exact_concurrent_replay(router.clone()).await;
    concurrent_conflict(router.clone()).await;

    let runtime_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&runtime_url)
        .await
        .unwrap();
    forbidden_runtime_operations_fail(&runtime_pool, &owner_pool).await;
    corrupted_schema_and_authority_fail_readiness(&router, &owner_pool, &runtime_url).await;
}

fn runtime_router(database_url: &str) -> Router {
    app(BrokerState::open_production(
        database_url,
        SECRET.to_vec(),
        "maintainer-v1".to_owned(),
        5678,
    )
    .unwrap())
}

async fn assert_ready(router: &Router, expected: StatusCode) {
    let response = router
        .clone()
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), expected);
    if expected == StatusCode::OK {
        assert_eq!(
            to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .as_ref(),
            br#"{"status":"ready","maintainer_webhook":"bootstrap_ping_only","product_webhook":"inactive","operator_api":"inactive"}"#
        );
    }
}

async fn forbidden_runtime_operations_fail(runtime: &PgPool, owner: &PgPool) {
    let owner_role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(owner)
        .await
        .unwrap();
    let set_owner_role: String = sqlx::query_scalar("SELECT format('SET ROLE %I', $1)")
        .bind(owner_role)
        .fetch_one(owner)
        .await
        .unwrap();
    for statement in [
        "CREATE SCHEMA forbidden_runtime_schema",
        "CREATE TABLE public.forbidden_runtime_table (id integer)",
        "CREATE TEMPORARY TABLE forbidden_runtime_temp (id integer)",
        "UPDATE public.maintainer_deliveries SET event = 'ping'",
        "DELETE FROM public.maintainer_deliveries",
        "TRUNCATE public.maintainer_deliveries",
        "ALTER TABLE public.maintainer_deliveries ADD COLUMN forbidden integer",
        "DROP TABLE public.maintainer_deliveries",
        "CREATE ROLE forbidden_runtime_role",
        "CREATE DATABASE forbidden_runtime_database",
        &set_owner_role,
    ] {
        assert!(
            runtime.execute(statement).await.is_err(),
            "succeeded: {statement}"
        );
    }

    // PostgreSQL reports an unauthorized GRANT as a successful no-op with a
    // warning, so prove that it cannot change the ACL rather than expecting an
    // error result.
    runtime
        .execute("GRANT SELECT ON public.maintainer_deliveries TO PUBLIC")
        .await
        .unwrap();
    let public_select: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
             FROM pg_catalog.pg_class relation,
                  LATERAL pg_catalog.aclexplode(
                      COALESCE(relation.relacl, pg_catalog.acldefault('r', relation.relowner))
                  ) acl
             WHERE relation.oid = 'public.maintainer_deliveries'::regclass
               AND acl.grantee = 0
               AND acl.privilege_type = 'SELECT'
         )",
    )
    .fetch_one(owner)
    .await
    .unwrap();
    assert!(!public_select);
}

async fn corrupted_schema_and_authority_fail_readiness(
    router: &Router,
    owner: &PgPool,
    runtime_url: &str,
) {
    owner
        .execute("CREATE ROLE dark_factory_broker_forbidden_database_grantee NOLOGIN")
        .await
        .unwrap();
    let (grant_unexpected_database_acl, revoke_unexpected_database_acl): (String, String) =
        sqlx::query_as(
            "SELECT
                 format(
                     'GRANT CONNECT ON DATABASE %I TO dark_factory_broker_forbidden_database_grantee',
                     current_database()
                 ),
                 format(
                     'REVOKE ALL ON DATABASE %I FROM dark_factory_broker_forbidden_database_grantee',
                     current_database()
                 )",
        )
        .fetch_one(owner)
        .await
        .unwrap();
    owner
        .execute(grant_unexpected_database_acl.as_str())
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute(revoke_unexpected_database_acl.as_str())
        .await
        .unwrap();
    owner
        .execute("DROP ROLE dark_factory_broker_forbidden_database_grantee")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    let (grant_connect, revoke_connect): (String, String) = sqlx::query_as(
        "SELECT
             format(
                 'GRANT CONNECT ON DATABASE %I TO dark_factory_broker_runtime WITH GRANT OPTION',
                 current_database()
             ),
             format(
                 'REVOKE GRANT OPTION FOR CONNECT ON DATABASE %I FROM dark_factory_broker_runtime',
                 current_database()
             )",
    )
    .fetch_one(owner)
    .await
    .unwrap();
    owner.execute(grant_connect.as_str()).await.unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner.execute(revoke_connect.as_str()).await.unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "GRANT USAGE ON SCHEMA public
             TO dark_factory_broker_runtime WITH GRANT OPTION",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute(
            "REVOKE GRANT OPTION FOR USAGE ON SCHEMA public
             FROM dark_factory_broker_runtime",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute("GRANT UPDATE ON public.maintainer_deliveries TO dark_factory_broker_runtime")
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("REVOKE UPDATE ON public.maintainer_deliveries FROM dark_factory_broker_runtime")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute("GRANT SELECT ON public.maintainer_deliveries TO PUBLIC")
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("REVOKE SELECT ON public.maintainer_deliveries FROM PUBLIC")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute("CREATE ROLE dark_factory_broker_forbidden_grantee NOLOGIN")
        .await
        .unwrap();
    owner
        .execute(
            "GRANT SELECT ON public.maintainer_deliveries
             TO dark_factory_broker_forbidden_grantee",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute(
            "REVOKE SELECT ON public.maintainer_deliveries
             FROM dark_factory_broker_forbidden_grantee",
        )
        .await
        .unwrap();
    owner
        .execute("DROP ROLE dark_factory_broker_forbidden_grantee")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute("GRANT CREATE ON SCHEMA public TO dark_factory_broker_runtime")
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("REVOKE CREATE ON SCHEMA public FROM dark_factory_broker_runtime")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "GRANT UPDATE (event) ON public.maintainer_deliveries
             TO dark_factory_broker_runtime",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute(
            "REVOKE UPDATE (event) ON public.maintainer_deliveries
             FROM dark_factory_broker_runtime",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "CREATE TABLE public.forbidden_runtime_relation (id integer);
             GRANT SELECT ON public.forbidden_runtime_relation TO dark_factory_broker_runtime",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("DROP TABLE public.forbidden_runtime_relation")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "CREATE TABLE public.forbidden_runtime_column_relation (id integer);
             GRANT SELECT (id) ON public.forbidden_runtime_column_relation
             TO dark_factory_broker_runtime",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("DROP TABLE public.forbidden_runtime_column_relation")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "CREATE FUNCTION public.forbidden_runtime_function()
             RETURNS integer LANGUAGE SQL IMMUTABLE AS 'SELECT 1'",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("DROP FUNCTION public.forbidden_runtime_function()")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute("ALTER TABLE public.maintainer_deliveries ENABLE ROW LEVEL SECURITY")
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("ALTER TABLE public.maintainer_deliveries DISABLE ROW LEVEL SECURITY")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute("ALTER TABLE public.maintainer_deliveries SET UNLOGGED")
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("ALTER TABLE public.maintainer_deliveries SET LOGGED")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "CREATE SCHEMA dark_factory_broker_owner_test;
             CREATE FUNCTION dark_factory_broker_owner_test.forbidden_trigger()
             RETURNS trigger LANGUAGE plpgsql AS $function$
             BEGIN
                 RETURN NULL;
             END
             $function$;
             CREATE TRIGGER forbidden_runtime_trigger
             BEFORE INSERT ON public.maintainer_deliveries
             FOR EACH ROW EXECUTE FUNCTION dark_factory_broker_owner_test.forbidden_trigger()",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute(
            "DROP TRIGGER forbidden_runtime_trigger ON public.maintainer_deliveries;
             DROP SCHEMA dark_factory_broker_owner_test CASCADE",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "CREATE RULE forbidden_runtime_rule AS
             ON INSERT TO public.maintainer_deliveries DO INSTEAD NOTHING",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("DROP RULE forbidden_runtime_rule ON public.maintainer_deliveries")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "CREATE TABLE public.forbidden_delivery_child ()
             INHERITS (public.maintainer_deliveries)",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("DROP TABLE public.forbidden_delivery_child")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "CREATE TABLE public.forbidden_migration_child ()
             INHERITS (public.dark_factory_schema_migrations)",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("DROP TABLE public.forbidden_migration_child")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "ALTER TABLE public.maintainer_deliveries
             DROP CONSTRAINT maintainer_deliveries_pkey",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute(
            "ALTER TABLE public.maintainer_deliveries
             ADD CONSTRAINT maintainer_deliveries_pkey PRIMARY KEY (delivery_id)",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "ALTER TABLE public.dark_factory_schema_migrations
             DROP CONSTRAINT dark_factory_schema_migrations_pkey",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute(
            "ALTER TABLE public.dark_factory_schema_migrations
             ADD CONSTRAINT dark_factory_schema_migrations_pkey PRIMARY KEY (component)",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "ALTER TABLE public.dark_factory_schema_migrations
             DROP CONSTRAINT dark_factory_schema_migrations_digest_format;
             ALTER TABLE public.dark_factory_schema_migrations
             ADD CONSTRAINT dark_factory_schema_migrations_digest_format
             CHECK (octet_length(digest) > 0)",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute(
            "ALTER TABLE public.dark_factory_schema_migrations
             DROP CONSTRAINT dark_factory_schema_migrations_digest_format;
             ALTER TABLE public.dark_factory_schema_migrations
             ADD CONSTRAINT dark_factory_schema_migrations_digest_format
             CHECK (digest ~ '^[0-9a-f]{64}$')",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "ALTER TABLE public.maintainer_deliveries
             DROP CONSTRAINT maintainer_deliveries_hook_id_positive;
             ALTER TABLE public.maintainer_deliveries
             ADD CONSTRAINT maintainer_deliveries_hook_id_positive CHECK (hook_id >= 0)",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute(
            "ALTER TABLE public.maintainer_deliveries
             DROP CONSTRAINT maintainer_deliveries_hook_id_positive;
             ALTER TABLE public.maintainer_deliveries
             ADD CONSTRAINT maintainer_deliveries_hook_id_positive CHECK (hook_id > 0)",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "ALTER TABLE public.maintainer_deliveries
             ALTER COLUMN received_at SET DEFAULT statement_timestamp()",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute(
            "ALTER TABLE public.maintainer_deliveries
             ALTER COLUMN received_at SET DEFAULT now()",
        )
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute("ALTER ROLE dark_factory_broker_runtime CREATEDB")
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("ALTER ROLE dark_factory_broker_runtime NOCREATEDB")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute("ALTER ROLE dark_factory_broker_runtime CONNECTION LIMIT 3")
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("ALTER ROLE dark_factory_broker_runtime CONNECTION LIMIT -1")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute("ALTER ROLE dark_factory_broker_runtime VALID UNTIL '2099-01-01'")
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("ALTER ROLE dark_factory_broker_runtime VALID UNTIL 'infinity'")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute(
            "ALTER ROLE dark_factory_broker_runtime
             SET default_transaction_read_only = on",
        )
        .await
        .unwrap();
    assert_ready(
        &runtime_router(runtime_url),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    owner
        .execute("ALTER ROLE dark_factory_broker_runtime RESET ALL")
        .await
        .unwrap();
    assert_ready(&runtime_router(runtime_url), StatusCode::OK).await;

    let (set_database_read_only, reset_database_settings): (String, String) =
        sqlx::query_as(
            "SELECT
                 format(
                     'ALTER ROLE dark_factory_broker_runtime IN DATABASE %I SET default_transaction_read_only = on',
                     current_database()
                 ),
                 format(
                     'ALTER ROLE dark_factory_broker_runtime IN DATABASE %I RESET ALL',
                     current_database()
                 )",
        )
        .fetch_one(owner)
        .await
        .unwrap();
    owner
        .execute(set_database_read_only.as_str())
        .await
        .unwrap();
    assert_ready(
        &runtime_router(runtime_url),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    owner
        .execute(reset_database_settings.as_str())
        .await
        .unwrap();
    assert_ready(&runtime_router(runtime_url), StatusCode::OK).await;

    owner
        .execute("CREATE ROLE dark_factory_broker_forbidden_parent NOLOGIN")
        .await
        .unwrap();
    owner
        .execute("GRANT dark_factory_broker_forbidden_parent TO dark_factory_broker_runtime")
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("REVOKE dark_factory_broker_forbidden_parent FROM dark_factory_broker_runtime")
        .await
        .unwrap();
    owner
        .execute("DROP ROLE dark_factory_broker_forbidden_parent")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;

    owner
        .execute("CREATE ROLE dark_factory_broker_forbidden_member NOLOGIN")
        .await
        .unwrap();
    owner
        .execute("GRANT dark_factory_broker_runtime TO dark_factory_broker_forbidden_member")
        .await
        .unwrap();
    assert_ready(router, StatusCode::SERVICE_UNAVAILABLE).await;
    owner
        .execute("REVOKE dark_factory_broker_runtime FROM dark_factory_broker_forbidden_member")
        .await
        .unwrap();
    owner
        .execute("DROP ROLE dark_factory_broker_forbidden_member")
        .await
        .unwrap();
    assert_ready(router, StatusCode::OK).await;
}

async fn exact_concurrent_replay(router: Router) {
    let body = br#"{"zen":"postgres concurrent replay"}"#;
    let delivery = "cc8a5c44-7f1f-11f0-952e-acde48001122";
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let router = router.clone();
        let request = signed_request(delivery, body);
        tasks.push(tokio::spawn(async move {
            router.oneshot(request).await.unwrap().status()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
}

async fn concurrent_conflict(router: Router) {
    let delivery = "dc8a5c44-7f1f-11f0-952e-acde48001122";
    let first = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .oneshot(signed_request(delivery, br#"{"zen":"first"}"#))
                .await
                .unwrap()
                .status()
        }
    });
    let second = tokio::spawn(async move {
        router
            .oneshot(signed_request(delivery, br#"{"zen":"second"}"#))
            .await
            .unwrap()
            .status()
    });
    let mut statuses = [first.await.unwrap(), second.await.unwrap()];
    statuses.sort_unstable();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
}

fn signed_request(delivery: &str, body: &[u8]) -> Request<Body> {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
    mac.update(body);
    Request::post("/v1/github/maintainer/webhook")
        .header("content-type", "application/json")
        .header("x-github-event", "ping")
        .header("x-github-delivery", delivery)
        .header("x-github-hook-id", "1234")
        .header("x-github-hook-installation-target-id", "5678")
        .header("x-github-hook-installation-target-type", "integration")
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
        )
        .body(Body::from(body.to_vec()))
        .unwrap()
}
