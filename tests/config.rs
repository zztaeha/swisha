// Config::from_env decides what guards the payout endpoint and what stops startup. Each guard and
// each refusal is pinned here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use swisha::config::{Config, InternalListener, Secret};
use swisha::env::{Env, EnvFile};
use swisha::error::Result;

// A folder of the shape SWISH_CERT_DIR must be: closed to other users and writable.
fn folder() -> String {
    let dir = std::env::temp_dir().join(format!("swisha-cfg-{}", swisha::swish::random_payout_uuid()));
    swisha::util::fs::create_private_dir(&dir).expect("certificate folder");
    dir.to_string_lossy().to_string()
}

// A certificate file to point DB_SSLROOTCERT at. Its contents are never read here.
fn root_certificate() -> String {
    let path = std::env::temp_dir().join(format!("swisha-root-{}.crt", swisha::swish::random_payout_uuid()));
    write_private(&path, "-----BEGIN CERTIFICATE-----\n-----END CERTIFICATE-----\n");
    path.to_string_lossy().to_string()
}

// Readable by the owner alone, as swisha demands of anything holding a key.
fn write_private(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

fn base() -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    for (k, v) in [
        ("DB_NAME", "swisha"),
        ("DB_USER", "swisha"),
        ("SWISH_CALLBACK_URL", "https://example.test/swish/callback"),
        ("SWISH_CERT_DIR", folder().as_str()),
    ] {
        m.insert(k, v.to_string());
    }
    m
}

// A real file on disk, because EnvFile reads one and pretending otherwise would test a
// different loader than the service uses.
fn load(settings: &BTreeMap<&'static str, String>) -> Result<Config> {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "swisha-config-{}.env",
        swisha::swish::random_payout_uuid()
    ));
    let body: String = settings
        .iter()
        .map(|(k, v)| format!("{k}={v}\n"))
        .collect();
    std::fs::write(&path, body).expect("write env file");
    let result = EnvFile::load(&path).and_then(|f| Config::from_env(&Env::file_only(f)));
    let _ = std::fs::remove_file(&path);
    result
}

