#[test]
fn production_sql_never_carries_a_runtime_password() {
    let journal = include_str!("../src/journal.rs");
    let provider_gate = journal
        .find("async fn normalize_neon_provider_database_acl")
        .expect("Neon provider-role gate");
    let provider_acl_revoke = journal
        .find("REVOKE ALL ON DATABASE %I FROM neon_superuser")
        .expect("Neon provider database ACL normalization");
    let provider_gate_source = &journal[provider_gate..provider_acl_revoke];
    assert!(provider_gate_source.contains("provider.rolvaliduntil IS NULL"));
    assert!(!provider_gate_source.contains("rolvaliduntil = 'infinity'"));
    assert!(provider_gate < provider_acl_revoke);
    assert!(!journal.contains("PASSWORD"));
}

#[test]
fn login_follows_the_full_owner_side_contract_audit() {
    let journal = include_str!("../src/journal.rs");
    let activation = journal
        .split("pub(crate) async fn activate_runtime")
        .nth(1)
        .expect("activation function")
        .split("async fn runtime_role_is_exact_for_activation")
        .next()
        .expect("activation function body");
    let provider_acl_normalization = activation
        .find("normalize_neon_provider_database_acl")
        .expect("post-reset provider ACL normalization");
    let contract_audit = activation
        .find("audit_runtime_before_login")
        .expect("full role, schema, ACL, and migration audit");
    let enable_login = activation
        .find("ALTER ROLE dark_factory_broker_runtime LOGIN")
        .expect("fixed login activation");
    assert!(provider_acl_normalization < contract_audit);
    assert!(contract_audit < enable_login);

    let audit = journal
        .split("async fn audit_runtime_before_login")
        .nth(1)
        .expect("audit helper")
        .split("async fn normalize_neon_provider_database_acl")
        .next()
        .expect("audit helper body");
    let role = audit.find("runtime_role_is_exact_for_activation").unwrap();
    let schema = audit.find("postgres_contract_is_exact").unwrap();
    let defaults = audit.find("default_privileges_are_exact").unwrap();
    assert!(role < schema);
    assert!(schema < defaults);
}

#[test]
fn provider_acl_is_normalized_during_staging_and_activation() {
    let journal = include_str!("../src/journal.rs");
    assert_eq!(
        journal
            .matches("normalize_neon_provider_database_acl(&mut transaction)")
            .count(),
        2
    );
}

#[test]
fn not_null_is_pinned_by_columns_not_version_specific_constraint_rows() {
    let journal = include_str!("../src/journal.rs");
    assert!(journal.contains("attribute.attnotnull"));
    assert_eq!(journal.matches("AND contype <> 'n'").count(), 2);
    assert!(journal.contains("&[1, 2, 3, 4, 5, 7, 8, 9, 10]"));
    assert!(journal.contains("&[1, 2, 3, 4]"));
    let audit = journal
        .split("async fn not_null_constraints_are_exact")
        .nth(1)
        .expect("version-aware NOT NULL audit")
        .split("async fn delivery_constraints_are_exact")
        .next()
        .expect("NOT NULL audit body");
    for invariant in [
        "convalidated",
        "conislocal",
        "coninhcount = 0",
        "NOT connoinherit",
        "conparentid = 0",
        "cardinality(conkey) = 1",
        "ELSE NOT EXISTS",
    ] {
        assert!(audit.contains(invariant), "missing {invariant}");
    }
}

#[test]
fn activation_retries_create_owner_pools_and_verify_the_restricted_url() {
    let provision = include_str!("../src/lib.rs")
        .split("pub async fn activate_runtime_from_env")
        .nth(1)
        .expect("activation-only function")
        .split("fn runtime_database_url")
        .next()
        .expect("activation-only function body");
    assert!(provision.contains("retry_with_delays"));
    assert!(provision.contains("journal::neon_owner_pool"));
    assert!(provision.contains("journal::activate_runtime"));
    assert!(provision.contains("journal::verify_runtime"));
    assert!(!provision.contains("recover_runtime_password"));
}

#[test]
fn operator_helper_isolated_environment_and_cleanup_are_fail_closed() {
    let helper = include_str!("../scripts/bootstrap-production.sh");
    let cleanup = helper.find("trap cleanup EXIT").expect("EXIT cleanup");
    let environment_check = helper.find("/.env*(N)").expect("local dotenv rejection");
    let first_vercel = helper.find("/usr/bin/env -i").expect("sanitized Vercel");
    let second_vercel = helper
        .rfind("/usr/bin/env -i")
        .expect("sanitized Vercel sink");
    assert!(cleanup < environment_check);
    assert!(environment_check < first_vercel);
    assert!(first_vercel < second_vercel);
    assert!(helper.contains("/usr/bin/pbpaste"));
    assert!(helper.contains("/usr/bin/pbcopy"));
    let stage = helper
        .split("stage)")
        .nth(1)
        .expect("credential staging mode")
        .split("activate)")
        .next()
        .expect("staging mode body");
    assert!(stage.contains("DARK_FACTORY_BROKER_DATABASE_URL production --sensitive"));
    let recovery = stage
        .find("runtime_url=\"$(\"$1\" credential")
        .expect("successful credential capture");
    let sensitive_write = stage
        .find("DARK_FACTORY_BROKER_DATABASE_URL production --sensitive")
        .expect("sensitive Vercel write");
    assert!(recovery < sensitive_write);
    assert!(stage.contains("set -eu"));
    assert!(stage.contains("setopt pipefail"));
    assert!(!stage.contains("vercel deploy"));
    assert!(!stage.contains("env rm"));
}

#[test]
fn credential_staging_and_activation_are_separate_resumable_operations() {
    let library = include_str!("../src/lib.rs");
    let recover = library
        .split("pub async fn recover_runtime_credential_from_env")
        .nth(1)
        .expect("credential command")
        .split("pub async fn activate_runtime_from_env")
        .next()
        .expect("credential command body");
    assert!(recover.contains("recover_runtime_password"));
    assert!(!recover.contains("journal::activate_runtime"));
    let activate = library
        .split("pub async fn activate_runtime_from_env")
        .nth(1)
        .expect("activation command");
    assert!(activate.contains("journal::activate_runtime"));
    assert!(!activate.contains("recover_runtime_password"));

    let helper = include_str!("../scripts/bootstrap-production.sh");
    assert!(helper.contains("runtime_url=\"$(\"$1\" credential"));
    assert!(helper.contains("\"${bootstrap}\" activate"));
}

#[test]
fn neon_http_client_is_absent_from_the_default_feature_set() {
    let manifest = include_str!("../Cargo.toml");
    assert!(manifest.contains("provision-runtime = [\"dep:reqwest\", \"dep:serde\"]"));
    assert!(manifest.contains("reqwest = {"));
    assert!(manifest.contains("optional = true"));
    assert!(manifest.contains("name = \"runtime-bootstrap\""));
    assert!(manifest.contains("required-features = [\"provision-runtime\"]"));
    assert!(!include_str!("../.env.example").contains("DARK_FACTORY_NEON_API_KEY"));
}
