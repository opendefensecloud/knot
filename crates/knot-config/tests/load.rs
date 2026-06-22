//! Integration tests for the layered config loader.

// figment::Jail::expect_with's closure signature returns Result<(), figment::Error>;
// figment::Error is sizeable enough to trip clippy::result_large_err. We can't
// change figment's API, so allow at the test crate level.
#![allow(clippy::result_large_err)]

use knot_config::Config;

#[test]
fn defaults_when_no_env_no_file() {
    figment::Jail::expect_with(|_jail| {
        let cfg = Config::load::<&str>(None).expect("load");
        assert_eq!(cfg.addr, ":3000");
        assert_eq!(cfg.env, "development");
        assert!(cfg.database_url.is_empty(), "database_url empty by default");
        assert!(!cfg.tracing_enabled);
        assert_eq!(cfg.log_level, "info");
        Ok(())
    });
}

#[test]
fn env_overrides_defaults() {
    figment::Jail::expect_with(|jail| {
        jail.set_env("KNOT_ADDR", ":9999");
        jail.set_env("KNOT_DATABASE_URL", "postgres://x:y@h/d");
        jail.set_env("KNOT_LOG_LEVEL", "debug");
        let cfg = Config::load::<&str>(None).expect("load");
        assert_eq!(cfg.addr, ":9999");
        assert_eq!(cfg.database_url, "postgres://x:y@h/d");
        assert_eq!(cfg.log_level, "debug");
        Ok(())
    });
}

#[test]
fn file_overrides_defaults_env_overrides_file() {
    figment::Jail::expect_with(|jail| {
        jail.create_file(
            "config.yaml",
            r#"
addr: ":7777"
log_level: warn
database_url: postgres://file:host/db
"#,
        )?;
        // No env yet: file values win.
        let cfg = Config::load(Some("config.yaml")).expect("load");
        assert_eq!(cfg.addr, ":7777");
        assert_eq!(cfg.log_level, "warn");

        // Set env: it overrides the file.
        jail.set_env("KNOT_ADDR", ":8888");
        let cfg = Config::load(Some("config.yaml")).expect("load with env");
        assert_eq!(cfg.addr, ":8888");
        assert_eq!(cfg.log_level, "warn", "log_level still from file");
        Ok(())
    });
}

#[test]
fn refuses_empty_session_key_in_production() {
    figment::Jail::expect_with(|jail| {
        jail.set_env("KNOT_ENV", "production");
        let result = Config::load::<&str>(None);
        assert!(
            result.is_err(),
            "production with no session key must fail to load"
        );
        Ok(())
    });
}

#[test]
fn oidc_fields_have_defaults_and_env_overrides() {
    figment::Jail::expect_with(|jail| {
        let cfg = Config::load::<&str>(None).expect("load");
        assert!(!cfg.oidc_enabled);
        assert_eq!(cfg.oidc_auto_provision, "off");
        assert!(cfg.oidc_allowed_domains.is_empty());
        assert!(cfg.oidc_role_from_groups.is_empty());

        jail.set_env("KNOT_OIDC_ENABLED", "true");
        jail.set_env("KNOT_OIDC_ISSUER", "http://dex:5556/dex");
        jail.set_env("KNOT_OIDC_CLIENT_ID", "knot");
        jail.set_env("KNOT_OIDC_CLIENT_SECRET", "secret");
        jail.set_env(
            "KNOT_OIDC_REDIRECT_URL",
            "http://localhost:3000/auth/oidc/callback",
        );
        jail.set_env("KNOT_OIDC_AUTO_PROVISION", "domain");
        jail.set_env("KNOT_OIDC_ALLOWED_DOMAINS", "example.com,other.com");
        jail.set_env(
            "KNOT_OIDC_ROLE_FROM_GROUPS",
            r#"{"knot-admin":"owner","knot-edit":"editor"}"#,
        );

        let cfg = Config::load::<&str>(None).expect("load");
        assert!(cfg.oidc_enabled);
        assert_eq!(cfg.oidc_issuer, "http://dex:5556/dex");
        assert_eq!(cfg.oidc_client_id, "knot");
        assert_eq!(cfg.oidc_auto_provision, "domain");
        assert_eq!(cfg.oidc_allowed_domains, "example.com,other.com");
        assert_eq!(
            cfg.oidc_role_from_groups,
            r#"{"knot-admin":"owner","knot-edit":"editor"}"#
        );
        Ok(())
    });
}