fn with(pairs: &[(&'static str, &str)]) -> Result<Config> {
    let mut s = base();
    for (k, v) in pairs {
        s.insert(k, v.to_string());
    }
    load(&s)
}

#[test]
fn a_minimal_configuration_loads_with_every_default_applied() {
    let c = with(&[]).expect("minimal config should load");
    assert!(matches!(c.internal, InternalListener::Tcp(ref a) if a == "127.0.0.1:8083"));
    assert_eq!(c.callback_addr, "127.0.0.1:8084");
    assert_eq!(c.table_payouts, "swisha_payouts");
    assert_eq!(c.swish_env, "test");
    assert_eq!(c.swish_max_payout, 150_000.0, "Swish's own limit");
    assert_eq!(c.payouts_per_minute, 30);
    assert_eq!(c.db_pool_size, 10);
    assert!(c.api_secret.is_none());
}

// What guards the payout endpoint depends entirely on where it listens, and getting this wrong
// puts a money-moving endpoint on a network with nothing in front of it.
#[test]
fn a_unix_socket_needs_no_secret() {
    let c = with(&[("SWISH_SERVER_SOCKET", "/tmp/swisha-test.sock")])
        .expect("a socket is guarded by its file permissions");
    assert!(matches!(c.internal, InternalListener::Unix(_)));
    assert!(c.api_secret.is_none());
}

#[test]
fn a_socket_takes_precedence_over_an_address() {
    let c = with(&[
        ("SWISH_SERVER_SOCKET", "/tmp/swisha-test.sock"),
        ("SWISH_SERVER_ADDR", "0.0.0.0:8083"),
    ])
    .expect("socket wins");
    assert!(matches!(c.internal, InternalListener::Unix(_)));
}

#[test]
fn loopback_without_a_secret_is_allowed() {
    for addr in ["127.0.0.1:8083", "127.0.0.5:8083", "[::1]:8083"] {
        assert!(
            with(&[("SWISH_SERVER_ADDR", addr)]).is_ok(),
            "{addr} is loopback and should not require a secret"
        );
    }
}

// Only a literal address counts. A name resolves through /etc/hosts and DNS, so it can point
// anywhere, and treating one as trusted would hand that decision to whoever controls
// resolution. Demanding a secret is the wrong-way-safe answer.
#[test]
fn a_hostname_is_never_assumed_to_be_loopback() {
    assert!(
        with(&[("SWISH_SERVER_ADDR", "localhost:8083")]).is_err(),
        "localhost is a name, not an address"
    );
    assert!(with(&[
        ("SWISH_SERVER_ADDR", "localhost:8083"),
        ("API_SHARED_SECRET", "0123456789abcdef"),
    ])
    .is_ok(), "with a secret it is fine, wherever it resolves");
}

#[test]
fn a_network_address_without_a_secret_refuses_to_start() {
    for addr in ["0.0.0.0:8083", "192.168.1.10:8083", "10.0.0.5:8083"] {
        let err = with(&[("SWISH_SERVER_ADDR", addr)])
            .expect_err(&format!("{addr} is reachable from the network"))
            .to_string();
        assert!(
            err.contains("API_SHARED_SECRET"),
            "the refusal should name what is missing: {err}"
        );
    }
}

#[test]
fn a_network_address_with_a_secret_is_accepted() {
    let c = with(&[
        ("SWISH_SERVER_ADDR", "0.0.0.0:8083"),
        ("API_SHARED_SECRET", "0123456789abcdef"),
    ])
    .expect("a secret is what makes a network bind acceptable");
    assert_eq!(c.api_secret.as_ref().map(Secret::expose), Some("0123456789abcdef"));
}

// A short secret is worse than none: it reads as protection while being guessable.
#[test]
fn a_secret_shorter_than_sixteen_characters_refuses_to_start() {
    let err = with(&[("API_SHARED_SECRET", "0123456789abcde")])
        .expect_err("fifteen characters is too short")
        .to_string();
    assert!(err.contains("16"), "{err}");

    assert!(
        with(&[("API_SHARED_SECRET", "0123456789abcdef")]).is_ok(),
        "sixteen is the documented minimum and must be accepted"
    );
}

#[test]
fn a_blank_secret_is_absent_rather_than_empty() {
    let c = with(&[("API_SHARED_SECRET", "   ")]).expect("blank means unset");
    assert!(c.api_secret.is_none(), "an empty secret must never authorize anyone");
}

// The two listeners exist so a proxy misconfiguration cannot expose the payout endpoint. Sharing
// one address would undo that quietly.
#[test]
fn the_two_listeners_may_not_share_an_address() {
    let err = with(&[
        ("SWISH_SERVER_ADDR", "127.0.0.1:9000"),
        ("SWISH_CALLBACK_ADDR", "127.0.0.1:9000"),
    ])
    .expect_err("one address cannot serve both")
    .to_string();
    assert!(err.contains("must differ"), "{err}");
}

// One process serves every number from one set of tables, so the settings that let several
// processes share a database are gone. A file still setting one stops on it.
#[test]
fn the_table_and_channel_settings_are_refused_by_name() {
    for key in ["TABLE_PAYOUTS", "TABLE_LOGS", "TABLE_EVENTS", "NOTIFY_PREFIX"] {
        let error = with(&[(key, "custom")]).expect_err(key).to_string();
        assert!(error.contains(key) && error.contains("DB_NAME"), "{error}");
    }
    let c = with(&[("TABLE_PAYOUTS", "")]).expect("left blank, a removed setting is simply absent");
    assert_eq!(
        (c.table_payouts.as_str(), c.table_events.as_str(), c.table_logs.as_str()),
        ("swisha_payouts", "swisha_events", "swisha_logs")
    );
}

#[test]
fn the_required_settings_are_actually_required() {
    for key in ["DB_NAME", "DB_USER", "SWISH_CALLBACK_URL", "SWISH_CERT_DIR"] {
        let mut s = base();
        s.remove(key);
        let err = load(&s).expect_err(&format!("{key} is required")).to_string();
        assert!(err.contains(key), "the error should name {key}: {err}");
    }
}

#[test]
fn an_unknown_swish_env_is_refused_before_anything_else() {
    let err = with(&[("SWISH_ENV", "staging")])
        .expect_err("only test and production exist")
        .to_string();
    assert!(err.to_lowercase().contains("swish_env"), "{err}");
}

// Resolved once at startup, so a request never decides which Swish it is talking to.
#[test]
fn the_base_url_follows_the_environment() {
    assert_eq!(
        with(&[("SWISH_ENV", "test")]).expect("test").swish_base_url,
        "https://mss.cpc.getswish.net"
    );
    assert_eq!(
        with(&[("SWISH_ENV", "production")]).expect("production").swish_base_url,
        "https://cpc.getswish.net"
    );
}

#[test]
fn trusted_proxies_parse_into_a_list() {
    let c = with(&[("TRUSTED_PROXY", "127.0.0.1, 10.0.0.1,")]).expect("config");
    assert_eq!(c.trusted_proxies.len(), 2, "a trailing comma adds nothing");
}

// A dropped entry is a proxy whose forwarded callbacks are all refused with 403 in production, so
// a typo stops startup instead.
#[test]
fn a_trusted_proxy_that_is_not_an_address_refuses_to_start() {
    for (value, named) in [("127.0.0.l", "127.0.0.l"), ("127.0.0.1, 10.0.0.l", "10.0.0.l"), ("proxy.local", "proxy.local")] {
        let error = with(&[("TRUSTED_PROXY", value)]).expect_err(value).to_string();
        assert!(error.contains("TRUSTED_PROXY") && error.contains(named), "{error}");
    }
}

// A port typo must not quietly become 5432, and every spelling of an IPv6 address has to reach the
// address it names.
#[test]
fn db_host_takes_a_host_a_port_ipv6_or_a_socket() {
    use swisha::config::DbAddress::{Socket, Tcp};
    for (value, expected) in [
        ("localhost", Tcp("localhost".into(), 5432)),
        ("db.internal:6432", Tcp("db.internal".into(), 6432)),
        ("127.0.0.1:5433", Tcp("127.0.0.1".into(), 5433)),
        ("::1", Tcp("::1".into(), 5432)),
        ("[::1]", Tcp("::1".into(), 5432)),
        ("[::1]:5433", Tcp("::1".into(), 5433)),
        ("/var/run/postgresql", Socket("/var/run/postgresql".into())),
    ] {
        let c = with(&[("DB_HOST", value)]).unwrap_or_else(|e| panic!("{value}: {e}"));
        assert_eq!(c.db_address().expect(value), expected, "{value}");
    }
}

#[test]
fn a_db_host_that_is_not_a_host_and_port_refuses_to_start() {
    for wrong in ["localhost:5433x", "localhost:", "localhost:0", "localhost:70000", "[::1]:x", "[::1", "::1]:5433", "a:b:c"] {
        let error = with(&[("DB_HOST", wrong)]).expect_err(wrong).to_string();
        assert!(error.contains("DB_HOST") && error.contains(wrong), "{error}");
    }
}

// Debug output reaches logs and panic messages, so neither the database password nor the API
// secret may appear in it.
#[test]
fn debug_output_never_shows_a_secret() {
    let (password, secret) = ("db-password-0451", "api-secret-0123456789");
    let c = with(&[("DB_PASS", password), ("API_SHARED_SECRET", secret)]).expect("config");
    assert_eq!(c.db_pass.expose(), password);
    assert_eq!(c.api_secret.as_ref().map(Secret::expose), Some(secret));
    let shown = format!("{c:?}");
    assert!(!shown.contains(password) && !shown.contains(secret), "{shown}");

    let path = std::env::temp_dir().join(format!("swisha-debug-{}.env", swisha::swish::random_payout_uuid()));
    write_private(&path, &format!("DB_PASS={password}\nAPI_SHARED_SECRET={secret}\n"));
    let file = EnvFile::load(&path).expect("load");
    let _ = std::fs::remove_file(&path);
    let shown = format!("{:?}", Env::with_file(file));
    assert!(shown.contains("DB_PASS"), "the keys are still named: {shown}");
    assert!(!shown.contains(password) && !shown.contains(secret), "{shown}");
}

// A certificate file holds a private key, and whoever reads the signing one can sign payouts from
// that number. Readable by its owner alone: a group read would reach the application, which joins
// swisha's group to use the socket. The folder that holds them is held to it too.
#[test]
fn the_private_file_rule() {
    use swisha::util::fs::is_private;
    for (mode, expected) in [
        (0o100600, true),
        (0o100400, true),
        (0o040700, true),
        (0o100640, false),
        (0o100440, false),
        (0o040750, false),
        (0o100644, false),
        (0o100660, false),
        (0o100606, false),
        (0o100604, false),
    ] {
        assert_eq!(is_private(mode), expected, "mode {mode:o}");
    }
}

// A configuration written for the old layout must stop on the setting it relies on, not start
// and quietly run without it.
#[test]
fn the_old_certificate_settings_are_refused_by_name() {
    for (old, replacement) in [
        ("SWISH_CERT", "POST /swish/payers"),
        ("SWISH_KEY", "POST /swish/payers"),
        ("SWISH_SIGNING_CERT", "POST /swish/payers"),
        ("SWISH_SIGNING_KEY", "POST /swish/payers"),
        ("SWISH_CA", "Remove the setting"),
    ] {
        let error = with(&[(old, "/etc/swisha/anything.pem")]).expect_err(old).to_string();
        assert!(error.contains(old) && error.contains(replacement), "{error}");
    }
    assert!(with(&[("SWISH_CA", "")]).is_ok(), "left blank, an old setting is simply absent");
}

// The templates say which fields have to be filled in, and the README repeats them. Reading the
// markers back out of the shipped files is the only way that claim stays true.
fn keys_marked_required(template: &str) -> Vec<String> {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(template),
    )
    .unwrap_or_else(|e| panic!("{template}: {e}"))
    .lines()
    .filter_map(|line| {
        let (key, comment) = line.split_once('=')?;
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            return None;
        }
        comment.contains("# required").then(|| key.to_string())
    })
    .collect()
}

