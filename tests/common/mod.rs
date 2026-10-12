// Shared test harness. Each test binary compiles the whole module and uses a slice of it, so
// unused items here are expected rather than dead.
#![allow(dead_code)]

use sqlx::AssertSqlSafe;

use std::sync::{Arc, OnceLock};

use ring::signature::RsaKeyPair;
use swisha::config::{Config, InternalListener};
use swisha::domain::errors::Language;
use swisha::payers::{CertificateFile, Payer};
use swisha::state::{AppState, Notifications, PayerState, Payers, SharedState, Store};

pub const SECRET: &str = "0123456789abcdef0123456789abcdef";
// The payer every single-payer test pays from. Swish's test merchant number, so a payload built
// here would be addressed exactly as one sent to MSS.
pub const PAYER: &str = "test";
pub const PAYER_NUMBER: &str = "1234679304";
pub const SSN: &str = "196408233234";
pub const PHONE: &str = "0701234567";
pub const KEY_HEADER: &str = "x-payer-key";

// A test number's key: the same for a name in every test, and shaped as swisha makes one.
pub fn key(name: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, format!("test key {name}").as_bytes());
    format!("swk_{}", swisha::util::hex::upper(digest.as_ref()))
}

// Generated per run and never written into the repository: a private key committed to an
// open-source payments project is exactly what secret scanning exists to stop. 2048 bits keeps
// the suite fast; Swish issues 4096, which costs 6.5 times as much to sign, so measure with that.
pub fn signing_key() -> Arc<RsaKeyPair> {
    static KEY: OnceLock<Arc<RsaKeyPair>> = OnceLock::new();
    KEY.get_or_init(|| {
        let out = std::process::Command::new("openssl")
            .args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048"])
            .output()
            .expect("openssl is required to generate a throwaway signing key");
        assert!(out.status.success(), "openssl genpkey failed");
        let pem = String::from_utf8(out.stdout).expect("PEM is UTF-8");
        let der = swisha::swish::sign::private_key_der(&pem).expect("decode the generated PEM");
        Arc::new(RsaKeyPair::from_pkcs8(&der).expect("parse the generated RSA key"))
    })
    .clone()
}

// Where the test database lives. Unset is a hard stop rather than a skip: a payments suite
// reporting "0 failed" while its double-payout guards never ran is the worst outcome available.
pub fn database_url() -> String {
    std::env::var("SWISHA_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "\n\nSWISHA_TEST_DATABASE_URL is not set, and every test needs a database.\n\n  \
             docker run -d --name swisha-pg -p 5433:5432 \\\n    \
             -e POSTGRES_USER=swisha -e POSTGRES_PASSWORD=swisha \\\n    \
             -e POSTGRES_DB=swisha_test postgres:18\n\n  \
             export SWISHA_TEST_DATABASE_URL=postgres://swisha:swisha@localhost:5433/swisha_test\n"
        )
    })
}

// One prefix per call, so tests sharing a server cannot see each other's rows.
pub fn table_prefix() -> String {
    format!("t{}", swisha::swish::random_payout_uuid()[..12].to_lowercase())
}

// A folder of the shape SWISH_CERT_DIR must be: closed to other users. Each call gets its own, so
// tests that write certificates never meet.
pub fn cert_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("swisha-certs-{}", swisha::swish::random_payout_uuid()));
    swisha::util::fs::create_private_dir(&dir).expect("certificate folder");
    dir
}

