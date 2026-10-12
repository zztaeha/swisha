# Security policy

swisha moves money. A defect here does not corrupt data, it pays someone, so security reports
are treated as the highest priority work in the project.

## Reporting a vulnerability

**Do not open a public issue.**

Use GitHub's private vulnerability reporting: the **Security** tab on this repository, then
**Report a vulnerability**. If that is unavailable to you, email **dev@calia.cc** with `swisha
security` in the subject.

Useful in a report, in rough order of value:

- What an attacker gains. A double payout, a payout to the wrong recipient and a leak of personal
  data are all more serious than a crash.
- The smallest sequence of requests or configuration that reproduces it.
- Which version, and whether the internal API was on a Unix socket, a loopback port or a public
  address.

**Test against the MSS simulator, never against production Swish.** Set `SWISH_ENV=test` and use
the test certificates from Swish's developer portal. No money moves there.

You will get an acknowledgment within seven days. If the report is confirmed, expect a fix or a
documented mitigation before anything is disclosed publicly, and credit in the release notes
unless you would rather not have it. Please allow 90 days before disclosing, or less if a fix
ships sooner.

## Supported versions

Only the latest release is supported, and fixes are not backported.

## What swisha guards

These are the properties a report can be written against. If you can break one, that is a
vulnerability.