#[test]
fn every_field_the_templates_mark_required_really_is() {
    for template in ["examples/swisha.dev.example", "examples/swisha.prod.example"] {
        let marked = keys_marked_required(template);
        assert!(!marked.is_empty(), "{template}: nothing is marked required");

        for key in &marked {
            let mut settings = base();
            settings.remove(key.as_str());
            let error = load(&settings)
                .err()
                .unwrap_or_else(|| panic!("{template}: {key} is marked required but loads without it"))
                .to_string();
            assert!(
                error.contains(key.as_str()),
                "{template}: dropping {key} failed with a message that never names it: {error}"
            );
        }
    }
}

#[test]
fn nothing_the_templates_leave_blank_is_secretly_required() {
    for template in ["examples/swisha.dev.example", "examples/swisha.prod.example"] {
        let marked = keys_marked_required(template);
        let mut settings = base();

        // Everything the template does not mark is stripped back to its default, which is what a
        // reader who filled in only the marked lines actually ends up running.
        settings.retain(|key, _| marked.iter().any(|m| m == key));
        assert!(
            load(&settings).is_ok(),
            "{template}: filling in only the fields marked required does not start"
        );
    }
}

// The payout ceiling is a guard, so a value it cannot use stops the service rather than becoming
// the default without a word. NaN compares false against every amount.
#[test]
fn the_payout_ceiling_is_refused_unless_it_is_a_usable_amount() {
    for wrong in [
        "NaN", "inf", "-inf", "5 000", "50k", "0", "0.99", "-100", "150000.01", "150001", "100.005",
    ] {
        let error = with(&[("SWISH_MAX_PAYOUT", wrong)])
            .err()
            .unwrap_or_else(|| panic!("SWISH_MAX_PAYOUT='{wrong}' was accepted"))
            .to_string();
        assert!(
            error.contains("SWISH_MAX_PAYOUT") && error.contains(wrong),
            "the error must name the setting and the value: {error}"
        );
    }
    for (value, expected) in
        [("1", 1.0), ("2500", 2_500.0), ("1000.50", 1_000.5), ("150000", 150_000.0)]
    {
        let config = with(&[("SWISH_MAX_PAYOUT", value)])
            .unwrap_or_else(|e| panic!("SWISH_MAX_PAYOUT='{value}' was refused: {e}"));
        assert_eq!(config.swish_max_payout, expected);
    }
    assert_eq!(with(&[("SWISH_MAX_PAYOUT", "")]).expect("blank").swish_max_payout, 150_000.0);
}

