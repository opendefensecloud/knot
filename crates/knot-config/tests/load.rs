//! Integration tests for the layered config loader.

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