// A certificate followed by its own key, as swisha keeps each file, with `cn` as its common name.
// Generated once per name and lifetime, under the lock, so parallel tests share one copy.
pub fn bundle(cn: &str, days: &str) -> String {
    static MADE: OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();
    let mut made = MADE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new())).lock().unwrap_or_else(|e| e.into_inner());
    let key = format!("{cn}/{days}");
    if let Some(pem) = made.get(&key) {
        return pem.clone();
    }
    let dir = std::env::temp_dir().join(format!("swisha-pair-{}", swisha::swish::random_payout_uuid()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let dates: Vec<&str> = match days {
        "expired" => vec!["-not_before", "20200101000000Z", "-not_after", "20200102000000Z"],
        days => vec!["-days", days],
    };
    let out = std::process::Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-subj", &format!("/CN={cn}")])
        .args(&dates)
        .arg("-keyout").arg(dir.join("k"))
        .arg("-out").arg(dir.join("c"))
        .output()
        .expect("openssl is required to generate a throwaway pair");
    assert!(out.status.success(), "openssl req failed");
    let pem = format!(
        "{}{}",
        std::fs::read_to_string(dir.join("c")).expect("certificate"),
        std::fs::read_to_string(dir.join("k")).expect("key")
    );
    let _ = std::fs::remove_dir_all(&dir);
    made.insert(key, pem.clone());
    pem
}

// The handlers never read the PEM: tests sign with signing_key() and talk to MockSwish over plain
// HTTP. Only the serial reaches a payload. It expires on the last day X.509 can name.
pub fn certificate(serial: &str) -> CertificateFile {
    let expires = swisha::util::time::unix_from_utc(9999, 12, 31, 23, 59, 59);
    CertificateFile { pem: String::new(), serial: serial.into(), expires }
}

pub fn config() -> Config {
    // Tests build their own reqwest clients, and rustls-no-provider panics on the first one
    // unless a provider is installed. Every test path reaches config() before that happens.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let prefix = table_prefix();
    Config {
        internal:             InternalListener::Tcp("127.0.0.1:0".into()),
        callback_addr:        "127.0.0.1:0".into(),
        db_host:              "127.0.0.1".into(),
        db_name:              "swisha_test".into(),
        db_user:              "swisha".into(),
        db_pass:              Default::default(),
        db_tls:               swisha::config::DbTls::Prefer,
        db_ssl_root_cert:     None,
        db_pool_size:         10,
        table_payouts:        format!("{prefix}_payouts"),
        table_logs:           format!("{prefix}_logs"),
        table_events:         format!("{prefix}_events"),
        table_payers:         format!("{prefix}_payers"),
        trusted_proxies:      Vec::new(),
        api_secret:           Some(SECRET.into()),
        swish_env:            "test".into(),
        swish_base_url:       "http://127.0.0.1:1".into(),
        swish_max_payout:     50_000.0,
        // The limiter is one map for the process, and every test calls from loopback. A test of the
        // limit sets its own budget and pays from a number of its own.
        payouts_per_minute:   100_000,
        swish_callback_url:   "https://example.test/swish/callback".into(),
        // Nothing listens here: a test that wants a status page starts one and points this at it.
        swish_status_url:     "http://127.0.0.1:1/api/v2/summary.json".into(),
        cert_dir:             cert_dir(),
        payout_message:       "{reference}".into(),
        error_language:       Language::English,
    }
}

// What Swish echoes as the callbackIdentifier header: the value stored with the payout.
pub async fn callback_identifier(state: &SharedState, payer: &str, reference: &str) -> String {
    use swisha::store::PayoutStore;
    state
        .store
        .snapshot(payer, reference)
        .await
        .ok()
        .flatten()
        .and_then(|snapshot| snapshot.callback_identifier)
        .unwrap_or_default()
}

pub fn payer(name: &str, number: &str) -> Payer {
    Payer {
        name: name.into(),
        nickname: name.into(),
        number: number.into(),
        tls: certificate("00"),
        signing: certificate("00"),
    }
}

// Ready to pay, with the shared throwaway signing key and a plain client: MockSwish speaks HTTP,
// so there is no certificate to present.
pub fn payer_state(payer: &Payer) -> PayerState {
    PayerState {
        name: payer.name.clone(),
        nickname: payer.nickname.clone(),
        number: payer.number.clone(),
        client: reqwest::Client::new(),
        signing_key: signing_key(),
        signing_serial: payer.signing.serial.clone(),
        tls_expires: payer.tls.expires,
        signing_expires: payer.signing.expires,
        probe: tokio::sync::RwLock::new(None),
    }
}

// A pool over the shipped schema, applied under this config's table prefix.
pub async fn pool_with_schema(config: &Config) -> sqlx::PgPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&database_url())
        .await
        .unwrap_or_else(|e| panic!("connect to {}: {e}", database_url()));

    sweep_old_tables(&pool).await;
    apply_schema(&pool, config).await;
    pool
}

// Runs schema/postgres.sql under this config's table prefix, as an upgrade runs it again.
pub async fn apply_schema(pool: &sqlx::PgPool, config: &Config) {
    for statement in schema_statements(config) {
        sqlx::query(AssertSqlSafe(statement.clone()))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{statement}"));
    }
}