| Property | How |
|---|---|
| One reference produces at most one payout | An atomic claim on the payer and reference before anything is sent |
| swisha never resubmits a payout on its own | A single submission call site, asserted by a test that scans the source. A person's redo goes through it as a new payout under a new reference |
| A settled payout never moves back | Nothing leaves `PAID`. `DEBITED` moves only on, to the `PAID` or the late `ERROR` Swish reports, and neither move goes back |
| swisha's own guess never replaces Swish's answer | `PENDING` and `NEEDS_REVIEW`, what swisha records when Swish has not answered, replace only `CREATED` or `PENDING` |
| A redo never pays a payout whose money left | Swish is asked about the payout, and every payout it redoes, first, and the redo goes out only on its word that each failed or never reached it. Swish's acceptance is kept on the trail, from its `201`, a callback, an answer about the instruction or `RP09`, so its later saying it has no record of a payout it took is refused, never read as "never sent". Its having no record proves nothing either where the trail lacks how the send ended, and is refused. The store then refuses a redo of `DEBITED`, `PAID`, `CREATED` or `PENDING`, or of any payout after one Swish paid or one at a status this version does not know, and claims one redo per payout under a row lock, with a unique index behind it |
| A payout Swish reports paid after it was redone is flagged | Once Swish's word reaches swisha, however it arrives, an `error` line in the service log names both payouts, both carry a review saying the recipient may be paid twice, health counts it, and nothing after them can be redone again |
| A manual confirmation names who and why | `by` and `note` are required and kept in the payout's history with the caller's IP, in the same write as the status, or not at all. It is refused until swisha has asked Swish one last time, and on a payout that was redone. A failure Swish reports afterwards cannot undo it, and is flagged instead: an `error` line in the service log, and a review on the payout |
| A repeat request cannot change an amount or a recipient | A claim never rewrites a stored payout, whatever its status, including one this version does not know |
| The payout endpoint is never unguarded | swisha refuses to start on a public address without a shared secret |
| The API secret alone reaches no stored number | Every route about a number also needs that number's key, checked from the headers alone before its path or body is read. The secret alone adds a number and reads health, which counts numbers and names none |
| A key reaches one number | A key for one number used on another is refused with `403`, which never says whether that other number exists. A stream opened with a key carries that number alone, and the service log, which spans every number, is never streamed |
| Keys are never stored | A key is 32 random bytes, shown once when the number is added or the key replaced. The store keeps its SHA-256, looked up through a unique index, and no log line holds a key |
| A change to a number names who made it | Adding a number, replacing its key or certificates, renaming, disabling or enabling it is written in the same write as its service log line, with `by`, the change and the caller's address. A line that cannot be written stops the change, and certificates already swapped in are swapped back |
| A replaced key dies at once | The key is read from the store on every request, never cached, so a replaced key fails on every instance from the moment it is replaced. Of two replacements made with one key, exactly one wins. An open event stream ends at once on the instance that replaced it, and within 10 seconds on the others |
| A disable stops payouts at once | A number's disabled flag is read with its key, so every instance refuses its next payout from that moment |
| The shared secret does not leak through timing | Compared byte by byte with no early exit once the lengths match |
| A forged callback cannot settle a payout | Swish's addresses in production, read from `X-Forwarded-For` behind a trusted proxy and never from `X-Real-IP`; a number swisha pays from; the stored instruction UUID; and the echoed `callbackIdentifier` header, compared in constant time. The UUID is in every answer, so the `callbackIdentifier` is the secret. A faked failure still cannot lead to a second payout, since a redo asks Swish first |
| A caller cannot pick its own address | Forwarded headers are read on the callback listener alone. The internal API's rate limit and audit trail take each caller's own address |
| Nothing swisha sends follows a redirect | Neither of swisha's HTTP clients follows one: a redirect from anything posing as Swish is its answer, so a signed payout and its personnummer never go where it points |
| Idle connections cannot starve a payout | Each listener holds a bounded number of connections, and closes one that takes over 30 seconds to send a request's headers, or whose body is still arriving 30 seconds after them. A head over 16 KB is refused. One number holds at most 64 event streams and swisha 10,000. Where a secret is set, a body is read only once the secret and the key have passed. In production a callback from outside Swish's addresses is refused before its body is read, and a callback body over 64 KB is refused unread. The caps assume the shipped unit's `LimitNOFILE=65536` |
| Neither a person's text, a path nor a callback can forge a log line | `by`, `note` and a nickname refuse control characters, line and paragraph separators, and format characters such as bidi overrides. A reference in a path that no payout could carry is answered `404` before the store is read or anything is logged. Every value a callback brings is quoted, escaped and cut to 64 characters in the log |
| Swish's identity is verified | rustls with the system trust store, which holds Swish's public DigiCert roots. Certificate verification is never relaxed, and no setting adds a trust anchor |
| A certificate swisha could not pay with is never kept | Both files are checked when a number is added and when its certificates are replaced: a certificate and its own key, not expired, and not issued to another Swish number, which Swish would refuse as `PA01` on every payout |
| Keys and the env file are closed to other users | swisha refuses to start while another user can open the env file or the certificate folder, writes every certificate file readable by itself alone, and will not load one other users can open. Not even its group may read one: the application joins swisha's group to reach the socket. A copy a crash leaves mid-write is just as closed, and removed once it is an hour old; a number's own folder a replacement moved aside goes back at once |
| The socket is never open before its mode is set | It is bound in a folder only swisha can enter, set to `0660` and read back, then moved into place, whatever the umask |
| Private keys stay out of the database, and a caller never names a path | A number's two files live in `SWISH_CERT_DIR`, in a folder named for its Swish number, as `tls-{number}-{name}.pem` and `signing-{number}-{name}.pem`. swisha builds every path from a number of digits alone and a name of `a-z`, `0-9`, `-` and `_` alone, checked again on every read, so a row changed in the database by hand never names a path outside the folder. The API takes the files' contents, never a path |
| A number's name is permanent, and a number cannot pay its past references again under a new one | The name is the store's key and no endpoint changes it. Adding a number refuses one with payouts on record under another name, and the refusal never names the other number |
| One number's trouble never stops another's | Each number loads and is checked on its own, has its own payout budget, and streams on its own channel. One whose certificates fail to load refuses its own payouts before anything is stored, and the others pay. A number flooding its budget, or one whose Swish hangs, left 900 others paying as fast as before when measured |
| The database connection is encrypted off this host | TLS is required for a database on another host unless `DB_SSLMODE` says otherwise; `verify-full` also checks the server's certificate |
| Secrets stay out of logs | The database password and the shared secret print as `<redacted>` in any debug output, and an env file's debug output names its keys only. No request's debug output holds a personnummer, a phone number or a private key |
| One payer cannot act for another | Each payer signs with its own key, asks Swish with its own certificate, and announces on its own channels |
| No `unsafe` code in swisha itself | `#![forbid(unsafe_code)]` on the library and the binary. The dependencies it builds on, among them ring, rustls, tokio and sqlx, use `unsafe` internally and are audited by their own projects |

The tests that hold these are in `tests/`: `security.rs`, `safety.rs`, `no_resubmission.rs`,
`failure.rs`, `manual.rs`, `lifecycle.rs`, `payers.rs`, `payer_admin.rs`, `registry.rs`,
`keys.rs`, `checks.rs`, `routing.rs`, `config.rs`, `startup.rs`, `limits.rs`, `socket.rs`,
`client_ip.rs`, `callback_log.rs` and `path_log.rs`, and the storage contract in
`src/store/conformance.rs`.

## What the operator is responsible for

swisha cannot enforce these from inside the process. Getting one wrong is a real vulnerability in
a deployment, but it is not a defect in swisha.

- **The reverse proxy must overwrite `X-Forwarded-For`, not append to it.** The common nginx
  snippet appends, which lets a caller put a Swish address in the header and defeat the callback
  allowlist. The README shows the correct block and why.
