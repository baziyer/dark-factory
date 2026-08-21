#[test]
fn production_sql_never_carries_a_runtime_password() {
    let journal = include_str!("../src/journal.rs");
    assert!(journal.contains("NOLOGIN PASSWORD NULL"));
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
    assert!(!journal.contains("PASSWORD %L"));
    assert!(!journal.contains("WITH LOGIN PASSWORD"));
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
    let contract_audit = activation
        .find("postgres_contract_is_exact")
        .expect("schema, ACL, and migration audit");
    let provider_acl_normalization = activation
        .find("normalize_neon_provider_database_acl")
        .expect("post-reset provider ACL normalization");
    let default_acl_audit = activation
        .find("default_privileges_are_exact")
        .expect("default ACL audit");
    let enable_login = activation
        .find("ALTER ROLE dark_factory_broker_runtime LOGIN")
        .expect("fixed login activation");
    assert!(provider_acl_normalization < contract_audit);
    assert!(contract_audit < default_acl_audit);
    assert!(default_acl_audit < enable_login);
}

#[test]
fn provider_acl_is_normalized_before_and_after_the_reset_boundary() {
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
fn reset_is_bracketed_by_distinct_owner_pools() {
    let provision = include_str!("../src/lib.rs")
        .split("pub async fn provision_runtime_from_env")
        .nth(1)
        .expect("provisioning function")
        .split("fn runtime_database_url")
        .next()
        .expect("provisioning function body");
    let close_preparation_pool = provision
        .find("preparation_pool.close().await")
        .expect("preparation pool close");
    let reset = provision
        .find("reset_runtime_password")
        .expect("password reset");
    let fresh_activation_pool = provision
        .find("let activation_pool")
        .expect("fresh activation pool");
    assert!(close_preparation_pool < reset);
    assert!(reset < fresh_activation_pool);
}

#[test]
fn operator_helper_isolated_environment_and_cleanup_are_fail_closed() {
    let helper = include_str!("../scripts/provision-production.sh");
    let cleanup = helper
        .find("trap clear_clipboard EXIT")
        .expect("EXIT cleanup");
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
}

#[test]
fn neon_http_client_is_absent_from_the_default_feature_set() {
    let manifest = include_str!("../Cargo.toml");
    assert!(manifest.contains("provision-runtime = [\"dep:reqwest\", \"dep:serde\"]"));
    assert!(manifest.contains("reqwest = {"));
    assert!(manifest.contains("optional = true"));
    assert!(manifest.contains("required-features = [\"provision-runtime\"]"));
    assert!(!include_str!("../.env.example").contains("DARK_FACTORY_NEON_API_KEY"));
}