// Each number's payout budget is a guard, so a value it cannot use stops the service rather than
// quietly becoming the default. A SaaS paying out in bulk raises it.
#[test]
fn the_payout_rate_is_refused_unless_it_is_a_whole_number_in_range() {
    for wrong in ["0", "-1", "2.5", "abc", "100001", "30 000", "NaN"] {
        let error = with(&[("SWISH_PAYOUTS_PER_MINUTE", wrong)])
            .err()
            .unwrap_or_else(|| panic!("SWISH_PAYOUTS_PER_MINUTE='{wrong}' was accepted"))
            .to_string();
        assert!(
            error.contains("SWISH_PAYOUTS_PER_MINUTE") && error.contains(wrong),
            "the error must name the setting and the value: {error}"
        );
    }
    for (value, expected) in [("1", 1), ("600", 600), ("100000", 100_000)] {
        let config = with(&[("SWISH_PAYOUTS_PER_MINUTE", value)])
            .unwrap_or_else(|e| panic!("SWISH_PAYOUTS_PER_MINUTE='{value}' was refused: {e}"));
        assert_eq!(config.payouts_per_minute, expected);
    }
    assert_eq!(with(&[("SWISH_PAYOUTS_PER_MINUTE", "")]).expect("blank").payouts_per_minute, 30);
}