- **Only the callback route may be public.** A prefix match instead of an exact one publishes the
  payout endpoint to the internet.
- **`TRUSTED_PROXY` must name the proxy**, or forwarded addresses are ignored and every genuine
  callback is refused. It applies to the callback listener alone, and it trusts that proxy's
  host: anything there can reach the listener as the proxy does and claim any address, so the
  `callbackIdentifier` is what holds against a forged callback from there.
- **The internal API speaks plain HTTP.** On anything but loopback the secret, every key and each
  payout's personnummer cross the network readable, and swisha warns at startup. Keep that hop on
  a private network, or behind a TLS terminator on swisha's own host, with the secret set: on
  loopback swisha takes requests without one, and the terminator forwards anyone's.
- **The database must be closed to other roles.** `examples/setup.sql.example` revokes `CONNECT`
  and `TEMPORARY` from `PUBLIC`. Without that, any role on the server can `LISTEN` to payout
  events, recipients included, and `NOTIFY` forged ones. A role granted `CONNECT` later, such as a
  city's staff for a view, can do the same on every number's channels, whatever it may `SELECT`.
  Serve per-city reads through your backend, and take a notification as a reason to read the
  status endpoint, never as a payout's state.
- **`examples/docker-compose.yml.example` is a development database.** Its superuser's password is
  in the file, so it listens on loopback alone. Never keep real payouts in it.
- **A database on another host should use `DB_SSLMODE=verify-full`.** The default, `require`,
  encrypts the connection but does not check who answers it.
- **`SWISH_CERT_DIR` is made by hand, owned by the service account and closed to everyone
  else**, on the host's own disk and outside any web root. swisha never creates it, and refuses a
  folder or a file other users can open, but cannot tell whether the account that owns it is the
  right one.
- **A backup of the certificate folder holds every number's private key.** Encrypt it, keep it
  as closed as the folder, and restore it with its owner and modes.
- **The `events` and `logs` event channels carry amounts and personal data.** Only `updates` is
  safe to forward to a browser.
- **Keep each number's key where only that number's owner reaches it.** A backend that stores
  every customer's key reaches every customer's number when it is broken into; one that passes
  each customer's own key through holds none. A key shown in a log, a ticket or a screenshot is
  replaced, not trusted.
- **Who among a number's users may confirm or redo a payout, or replace its certificates, is yours
  to enforce.** swisha records the `by` it is sent but cannot verify it: anyone holding the
  secret and a number's key can call every endpoint for that number. Anyone holding the secret
  can add a number.
- **The env file is full control.** The server commands `--new-key`, `--disable`, `--enable` and
  `--list-payers` act on every number with it, so who can read it, or run as the service account,
  is who controls every number.
- **Running swisha on more than one host needs the certificate folder shared**, at the same path
  on each, mounted for the service account alone with the same user ID on every host. A host
  that cannot read a number's files holds that number aside and refuses its payouts.
- **Keep the shared secret out of version control.** The shipped `.gitignore` covers `*.env`,
  `*.pem`, `*.key`, `*.p12` and `*.csr` for that reason.

## Deliberate decisions that are not bugs

Reports about these are welcome as discussion, but they are choices rather than oversights.

- **swisha never retries a payout on its own.** A resubmission needs a fresh
  `payoutInstructionUUID`, which Swish cannot tie back to the original, so an already-debited
  payout could be debited again. Checking only ever reads, and paying again is a person's redo.
- **The event stream is lossy.** A subscriber that falls behind misses messages rather than
  slowing the payout path. Anything that must not be missed is read back from the status endpoint.
- **The rate limit is per process.** Running several instances behind a load balancer multiplies
  it. It is a guard against runaway callers, not a quota, and `SWISH_PAYOUTS_PER_MINUTE` sets it.
- **Keys are hashed with SHA-256, not a slow password hash.** A key is 256 random bits rather
  than something a person chose, so there is nothing to guess, and a slow hash would only slow
  every request.
- **The Unix socket path is not a secret.** The permission bits are the authentication, and the
  kernel checks them before swisha sees a byte.
- **Outside production the callback allowlist logs but does not enforce.** This is what lets the
  MSS simulator and local tests work.

## Scope

In scope: this repository, and any deployment following the README.

Out of scope: vulnerabilities in Swish's own API or simulator, which belong to Swish; findings
that require an attacker to already hold the env file, a signing key, write access to the
database, or a number's key together with the shared secret, which reach that number by design;
and denial of service through sheer request volume, which is the proxy's job. A handful of
connections or streams that makes a payout fail is in scope, as is a way to reach a stored number
with the shared secret alone, or one number with another's key.
