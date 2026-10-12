//! The HTTP layer: two listeners, deliberately separate.
//!
//! [`internal_router`] carries everything the operator's own backend calls. [`callback_router`]
//! carries the one route Swish itself calls, and nothing else, so a proxy misconfiguration
//! cannot expose the payout endpoint to the internet.

pub mod access;
pub mod client_ip;
pub mod rate_limit;
pub mod response;
pub mod routes;
pub mod secret;

use std::net::SocketAddr;
use std::time::Duration;

use axum::{
    Router,
    routing::{get, post, put},
};
use tokio::net::{TcpListener, UnixListener};

use crate::state::SharedState;

/// Routes only the operator's own backend may reach: payout, confirm and redo, status, events,
/// health, and the numbers swisha pays from. Behind the secret, every route about a stored number
/// also needs that number's key: see [`access`].
///
/// Each request runs to its end even when its caller hangs up, so a change is never cut off
/// between a write and what must follow it, such as a redo claimed and never sent.
///
/// Kept off the callback listener so no proxy misconfiguration can expose the payout endpoint.
/// [`serve`] binds both routers. To embed them in your own axum server instead, give each its
/// state, keep them on separate listeners, and serve both with the caller's address: the callback
/// checks it against Swish's, and the API logs and rate-limits by it. Served so, they go without
/// [`serve`]'s header and body deadlines, its cap on a request's head, its connection caps and its
/// drain.
///
/// ```no_run
/// # async fn wire(state: swisha::state::SharedState) -> std::io::Result<()> {
/// use std::net::SocketAddr;
/// use tokio::net::TcpListener;
///
/// let internal = swisha::http::internal_router().with_state(state.clone());
/// let callback = swisha::http::callback_router().with_state(state);
/// let api = TcpListener::bind("127.0.0.1:8083").await?;
/// let swish = TcpListener::bind("127.0.0.1:8084").await?;
/// tokio::spawn(async move {
///     axum::serve(api, internal.into_make_service_with_connect_info::<SocketAddr>()).await
/// });
/// axum::serve(swish, callback.into_make_service_with_connect_info::<SocketAddr>()).await
/// # }
/// ```
pub fn internal_router() -> Router<SharedState> {
    Router::new()
        .route("/system/health", get(routes::health::get))
        .route("/swish/payout", post(routes::payout::payout))
        .route("/swish/payout/{payer}/{reference}/confirm", post(routes::confirm::confirm))
        .route("/swish/payout/{payer}/{reference}/redo", post(routes::payout::redo))
        .route("/swish/status/{payer}/{reference}", get(routes::status::status))
        .route("/swish/status/{reference}", get(routes::status::status_by_reference))
        .route("/events", get(routes::events::events))
        .route("/swish/payers", post(routes::payers::add))
        .route("/swish/payers/{name}", get(routes::payers::read).patch(routes::payers::nickname))
        .route("/swish/payers/{name}/certificates", put(routes::payers::certificates))
        .route("/swish/payers/{name}/disable", post(routes::payers::disable))
        .route("/swish/payers/{name}/enable", post(routes::payers::enable))
        .route("/swish/payers/{name}/key", post(routes::payers::key))
        .route_layer(axum::middleware::from_fn(to_the_end))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
}

// hyper drops a request's future when its connection closes, so the work runs in a task of its own.
// Under serve it is counted too, so a stop waits for work whose caller already left.
async fn to_the_end(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let started = request.extensions().get::<Unfinished>().map(Unfinished::start);
    let work = tokio::spawn(async move {
        let _started = started;
        next.run(request).await
    });
    work.await.unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
}

// The requests whose work is still running, whether or not their callers are still there.
#[derive(Clone)]
struct Unfinished(std::sync::Arc<tokio::sync::watch::Sender<usize>>);

impl Unfinished {
    fn new() -> Self {
        Self(std::sync::Arc::new(tokio::sync::watch::Sender::new(0)))
    }

    fn start(&self) -> Started {
        self.0.send_modify(|running| *running += 1);
        Started(std::sync::Arc::clone(&self.0))
    }

    async fn done(&self) {
        let _ = self.0.subscribe().wait_for(|running| *running == 0).await;
    }
}

// One request's work, counted until it ends.
struct Started(std::sync::Arc<tokio::sync::watch::Sender<usize>>);

impl Drop for Started {
    fn drop(&mut self) {
        self.0.send_modify(|running| *running -= 1);
    }
}

/// The one route Swish itself calls, and so the only one that has to be publicly routable. A body
/// over 64 KB, far more than any callback Swish sends, is refused with `413` unread.
pub fn callback_router() -> Router<SharedState> {
    Router::new()
        .route("/swish/callback", post(routes::callback::callback))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
}

// Each listener owns its whole surface, unknown paths included, so a routing test exercises
// what actually ships rather than a router main.rs then adds to.
async fn not_found() -> crate::error::ApiError {
    crate::error::ApiError::NotFound
}