// The pool caps how many queries run at once: 10 carried about 400 payouts a second. A value it
// cannot use stops the service rather than quietly becoming the default.
#[test]
fn the_pool_size_is_refused_unless_it_is_a_whole_number_in_range() {
    for wrong in ["0", "-1", "1.5", "many", "1001"] {
        let error = with(&[("DB_POOL_SIZE", wrong)])
            .err()
            .unwrap_or_else(|| panic!("DB_POOL_SIZE='{wrong}' was accepted"))
            .to_string();
        assert!(error.contains("DB_POOL_SIZE") && error.contains(wrong), "{error}");
    }
    for (value, expected) in [("1", 1), ("32", 32), ("1000", 1000)] {
        let config = with(&[("DB_POOL_SIZE", value)])
            .unwrap_or_else(|e| panic!("DB_POOL_SIZE='{value}' was refused: {e}"));
        assert_eq!(config.db_pool_size, expected);
    }
    assert_eq!(with(&[("DB_POOL_SIZE", "")]).expect("blank").db_pool_size, 10);
}

// Swish refuses every payout whose callback is not HTTPS with RP03, so a wrong URL here would
// turn every payout into a refusal. Max 684 characters, per Swish's reference.
#[test]
fn the_callback_url_must_be_one_swish_accepts() {
    let long = format!("https://example.test/{}", "a".repeat(664));
    for wrong in [
        "http://example.test/swish/callback",
        "ftp://example.test/swish/callback",
        "example.test/swish/callback",
        "https://",
        "https://example.test/swish callback",
        long.as_str(),
    ] {
        let error = with(&[("SWISH_CALLBACK_URL", wrong)])
            .err()
            .unwrap_or_else(|| panic!("SWISH_CALLBACK_URL='{wrong}' was accepted"))
            .to_string();
        assert!(error.contains("SWISH_CALLBACK_URL"), "the error must name the setting: {error}");
    }
    let longest = format!("https://example.test/{}", "a".repeat(663));
    for right in [
        "https://example.test/swish/callback",
        "HTTPS://example.test/cb",
        longest.as_str(),
    ] {
        assert!(with(&[("SWISH_CALLBACK_URL", right)]).is_ok(), "{right} should load");
    }
}