#[test]
fn oidc_enabled_requires_issuer_client_redirect() {
    figment::Jail::expect_with(|jail| {
        jail.set_env("KNOT_OIDC_ENABLED", "true");
        // No issuer, client_id, redirect.
        let err = Config::load::<&str>(None).expect_err("must fail validation");
        assert!(err.to_string().contains("oidc_issuer"), "got: {err}");
        Ok(())
    });
}

#[test]
fn oidc_auto_provision_must_be_known_policy() {
    figment::Jail::expect_with(|jail| {
        jail.set_env("KNOT_OIDC_AUTO_PROVISION", "bogus");
        let err = Config::load::<&str>(None).expect_err("must fail");
        assert!(err.to_string().contains("auto_provision"), "got: {err}");
        Ok(())
    });
}

#[test]
fn oidc_client_id_accepts_numeric_value() {
    // Zitadel (and other IdPs) issue all-numeric client IDs. figment's Env
    // provider parses bare-digit values as integers, so the String field must
    // still accept them rather than failing with "invalid type: found unsigned
    // int ..., expected a string".
    figment::Jail::expect_with(|jail| {
        jail.set_env("KNOT_OIDC_ENABLED", "true");
        jail.set_env("KNOT_OIDC_ISSUER", "https://zitadel.example.com");
        jail.set_env("KNOT_OIDC_CLIENT_ID", "378364338165023482");
        jail.set_env("KNOT_OIDC_CLIENT_SECRET", "an-alphanumeric-secret");
        jail.set_env(
            "KNOT_OIDC_REDIRECT_URL",
            "https://knot.example.com/auth/oidc/callback",
        );
        let cfg = Config::load::<&str>(None).expect("numeric client_id must load");
        assert_eq!(cfg.oidc_client_id, "378364338165023482");
        Ok(())
    });
}

#[test]
fn oidc_client_secret_accepts_numeric_value() {
    // Same class of bug as the client id: an all-digit client secret arrives
    // from figment's Env provider as an integer and must still load as a String.
    figment::Jail::expect_with(|jail| {
        jail.set_env("KNOT_OIDC_ENABLED", "true");
        jail.set_env("KNOT_OIDC_ISSUER", "https://zitadel.example.com");
        jail.set_env("KNOT_OIDC_CLIENT_ID", "client-abc");
        jail.set_env("KNOT_OIDC_CLIENT_SECRET", "900219372854771713");
        jail.set_env(
            "KNOT_OIDC_REDIRECT_URL",
            "https://knot.example.com/auth/oidc/callback",
        );
        let cfg = Config::load::<&str>(None).expect("numeric client_secret must load");
        assert_eq!(cfg.oidc_client_secret, "900219372854771713");
        Ok(())
    });
}

#[test]
fn oidc_extra_audiences_default_empty_and_env_parsed() {
    // Zitadel issues ID tokens whose `aud` contains the client_id AND the
    // project id; operators list the extra (project) audiences here so the
    // verifier trusts them. Comma-separated, surrounding whitespace ignored,
    // empties dropped.
    figment::Jail::expect_with(|jail| {
        let cfg = Config::load::<&str>(None).expect("load");
        assert!(cfg.oidc_extra_audiences.is_empty());
        assert!(cfg.oidc_extra_audiences_list().is_empty());

        jail.set_env(
            "KNOT_OIDC_EXTRA_AUDIENCES",
            "366700366412350659, 378364338165023482 ,",
        );
        let cfg = Config::load::<&str>(None).expect("load");
        assert_eq!(
            cfg.oidc_extra_audiences_list(),
            vec![
                "366700366412350659".to_string(),
                "378364338165023482".to_string(),
            ]
        );
        Ok(())
    });
}

#[test]
fn oidc_extra_audiences_accepts_single_numeric_value() {
    // A single all-digit audience (e.g. a Zitadel project id) arrives from
    // figment's Env provider as an integer and must still load as a String,
    // exactly like the numeric client id/secret above.
    figment::Jail::expect_with(|jail| {
        jail.set_env("KNOT_OIDC_EXTRA_AUDIENCES", "366700366412350659");
        let cfg = Config::load::<&str>(None).expect("numeric extra audience must load");
        assert_eq!(cfg.oidc_extra_audiences, "366700366412350659");
        assert_eq!(
            cfg.oidc_extra_audiences_list(),
            vec!["366700366412350659".to_string()]
        );
        Ok(())
    });
}