// schema/postgres.sql's statements under this config's table prefix. `swisha_` is the only name in
// it, so substituting it is the whole of the isolation.
pub fn schema_statements(config: &Config) -> Vec<String> {
    let prefix = config
        .table_payouts
        .strip_suffix("_payouts")
        .expect("config() builds table names from a prefix");

    let ddl = include_str!("../../schema/postgres.sql")
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("swisha_", &format!("{prefix}_"));
    statements(&ddl)
}

// The schema's statements, split at each `;` outside a `$$` body, as psql splits them.
fn statements(ddl: &str) -> Vec<String> {
    let mut statements = vec![String::new()];
    for (i, part) in ddl.split("$$").enumerate() {
        let current = statements.last_mut().expect("never empty");
        if i % 2 == 1 {
            current.push_str(&format!("$${part}$$"));
            continue;
        }
        let mut pieces = part.split(';');
        current.push_str(pieces.next().unwrap_or_default());
        statements.extend(pieces.map(str::to_string));
    }
    statements.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

// Tests do not drop their own tables, because a panicking test would leave them behind anyway.
// Instead the first pool of each run clears what earlier runs left, which is self-healing and
// needs no guard threaded through 300 call sites.
async fn sweep_old_tables(pool: &sqlx::PgPool) {
    static SWEPT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    SWEPT
        .get_or_init(|| async {
            let names: Vec<String> = sqlx::query_scalar(
                "SELECT tablename FROM pg_tables WHERE schemaname = current_schema() \
                 AND tablename ~ '^t[0-9a-f]{12}_(payouts|events|logs|payers)$'",
            )
            .fetch_all(pool)
            .await
            .unwrap_or_default();

            for name in names {
                let _ = sqlx::query(AssertSqlSafe(format!("DROP TABLE IF EXISTS {name} CASCADE")))
                    .execute(pool)
                    .await;
            }
        })
        .await;
}

// The one number every single-payer test pays from.
pub fn payers() -> Vec<Payer> {
    vec![payer(PAYER, PAYER_NUMBER)]
}

// Hands the pool back alongside the state, for a test that queries the tables or closes the pool.
pub async fn state_and_pool(config: Config) -> (SharedState, sqlx::PgPool) {
    state_and_pool_with(config, payers()).await
}

pub async fn state_and_pool_with(config: Config, payers: Vec<Payer>) -> (SharedState, sqlx::PgPool) {
    let pool = pool_with_schema(&config).await;
    let state = state_over(pool.clone(), config, payers.iter().map(payer_state).collect()).await;
    (state, pool)
}

// State over a pool, with numbers loaded as given rather than from their files. Each is stored
// too, with key(name), since every request about a number is checked against the store.
pub async fn state_over(pool: sqlx::PgPool, config: Config, ready: Vec<PayerState>) -> SharedState {
    state_over_with(pool, config, ready, swisha::events::EventStream::default()).await
}

// The same, streaming through `stream`, for a test of the stream's own limits.
pub async fn state_with_stream(config: Config, payers: Vec<Payer>, stream: swisha::events::EventStream) -> SharedState {
    let pool = pool_with_schema(&config).await;
    state_over_with(pool, config, payers.iter().map(payer_state).collect(), stream).await
}

async fn state_over_with(
    pool: sqlx::PgPool,
    config: Config,
    ready: Vec<PayerState>,
    stream: swisha::events::EventStream,
) -> SharedState {
    use swisha::domain::payer::{AddPayerOutcome, NewPayer};
    use swisha::store::PayoutStore;
    let store = Store::new(
        pool.clone(),
        &config.table_payouts,
        &config.table_events,
        &config.table_logs,
        &config.table_payers,
    );
    for payer in &ready {
        let hash = swisha::payers::key_hash(&key(&payer.name)).expect("a test key is shaped as one");
        let row = NewPayer {
            name: &payer.name,
            nickname: &payer.nickname,
            number: &payer.number,
            added_by: "test",
            key_hash: &hash,
        };
        let outcome = store.add_payer(&row, &logged()).await.expect("store a test number");
        assert!(matches!(outcome, AddPayerOutcome::Added | AddPayerOutcome::NameTaken), "{}: {outcome:?}", payer.name);
    }
    let mut state = AppState::new(store, Notifications::new(pool), config, Payers::new(ready));
    state.stream = stream;
    Arc::new(state)
}

pub async fn state_with(config: Config) -> SharedState {
    state_with_payers(config, payers()).await
}

pub async fn state_with_payers(config: Config, payers: Vec<Payer>) -> SharedState {
    state_and_pool_with(config, payers).await.0
}

pub async fn state() -> SharedState {
    state_with(config()).await
}

// The line a change to a number is written with, for a test that changes one in the store.
pub fn logged() -> swisha::domain::payer::LogLine<'static> {
    swisha::domain::payer::LogLine { level: "info", message: "test", context: None, ip: None }
}