// Swish requires the personnummer on every payout, so the switch that made it optional is gone,
// and a configuration still setting it is told so instead of being silently ignored.
#[test]
fn require_ssn_is_refused_now_that_it_is_always_on() {
    for value in ["true", "false"] {
        let error = with(&[("SWISH_REQUIRE_SSN", value)]).expect_err(value).to_string();
        assert!(error.contains("SWISH_REQUIRE_SSN") && error.contains("always required"), "{error}");
    }
}

// The template is used for every payout that leaves message out, so a character Swish refuses
// there would spend every such reference. {reference} itself is always safe.
#[test]
fn a_message_template_swish_would_refuse_stops_startup() {
    for bad in ["\"Faktura #{reference}\"", "Faktura/{reference}", "Café {reference}", "{reference} 😀"] {
        let error = with(&[("SWISH_PAYOUT_MESSAGE", bad)]).expect_err(bad).to_string();
        assert!(error.contains("SWISH_PAYOUT_MESSAGE") && error.contains("!?(),.-:;"), "{error}");
    }
    for good in ["Utbetalning {reference}", "Faktura: {reference}, tack!", "{reference}"] {
        let c = with(&[("SWISH_PAYOUT_MESSAGE", good)]).unwrap_or_else(|e| panic!("{good}: {e}"));
        assert_eq!(c.payout_message, good);
    }
}

// A misspelled key in the env file would otherwise be ignored, and its setting would quietly take
// its default: a lower SWISH_MAX_PAYOUT would stay at 150,000.
#[test]
fn a_key_in_the_env_file_swisha_does_not_read_stops_startup() {
    for (key, value) in [
        ("SWISH_MAX_PAYOT", "1000"),
        ("TRUSTED_PROXIES", "10.0.0.5"),
        ("SWISH_NUMBER2", "1239876543"),
        ("DATABASE_URL", "postgres://elsewhere"),
    ] {
        let error = with(&[(key, value)]).expect_err(key).to_string();
        assert!(error.contains(key) && error.contains("not a setting swisha reads"), "{error}");
    }
    // Left blank it is simply absent, as removed settings are, so an older template still loads.
    assert!(with(&[("SWISH_MAX_PAYOT", "")]).is_ok());
}