async fn method_not_allowed() -> crate::error::ApiError {
    crate::error::ApiError::Rejected { status: 405, message: "Method not allowed.".into() }
}

/// Where the internal API listens, already bound.
pub enum InternalSocket {
    /// A network address. Each request's peer address reaches the handlers.
    Tcp(TcpListener),
    /// A Unix socket, guarded by its file permissions.
    Unix(UnixListener),
}

/// Binds the internal API's Unix socket at `path` with mode `0660`, and reads the mode back
/// before anything is served.
///
/// The permission bits are the whole authentication, so they are set here rather than inherited
/// from the umask: `022` would leave the socket unwritable by its own group, `000` would open it
/// to the whole host. The socket is bound in a folder only swisha can enter and moved to `path`
/// once its mode is set, so no connection can reach it before. A stale socket left by an unclean
/// stop is replaced; anything else at `path` is refused.
///
/// # Errors
///
/// When the directory does not exist, `path` holds something that is not a socket, the bind, the
/// permissions or the move fail, or the socket would be reachable by other users.
pub fn bind_socket(path: &std::path::Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::FileTypeExt;
    let fail = |message: String| std::io::Error::other(message);

    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty());
    let parent = parent.unwrap_or(std::path::Path::new("."));
    if !parent.exists() {
        return Err(fail(format!("Socket directory does not exist: {}", parent.display())));
    }
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && !meta.file_type().is_socket()
    {
        return Err(fail(format!("{} exists and is not a socket, refusing to replace it", path.display())));
    }

    let staging = parent.join(format!(".swisha-{}", &crate::swish::random_payout_uuid()[..12]));
    crate::util::fs::create_private_dir(&staging).map_err(|e| fail(e.to_string()))?;
    let bound = bind_staged(&staging.join("s"), path);
    let _ = std::fs::remove_dir_all(&staging);
    bound
}

// Binds at `staged`, sets and confirms 0660, then moves the socket to `path`, replacing a stale one
// in the same step.
fn bind_staged(staged: &std::path::Path, path: &std::path::Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    let fail = |message: String| std::io::Error::other(message);

    let listener = UnixListener::bind(staged).map_err(|e| fail(format!("Failed to bind {}: {e}", path.display())))?;
    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o660))
        .map_err(|e| fail(format!("Cannot set permissions on {}: {e}", path.display())))?;

    // Confirm rather than assume: this is the whole access control.
    let mode = std::fs::metadata(staged)
        .map_err(|e| fail(format!("Cannot read back {}: {e}", path.display())))?
        .permissions()
        .mode();
    if mode & 0o007 != 0 {
        return Err(fail(format!(
            "{} is world-accessible ({:o}), refusing to serve payouts",
            path.display(),
            mode & 0o777
        )));
    }
    std::fs::rename(staged, path)
        .map_err(|e| fail(format!("Cannot move the socket into place at {}: {e}", path.display())))?;
    Ok(listener)
}

/// What a listener lets its callers hold open. Each connection holds a file, and a payout that
/// cannot open one to Swish fails with its reference spent.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// How long a connection may take to send a request's headers: its first, or its next once
    /// kept alive. A connection that takes longer is closed.
    pub header_read_timeout: Duration,
    /// How long a request may take to send its body, from the end of its headers. A body still
    /// arriving then is refused, and its connection closed.
    pub body_read_timeout: Duration,
    /// How many connections the internal API holds open at once. More wait in the kernel's queue
    /// until one closes.
    pub internal_connections: usize,
    /// How many connections the callback listener holds open at once.
    pub callback_connections: usize,
}

impl Default for Limits {
    /// 30 seconds for headers, and 30 for a body. 12,000 connections on the internal API: every
    /// event stream swisha allows, and 2,000 beside them. 1,024 on the callback, behind its proxy.
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(30),
            body_read_timeout: Duration::from_secs(30),
            internal_connections: crate::events::STREAMS + 2_000,
            callback_connections: 1_024,
        }
    }
}