// The instruction as Swish received it. It is sent as a RawValue so the signed bytes stay
// byte-identical, which means it arrives nested rather than as a string.
pub fn payload(sent: &serde_json::Value) -> serde_json::Value {
    assert!(sent["payload"].is_object(), "the instruction goes out as an object: {sent}");
    sent["payload"].clone()
}

// Has the database refuse every write of a row matching `rows` to one of this test's tables, as a
// full disk or a lock timeout would, until allow() lifts it.
pub async fn refuse(state: &SharedState, table: &str, rows: &str) {
    alter(state, &format!("ALTER TABLE {table} ADD CONSTRAINT refused CHECK (NOT ({rows})) NOT VALID")).await;
}

pub async fn allow(state: &SharedState, table: &str) {
    alter(state, &format!("ALTER TABLE {table} DROP CONSTRAINT refused")).await;
}

// Moves a payout to `status` under the store's status rules, with nothing on its trail, as Swish or
// the checks would have left it. Reports whether the status applied.
pub async fn stage(state: &SharedState, payer: &str, reference: &str, status: &str) -> bool {
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET status = $1, updated_at = NOW() WHERE payer = $2 AND reference = $3 AND {}",
        state.config.table_payouts,
        swisha::domain::status::writable_condition("$1")
    )))
    .bind(status)
    .bind(payer)
    .bind(reference)
    .execute(state.store.pool())
    .await
    .expect("stage")
    .rows_affected()
        > 0
}

async fn alter(state: &SharedState, statement: &str) {
    sqlx::query(AssertSqlSafe(statement.to_owned()))
        .execute(state.store.pool())
        .await
        .unwrap_or_else(|e| panic!("{statement}: {e}"));
}

// Serves a router through axum::serve, with the connect info the callback handler needs. Not the
// binary's accept loop: no header timeout and no connection cap, which limits.rs tests.
#[cfg(feature = "http")]
pub async fn serve(router: axum::Router) -> String {
    use std::net::SocketAddr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .ok();
    });
    format!("http://{addr}")
}

// A stand-in for Swish's Payouts API, so the submit and poll paths can be driven through every
// answer Swish documents without touching MSS. Scripted, and it records what it was sent.
#[cfg(feature = "http")]
#[derive(Clone, Default)]
pub struct MockSwish {
    inner: Arc<std::sync::Mutex<MockInner>>,
}

#[cfg(feature = "http")]
#[derive(Default)]
struct MockInner {
    post_status: Option<u16>,
    post_body: String,
    // Successive GETs walk this list and then repeat its last entry. Empty answers 400 naming
    // PA01, what Swish documents and its simulator answers for a UUID it has no record of.
    get_answers: Vec<GetAnswer>,
    // What a GET on one instruction answers instead, whatever the list says.
    by_uuid: std::collections::HashMap<String, GetAnswer>,
    posts: Vec<serde_json::Value>,
    gets: usize,
    // The raw body of every POST, so a test can verify the signature over the exact bytes, and
    // the x-test-payer header of every call, so it can tell whose client made it.
    raw_posts: Vec<String>,
    posts_by: Vec<Option<String>>,
    gets_by: Vec<(String, Option<String>)>,
    // How long the POST takes to answer, so a request can be caught in flight.
    post_delay: std::time::Duration,
}

#[cfg(feature = "http")]
#[derive(Clone)]
enum GetAnswer {
    Body(serde_json::Value),
    Code(u16),
    NoRecord,
}

#[cfg(feature = "http")]
impl MockSwish {
    pub fn new() -> Self {
        Self::default()
    }

    // Swish answers 201 with a Location header when it accepts an instruction.
    pub fn accepts(self) -> Self {
        self.inner.lock().unwrap().post_status = Some(201);
        self
    }

    pub fn answers_after(self, delay: std::time::Duration) -> Self {
        self.inner.lock().unwrap().post_delay = delay;
        self
    }

    pub fn rejects(self, status: u16, body: &str) -> Self {
        let mut i = self.inner.lock().unwrap();
        i.post_status = Some(status);
        i.post_body = body.to_string();
        drop(i);
        self
    }