// sqlx alone prefers TLS and quietly falls back to plaintext, which for a database on another
// host carries personnummer, amounts and credentials in clear. Across the network TLS is
// required unless DB_SSLMODE says otherwise; on loopback or a socket it adds nothing.
#[test]
fn the_database_connection_requires_tls_off_this_host() {
    use swisha::config::DbTls;
    for (host, expected) in [
        ("db.internal:5432", DbTls::Require),
        ("10.0.0.5", DbTls::Require),
        ("localhost", DbTls::Prefer),
        ("127.0.0.1:5433", DbTls::Prefer),
        ("[::1]:5432", DbTls::Prefer),
        ("/var/run/postgresql", DbTls::Prefer),
    ] {
        let c = with(&[("DB_HOST", host)]).unwrap_or_else(|e| panic!("{host}: {e}"));
        assert_eq!(c.db_tls, expected, "{host}");
    }
    for (value, expected) in [
        ("disable", DbTls::Disable),
        ("prefer", DbTls::Prefer),
        ("require", DbTls::Require),
        ("verify-ca", DbTls::VerifyCa),
        ("verify-full", DbTls::VerifyFull),
    ] {
        let c = with(&[("DB_HOST", "db.internal"), ("DB_SSLMODE", value)]).expect(value);
        assert_eq!(c.db_tls, expected, "{value}");
    }
    let error = with(&[("DB_SSLMODE", "on")]).expect_err("not a mode").to_string();
    assert!(error.contains("DB_SSLMODE") && error.contains("verify-full"), "{error}");

    let root = root_certificate();
    let c = with(&[("DB_SSLMODE", "verify-full"), ("DB_SSLROOTCERT", root.as_str())]).expect("a root file");
    assert_eq!(c.db_ssl_root_cert.as_deref(), Some(std::path::Path::new(&root)));
    let error = with(&[("DB_SSLROOTCERT", "/nonexistent/root.crt")]).expect_err("missing").to_string();
    assert!(error.contains("DB_SSLROOTCERT"), "{error}");
}

// Every number's certificates live in this folder, so it is held to the rule for a key file:
// closed to other users. swisha writes into it, so it has to be writable too.
#[test]
fn the_certificate_folder_must_exist_be_closed_and_be_writable() {
    use std::os::unix::fs::PermissionsExt;
    let open = folder();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let error = with(&[("SWISH_CERT_DIR", open.as_str())]).expect_err("open folder").to_string();
    assert!(error.contains(&open) && error.contains("chmod 700"), "{error}");

    let missing = format!("{open}/nothing-here");
    let error = with(&[("SWISH_CERT_DIR", missing.as_str())]).expect_err("missing folder").to_string();
    assert!(error.contains("SWISH_CERT_DIR") && error.contains(&missing), "{error}");

    let file = root_certificate();
    let error = with(&[("SWISH_CERT_DIR", file.as_str())]).expect_err("a file").to_string();
    assert!(error.contains("not a directory"), "{error}");

    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    let error = with(&[("SWISH_CERT_DIR", open.as_str())]).expect_err("read-only folder").to_string();
    assert!(error.contains("cannot write"), "{error}");

    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let c = with(&[("SWISH_CERT_DIR", open.as_str())]).expect("a private, writable folder");
    assert_eq!(c.cert_dir, std::path::PathBuf::from(&open));
    let leftovers = std::fs::read_dir(&open).expect("read").count();
    assert_eq!(leftovers, 0, "the write check leaves nothing behind");
}

// Numbers are stored in the database from 0.4. A file still carrying a payer block stops on it and
// says where numbers go now, rather than starting with numbers it silently ignores.
#[test]
fn the_payer_blocks_from_before_0_4_are_refused_by_name() {
    for (key, value) in [
        ("SWISH_NUMBER", "1234679304"),
        ("SWISH_NAME", "sthlm"),
        ("SWISH_TLS_2", "/etc/swisha/certs/gbg-tls.pem"),
        ("SWISH_SIGNING_15", "/etc/swisha/certs/kiruna-signing.pem"),
    ] {
        let error = with(&[(key, value)]).expect_err(key).to_string();
        assert!(
            error.contains(key) && error.contains("POST /swish/payers") && error.contains("Upgrading from 0.3"),
            "{error}"
        );
    }
    let blank = [("SWISH_NUMBER", ""), ("SWISH_NAME", ""), ("SWISH_TLS_7", ""), ("SWISH_SIGNING_7", "")];
    assert!(with(&blank).is_ok(), "a 0.3 template left blank still loads");
}