/// Serves both listeners until `shutdown` resolves, then stops accepting and lets the requests in
/// flight finish, for at most `drain`.
///
/// A payout caught mid-request by a restart therefore still gets its answer from Swish and its
/// row written, even when its caller has already hung up. Event streams are ended first, so the
/// drain waits for payouts and nothing else.
/// A listener stopping on its own is an error, so the caller can exit and a supervisor restart
/// both rather than leave the service half-serving. Background polls are not waited for: their
/// payouts are already stored and the checks pick them up.
///
/// Each listener holds at most as many connections as `limits` says, and closes one that takes
/// longer than its header timeout to send a request's headers, or its body timeout to send the
/// body. A head over 16 KB is refused with `431`.
///
/// # Errors
///
/// When either listener stops before `shutdown` resolves.
///
/// # Examples
///
/// ```no_run
/// # async fn run(state: swisha::state::SharedState) -> std::io::Result<()> {
/// use std::time::Duration;
/// use swisha::http::{InternalSocket, Limits, serve};
/// use tokio::net::TcpListener;
///
/// let internal = InternalSocket::Tcp(TcpListener::bind("127.0.0.1:8083").await?);
/// let callback = TcpListener::bind("127.0.0.1:8084").await?;
/// let stop = async {
///     let _ = tokio::signal::ctrl_c().await;
/// };
/// serve(internal, callback, state, stop, Duration::from_secs(60), Limits::default()).await
/// # }
/// ```
pub async fn serve(
    internal: InternalSocket,
    callback: TcpListener,
    state: SharedState,
    shutdown: impl Future<Output = ()> + Send + 'static,
    drain: Duration,
    limits: Limits,
) -> std::io::Result<()> {
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let (stream, unfinished) = (state.stream.clone(), Unfinished::new());
    let internal_router = internal_router().with_state(state.clone());
    let mut internal_task = match internal {
        InternalSocket::Tcp(listener) => tokio::spawn(accept(
            listener,
            internal_router,
            Some,
            limits.internal_connections,
            limits,
            stopped.clone(),
            unfinished.clone(),
        )),
        InternalSocket::Unix(listener) => tokio::spawn(accept(
            listener,
            internal_router,
            |_| None,
            limits.internal_connections,
            limits,
            stopped.clone(),
            unfinished.clone(),
        )),
    };
    let mut callback_task = tokio::spawn(accept(
        callback,
        callback_router().with_state(state),
        Some,
        limits.callback_connections,
        limits,
        stopped,
        unfinished.clone(),
    ));

    tokio::select! {
        result = &mut internal_task => return Err(stopped_early("internal", result)),
        result = &mut callback_task => return Err(stopped_early("callback", result)),
        () = shutdown => {}
    }

    let _ = stop.send(true);
    stream.close();
    let drained = tokio::time::timeout(drain, async {
        let _ = internal_task.await;
        let _ = callback_task.await;
        unfinished.done().await;
    })
    .await;
    if drained.is_err() {
        tracing::warn!("requests were still running after {drain:?}, stopping anyway");
    }
    Ok(())
}

// Accepts until `stopped`, holding at most `limit` connections open, then lets those still open
// finish: each is closed once the request it is serving has its answer.
async fn accept<L>(
    mut listener: L,
    router: Router,
    peer: fn(L::Addr) -> Option<SocketAddr>,
    limit: usize,
    limits: Limits,
    mut stopped: tokio::sync::watch::Receiver<bool>,
    unfinished: Unfinished,
) where
    L: axum::serve::Listener,
{
    use axum::extract::Request;
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioIo, TokioTimer};

    let open = std::sync::Arc::new(tokio::sync::Semaphore::new(limit));
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    loop {
        let permit = tokio::select! {
            permit = open.clone().acquire_owned() => permit,
            _ = stopped.wait_for(|stop| *stop) => break,
        };
        // The semaphore is never closed, so only a stop ends the wait above.
        let Ok(permit) = permit else { break };
        let (io, addr) = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = stopped.wait_for(|stop| *stop) => break,
        };
        let (peer, router, unfinished) = (peer(addr), router.clone(), unfinished.clone());
        let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
            if let Some(peer) = peer {
                request.extensions_mut().insert(axum::extract::ConnectInfo(peer));
            }
            request.extensions_mut().insert(unfinished.clone());
            let expires = Box::pin(tokio::time::sleep(limits.body_read_timeout));
            respond(router.clone(), request.map(|body| Deadline { body, expires }))
        });
        // hyper buffers about 400 KB of an unfinished head by default: 500 held 210 MB, measured.
        // 16 KB holds every head swisha takes, and 500 heads just under it held 13 MB.
        let connection = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(limits.header_read_timeout)
            .max_buf_size(16 * 1024)
            .serve_connection(TokioIo::new(io), service);
        let connection = graceful.watch(connection);
        tokio::spawn(async move {
            let _ = connection.await;
            drop(permit);
        });
    }
    graceful.shutdown().await;
}

// A request's body, failed once its deadline passes: a sender that stops midway cannot hold its
// connection, and its place under the cap, for longer.
struct Deadline {
    body: hyper::body::Incoming,
    expires: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl hyper::body::Body for Deadline {
    type Data = axum::body::Bytes;
    type Error = axum::BoxError;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        match std::pin::Pin::new(&mut self.body).poll_frame(cx) {
            std::task::Poll::Pending if self.expires.as_mut().poll(cx).is_ready() => {
                std::task::Poll::Ready(Some(Err("the request body took too long to arrive".into())))
            }
            polled => polled.map_err(Into::into),
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

// The router answers through tower's Service trait, which swisha only reaches through axum's
// extension of it. A router is always ready, so it is called without waiting.
fn respond<S: axum::ServiceExt<R>, R>(mut router: S, request: R) -> S::Future {
    router.call(request)
}

fn stopped_early(listener: &str, result: Result<(), tokio::task::JoinError>) -> std::io::Error {
    std::io::Error::other(format!("the {listener} listener stopped on its own: {result:?}"))
}
