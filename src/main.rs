#![forbid(unsafe_code)]
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::Level;
use swisha::{backend, config, events, http, payers, state, swish};

pub const SCHEMA: &str = include_str!("../schema/postgres.sql");

const USAGE: &str = "\
usage: swisha                   serve, configured by SWISHA_ENV_FILE or the environment
       swisha --print-schema    print the PostgreSQL tables swisha uses
       swisha --new-key NAME    give a number a new key and print it; the old one stops working
       swisha --disable NAME    stop a number taking new payouts
       swisha --enable NAME     let a disabled number take payouts again
       swisha --list-payers     list every number, whether it can pay, and its certificates";

// What the binary was asked to do. Everything but serving is an operator's command on the server,
// where the env file, and so the database, already means full control over every number.
enum Command {
    Serve,
    PrintSchema,
    NewKey(String),
    Disable(String),
    Enable(String),
    ListPayers,
}

// How long a stop waits for requests in flight. A submit to Swish times out after 55s, and
// systemd sends SIGKILL after 90s by default, so this fits between the two.
const SHUTDOWN_DRAIN: std::time::Duration = std::time::Duration::from_secs(60);

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = command(&args) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    // Answered before anything else so it works without a database or certificates.
    if matches!(command, Command::PrintSchema) {
        print!("{SCHEMA}");
        return;
    }

    // Read the configuration file, if one was named, before anything reads a setting.
    let env = swisha::env::Env::discover().unwrap_or_else(|e| {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    });

    // A command prints its result alone on stdout, so its log lines go to stderr.
    let logging = tracing_subscriber::fmt().with_max_level(log_level(&env));
    if matches!(command, Command::Serve) {
        logging.init();
    } else {
        logging.with_writer(std::io::stderr).init();
    }
    if let Some(path) = env.source() {
        tracing::info!("configuration loaded from {}", path.display());
    }

    let config = config::Config::from_env(&env).unwrap_or_else(|e| {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    });

    let (store, notifier) = backend::connect(&config).await.unwrap_or_else(|e| {
        eprintln!("Storage error: {e}");
        std::process::exit(1);
    });
    backend::verify_store(&store).await.unwrap_or_else(|e| {
        eprintln!("Storage error: {e}");
        std::process::exit(1);
    });

    let state: state::SharedState =
        Arc::new(state::AppState::new(store, notifier, config.clone(), state::Payers::default()));
    if !matches!(command, Command::Serve) {
        std::process::exit(run(&state, command).await);
    }

    // Every stored number, before the first request. One whose certificates fail is held aside
    // with the reason and refuses its payouts; the others pay.
    payers::refresh(&state).await.unwrap_or_else(|e| {
        eprintln!("Storage error: {e}");
        std::process::exit(1);
    });
    let roster = state.payers.snapshot();
    let ready = roster.entries().iter().filter(|entry| entry.loaded.is_ok()).count();
    tracing::info!("Swish numbers stored: {}, ready to pay: {ready}", roster.entries().len());

    // The numbers stay current as they change, and their reach to Swish is checked, one a second.
    tokio::spawn(payers::watch(state.clone()));

    // Swish's own status page, every 5 minutes. Read so health can say whose problem an outage
    // is; never consulted before a payout, so a wrong page cannot stop one.
    {
        let s = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                swish::status_page::refresh(&s).await;
            }
        });
    }

    // Ask Swish about every payout it has not given its last word on, each number on its own.
    tokio::spawn(swish::reconcile::run(state.clone(), swish::reconcile::CHECKS));

    events::log_admin_event("info", "service_started", serde_json::json!({}), "", &state).await;

    let callback_listener = bind(&config.callback_addr).await;
    let internal_listener = match &config.internal {
        config::InternalListener::Tcp(addr) => http::InternalSocket::Tcp(bind(addr).await),
        config::InternalListener::Unix(path) => {
            let listener = http::bind_socket(path).unwrap_or_else(|e| {
                eprintln!("{e}");
                std::process::exit(1);
            });
            http::InternalSocket::Unix(listener)
        }
    };

    tracing::info!(
        "swisha payout and health listening on {}",
        config.internal.describe()
    );
    if config.api_secret.is_none() {
        tracing::info!("no shared secret configured: the listener itself is the boundary");
    }
    tracing::info!("swisha callback listening on {}", config.callback_addr);
    if config.swish_env == "production" {
        let ips = http::routes::callback::active_callback_ips();
        tracing::info!("callback IP whitelist ({} entries): {}", ips.len(), ips.join(", "));
    } else {
        tracing::warn!("swish running in '{}' mode – callback IP allowlist is DISABLED, any IP can submit callbacks", config.swish_env);
    }

    let (stopped, limits) = (shutdown_signal(), http::Limits::default());
    match http::serve(internal_listener, callback_listener, state, stopped, SHUTDOWN_DRAIN, limits).await {
        Ok(()) => tracing::info!("stopped"),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

fn command(args: &[String]) -> Option<Command> {
    match args {
        [] => Some(Command::Serve),
        [flag] if flag == "--print-schema" => Some(Command::PrintSchema),
        [flag] if flag == "--list-payers" => Some(Command::ListPayers),
        [flag, name] if flag == "--new-key" => Some(Command::NewKey(name.clone())),
        [flag, name] if flag == "--disable" => Some(Command::Disable(name.clone())),
        [flag, name] if flag == "--enable" => Some(Command::Enable(name.clone())),
        _ => None,
    }
}

// An operator's command, against the database the service uses. Answers the exit code.
async fn run(state: &state::SharedState, command: Command) -> i32 {
    let user = std::env::var("SUDO_USER").or_else(|_| std::env::var("USER"));
    let request = swisha::domain::request::PersonRequest {
        by: format!("{} on the server", user.as_deref().unwrap_or("someone")),
        note: None,
    };
    let done = match command {
        Command::NewKey(name) => payers::replace_key(state, &name, None, &request, "").await.map(|key| {
            eprintln!("{name} has a new key, and its old one no longer works. It is shown this once:");
            println!("{key}");
        }),
        Command::Disable(name) => payers::set_disabled(state, &name, true, &request, "").await.map(|()| {
            eprintln!("{name} is disabled: it takes no new payouts, and its earlier ones still settle.");
        }),
        Command::Enable(name) => payers::set_disabled(state, &name, false, &request, "").await.map(|()| {
            eprintln!("{name} is enabled and takes payouts again.");
        }),
        Command::ListPayers => match payers::refresh(state).await {
            Ok(()) => {
                print!("{}", payers::listing(&state.payers.snapshot(), swisha::util::time::unix_now()));
                Ok(())
            }
            Err(e) => Err(payers::PayerError::Failed(format!("Could not read the numbers: {e}"))),
        },
        Command::Serve | Command::PrintSchema => Ok(()),
    };
    match done {
        Ok(()) => 0,
        Err(payers::PayerError::NotFound) => {
            eprintln!("No number is stored under that name.");
            1
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

// SIGTERM is what systemd and docker send to stop a service; SIGINT is Ctrl-C at a terminal.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).unwrap_or_else(|e| {
        eprintln!("Cannot listen for SIGTERM: {e}");
        std::process::exit(1);
    });
    tokio::select! {
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    tracing::info!("shutting down: finishing the requests in flight");
}

async fn bind(addr: &str) -> TcpListener {
    TcpListener::bind(addr).await.unwrap_or_else(|e| {
        eprintln!("Failed to bind to {addr}: {e}");
        std::process::exit(1);
    })
}

// RUST_LOG takes a bare level here. Per-target directives such as "swisha=debug,sqlx=warn"
// need tracing-subscriber's env-filter feature, which pulls in three regex crates.
fn log_level(env: &swisha::env::Env) -> Level {
    match env.optional("RUST_LOG", "").trim().to_ascii_lowercase().as_str() {
        "error" => Level::ERROR,
        "warn"  => Level::WARN,
        "debug" => Level::DEBUG,
        "trace" => Level::TRACE,
        _       => Level::INFO,
    }
}
