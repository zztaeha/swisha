#![cfg(feature = "http")]

// How often a burst of requests reads every number. A binary of its own: the statements are
// counted through a log subscriber, which a subscriber set for one test would miss in another.

mod common;

use std::os::unix::fs::PermissionsExt;

use swisha::store::PayoutStore;

#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Captured {
    fn count(&self, statement: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().expect("capture")).matches(statement).count()
    }
}

// A number another instance stored, asked for here by 32 requests at once before this instance's
// next refresh. They share one read of every number: 10,000 numbers took 320 to 787 ms a read.
#[tokio::test(flavor = "current_thread")]
async fn requests_for_a_number_new_here_share_one_read() {
    let captured = Captured::default();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    );
    let config = common::config();
    let (state, _pool) = common::state_and_pool_with(config.clone(), Vec::new()).await;
    let (name, number) = ("sthlm", "1234679304");
    let hash = swisha::payers::key_hash(&common::key(name)).expect("a test key");
    let row = swisha::domain::payer::NewPayer { name, nickname: name, number, added_by: "Anna", key_hash: &hash };
    state.store.add_payer(&row, &common::logged()).await.expect("store");
    let folder = config.cert_dir.join(number);
    std::fs::create_dir(&folder).expect("the number's folder");
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    for (role, days) in [("tls", "30"), ("signing", "31")] {
        let file = folder.join(format!("{role}-{number}-{name}.pem"));
        std::fs::write(&file, common::bundle(number, days)).expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    let read = format!("FROM {} ORDER BY name", config.table_payers);
    let before = captured.count(&read);
    let finds: Vec<_> = (0..32)
        .map(|_| {
            let state = state.clone();
            tokio::spawn(async move { swisha::payers::find(&state, name).await.is_some_and(|entry| entry.loaded.is_ok()) })
        })
        .collect();
    for find in finds {
        assert!(find.await.expect("join"), "found, and loaded");
    }
    assert_eq!(captured.count(&read) - before, 1, "one read of every number for the 32");
}