    // What a GET on the instruction returns, from now on.
    pub fn resolves_to(self, status: &str) -> Self {
        self.resolves_with(serde_json::json!({ "status": status }))
    }

    pub fn resolves_with(self, body: serde_json::Value) -> Self {
        self.inner.lock().unwrap().get_answers = vec![GetAnswer::Body(body)];
        self
    }

    // Successive GETs answer these statuses in turn, then keep answering the last.
    pub fn resolves_through(self, statuses: &[&str]) -> Self {
        self.inner.lock().unwrap().get_answers = statuses
            .iter()
            .map(|status| GetAnswer::Body(serde_json::json!({ "status": status })))
            .collect();
        self
    }

    // Every GET answers this HTTP status with no body, such as 429.
    pub fn answers_gets_with(self, code: u16) -> Self {
        self.inner.lock().unwrap().get_answers = vec![GetAnswer::Code(code)];
        self
    }

    // What a GET on one instruction answers from now on, while the others answer as before.
    pub fn resolves_for(self, uuid: &str, status: &str) -> Self {
        let answer = GetAnswer::Body(serde_json::json!({ "status": status }));
        self.inner.lock().unwrap().by_uuid.insert(uuid.to_string(), answer);
        self
    }

    pub fn answers_for(self, uuid: &str, code: u16) -> Self {
        self.inner.lock().unwrap().by_uuid.insert(uuid.to_string(), GetAnswer::Code(code));
        self
    }

    // From now on Swish has no record of this instruction, as it answers for one never received.
    pub fn forgets(self, uuid: &str) -> Self {
        self.inner.lock().unwrap().by_uuid.insert(uuid.to_string(), GetAnswer::NoRecord);
        self
    }

    pub fn posted(&self) -> Vec<serde_json::Value> {
        self.inner.lock().unwrap().posts.clone()
    }

    pub fn get_count(&self) -> usize {
        self.inner.lock().unwrap().gets
    }

    pub fn raw_posts(&self) -> Vec<String> {
        self.inner.lock().unwrap().raw_posts.clone()
    }

    pub fn posts_by(&self) -> Vec<Option<String>> {
        self.inner.lock().unwrap().posts_by.clone()
    }

    pub fn gets_by(&self) -> Vec<(String, Option<String>)> {
        self.inner.lock().unwrap().gets_by.clone()
    }

    pub async fn start(self) -> String {
        use axum::{extract::{Path, State}, routing::{get, post}, Json, Router};
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;

        fn caller(headers: &HeaderMap) -> Option<String> {
            headers.get("x-test-payer").and_then(|v| v.to_str().ok()).map(str::to_string)
        }

        async fn create(State(m): State<MockSwish>, headers: HeaderMap, raw: String) -> impl IntoResponse {
            let delay = m.inner.lock().unwrap().post_delay;
            tokio::time::sleep(delay).await;
            let body: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
            let mut i = m.inner.lock().unwrap();
            i.posts.push(body);
            i.raw_posts.push(raw);
            i.posts_by.push(caller(&headers));
            match i.post_status {
                Some(201) | None => (StatusCode::CREATED, String::new()),
                Some(code) => (
                    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    i.post_body.clone(),
                ),
            }
        }

        async fn read(
            State(m): State<MockSwish>,
            Path(uuid): Path<String>,
            headers: HeaderMap,
        ) -> impl IntoResponse {
            let mut i = m.inner.lock().unwrap();
            let listed = i.get_answers.get(i.gets).or(i.get_answers.last());
            let answer = i.by_uuid.get(&uuid).or(listed).cloned();
            i.gets += 1;
            i.gets_by.push((uuid, caller(&headers)));
            match answer {
                Some(GetAnswer::Body(body)) => (StatusCode::OK, Json(body)).into_response(),
                Some(GetAnswer::Code(code)) => {
                    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR).into_response()
                }
                Some(GetAnswer::NoRecord) | None => {
                    let unknown = serde_json::json!([{ "errorCode": "PA01", "errorMessage": "Parameter is not correct." }]);
                    (StatusCode::BAD_REQUEST, Json(unknown)).into_response()
                }
            }
        }

        let router = Router::new()
            .route("/swish-cpcapi/api/v1/payouts", post(create))
            .route("/swish-cpcapi/api/v1/payouts/{uuid}", get(read))
            .with_state(self);
        serve(router).await
    }
}
