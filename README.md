# swisha

swisha sends Swish payouts for you. Your application asks it over HTTP to pay someone, and swisha
signs the payout, sends it to Swish once, follows it until Swish gives its final answer, and never
pays the same reference twice.

It runs as its own process, so the application calling it can be written in any language. Rust is
what swisha is built in, not something you need to write.

MIT · Rust 1.99 or newer · PostgreSQL 18, the only database it supports · Linux or macOS · no
`unsafe` · fourteen direct dependencies

**Start here:** [What it does](#what-it-does) · [Why it exists](#why-it-exists) ·
[How it fits together](#how-it-fits-together) · [Words used here](#words-used-here) ·
[What you need](#what-you-need) · [Quick start](#quick-start)

**Using it:** [How payouts work](#how-payouts-work) ·
[When a person has to decide](#when-a-person-has-to-decide) · [API](#api) ·
[Swish numbers](#swish-numbers)

**Running it:** [Configuration](#configuration) · [Security](#security) · [Database](#database) ·
[Going live](#going-live) · [Running in a container](#running-in-a-container) ·
[Running many numbers](#running-many-numbers) · [Monitoring](#monitoring) ·
[Troubleshooting](#troubleshooting) · [Upgrading](#upgrading)

**More:** [Using it as a library](#using-it-as-a-library) · [Development](#development) ·
[Reference](#reference) · [License](#license)

---

## Overview

### What it does

- **Signs correctly.** Double SHA-512 RSA, as the CPC payouts reference requires, verified end to
  end against the Swish MSS simulator.
- **Never pays twice on its own.** A reference is claimed atomically before anything is sent, and
  no code path resubmits one. A test asserts the crate has exactly one call site that can submit
  a payout at all. A person's redo goes through it too, as a new payout under a new reference.
- **Hears Swish's last word.** It asks Swish for the outcome right after every payout and follows
  it past `DEBITED` to `PAID`, then keeps checking anything unconfirmed for 48 hours, so a lost
  callback or a bank's maintenance window still settles. Checking only ever reads: it never moves
  money.
- **Hands the rest to a person, safely.** When Swish never answers or reports a failure, the API
  says what to check and offers two guarded actions: confirm the payout as paid, or redo it under
  a new reference. A redo asks Swish first and goes out only on its word that nothing was paid,
  once, however many people press at the same moment.
- **Pays from any number of Swish numbers.** Each is added through the API with its own two
  certificates, checked before they are kept, and runs as its own account: one number's backlog,
  outage or broken certificate never delays another's payouts or checks. One city's payouts,
  references and live events never touch another's.
- **Gives each number its own key.** The API secret on its own only adds numbers and reads health.
  Every request about a number also needs that number's key, so one leaked key, or one bug that
  sends a customer's request with another customer's number, reaches one number at most.
- **Says whose problem it is.** Health reports what swisha sees of Swish beside what Swish's own
  status page says about payouts, and warns a month before any certificate expires.
- **Explains failures.** 27 Swish error responses are mapped to a plain message and a category, in
  English or Swedish, so a person knows whether to pay again, ask the recipient to fix something,
  or escalate. The full list is in [Swish error codes](#swish-error-codes).
- **Stores everything in PostgreSQL 18**, the only database swisha supports.
- **Contains no `unsafe` code.** The crate is `#![forbid(unsafe_code)]`.

### Why it exists

Swish's payout API is not the same as its payment API, and the parts that differ are the parts
that are easy to get wrong:

- the payload must carry a **double SHA-512** RSA signature;
- the certificate that signs is not always the certificate that authenticates;
- a rejected signature comes back as an error that says nothing about signatures.

Most open examples are for payments rather than payouts, or predate the current API. swisha is a
working implementation with the awkward parts already solved and verified against Swish's own
simulator.

Payouts are business to consumer only: a merchant pays a person. One swisha pays from as many
Swish numbers as you add to it, each with its own certificates and its own key, and a caller
reaches a number only with that number's key.

### How it fits together

```text
                     internal API                          signed payout, mutual TLS
 Your backend  ─────────────────────────>  swisha  ─────────────────────────────────>  Swish
               Unix socket or               │  ▲
               127.0.0.1:8083               │  │  callback listener, 127.0.0.1:8084
               x-payer-key on each request  │  │
                                            │  └──── your reverse proxy (HTTPS) <──── Swish's callback
                                            ▼
                                      PostgreSQL 18
```

1. Your backend sends a payout to swisha's **internal API**, which only your own systems reach.
2. swisha claims the reference in PostgreSQL. A reference that is already claimed is refused, so
   a payout can never be sent twice.
3. swisha signs the payout with the number's signing key and sends it to Swish once, over a
   connection authenticated with the number's TLS certificate, and answers your backend `202`.
4. Swish tells swisha the outcome by calling its **callback listener**, which your reverse proxy
   publishes over HTTPS, and swisha also asks Swish directly. Either way the payout reaches
   `PAID`, or a status a person acts on.
5. Every change is written to PostgreSQL and announced on an event stream. Your backend can always
   read the payout back from the status endpoint.

### Words used here

| Word | Means |
|---|---|
| **Swish number**, or **number** | A merchant Swish number you pay from, 10 or 11 digits, such as `1231234567`. Each has two certificates and its own key |
| **payer**, or **name** | The permanent name a number is stored under, such as `sthlm`. Requests name the number by it |
| **reference** | Your identifier for one payout, such as `INV-1001`, sent to Swish as `payerPaymentReference`. With the payer, it is what stops a payout being sent twice |
| **recipient** | The person paid. `payee_alias` is their Swish number, `payee_ssn` their personnummer |
| **personnummer** | A Swedish personal identity number. swisha takes the 12-digit form, `YYYYMMDDNNNN` |
| **`swish_ref`** | The `payoutInstructionUUID` swisha made for a payout: what Swish knows it by |
| **key** | A number's own credential, `swk_` and 64 hex characters, sent as `x-payer-key`. Shown once |
| **API secret** | An optional shared secret for the whole internal API, sent as `x-api-secret` |
| **callback** | Swish calling swisha back with a payout's outcome |
| **checks** | swisha asking Swish about a payout that has no final answer yet. They only read |
| **confirm**, **redo** | What a person may do about a payout Swish never settled: mark it paid, or pay it again under a new reference |
| **MSS** | The Merchant Swish Simulator, Swish's test environment. No money moves there |

## Getting started

### What you need

- **Linux or macOS.** swisha is built on Unix sockets and Unix file permissions, and does not run
  on Windows.
- **Rust 1.99 or newer**, to build it.
- **PostgreSQL 18.** It is the only database swisha supports.
- **For each Swish number:** its two certificates from the Swish Certificate System, or Swish's
  test bundle to try it out.
- **For the quick start:** `curl`, `jq`, `openssl`, and `cloudflared` if you want Swish's callbacks
  to reach your machine.
- **In production:** a reverse proxy with an HTTPS certificate in front of the callback, such as
  nginx, and a systemd host or a container. See [Going live](#going-live).

### Quick start

This runs swisha against MSS, Swish's simulator, where no money moves.

**1. Build it.**

```bash
git clone https://github.com/zztaeha/swisha && cd swisha
cargo build --release
```

**2. Get the test certificates.** Swish publishes a bundle on its
[developer portal](https://developer.swish.nu/documentation/environments). This README was
verified with `MSS_test_3.0.zip`. Like production, it has two certificates: one that
authenticates the TLS connection and one that signs each payout. swisha reads each as **one file
holding the certificate and its key**, so join each pair once:

```bash
cd client_cert
cat Swish_Merchant_TestCertificate_1234679304.pem \
    Swish_Merchant_TestCertificate_1234679304.key > tls.pem
cat Swish_Merchant_TestSigningCertificate_1234679304.pem \
    Swish_Merchant_TestSigningCertificate_1234679304.key > signing.pem
chmod 600 tls.pem signing.pem
```

MSS refuses a payout signed with anything but the signing certificate. The bundle's
`Swish_TLS_RootCA.pem` is not needed: Swish's servers, the simulator included, use public
certificates the system already trusts.

Keys must be **PKCS#8**, the form that starts `-----BEGIN PRIVATE KEY-----`. If yours starts
`-----BEGIN RSA PRIVATE KEY-----` it is PKCS#1, the same key in a different wrapper:

```bash
openssl pkcs8 -topk8 -nocrypt -in swish.key -out swish.pkcs8.key
```

**3. Make the certificate folder.** You hand both files to swisha once it runs, and it keeps its
own copies, readable by itself alone, in a folder you give it. Make that folder now, closed to
other users. swisha never makes it, and refuses to start while it is missing or while other users
can open it:

```bash
mkdir -m 700 ~/swisha-certs
```

**4. Write the config file.** Copy the development template and fill in the fields marked
`required`: the two database fields, the callback URL from step 5, and `SWISH_CERT_DIR`, the
absolute path of the folder you just made. Everything else is blank, and blank means the default:

```bash
sudo mkdir -p /etc/swisha
sudo cp examples/swisha.dev.example /etc/swisha/dev.env
sudo chown "$USER" /etc/swisha/dev.env
chmod 600 /etc/swisha/dev.env
```

`DB_NAME` is the database you create in step 6, `swisha` here, and `DB_USER` the PostgreSQL role
that owns it. On a laptop that is usually your own login name; leave `DB_PASS` blank for a role
that needs no password. `/etc/swisha/` is where the rest of this README keeps configuration. The
file holds a database password, so it is yours alone from the start: swisha refuses to start while
other users can open it.

**5. Give Swish a callback address.** `SWISH_CALLBACK_URL` has to be set even locally, and to an
`https://` URL: Swish refuses every payout with any other, so swisha refuses to start with one.
You do not need callbacks to try swisha, since it asks Swish for the outcome after every payout
and follows it to `PAID` either way. A callback brings each step the moment it happens. For a real
callback on your machine, point a tunnel at the callback listener, and set `SWISH_CALLBACK_URL` to
the address it prints followed by `/swish/callback`:

```bash
cloudflared tunnel --url http://127.0.0.1:8084
```

A `~/.cloudflared/config.yml` applies to this tunnel too, and its ingress rules can answer `404`
before swisha sees anything. Add `--config` pointing at an empty file to leave it out.

**6. Create the database.** `--print-schema` writes the tables to stdout and needs no
configuration and no connection of its own:

```bash
createdb swisha
./target/release/swisha --print-schema | psql swisha
```

**7. Start swisha.**

```bash
SWISHA_ENV_FILE=/etc/swisha/dev.env ./target/release/swisha
```

swisha does not search for that file: it opens exactly the path it is given. An absolute path
loads the file you meant from any working directory, and still works unchanged when you later put
it in a systemd unit.

Two listeners come up: the internal API on `127.0.0.1:8083` and the Swish callback on
`127.0.0.1:8084`.

**8. Add the test number.** No number is stored yet. Its name, `test` here, is what every payout
from it names as `payer`, and it never changes. The test merchant number is `1234679304`, and
swisha checks that both certificates were issued to it. The files go in as text, which `jq` builds
from them:

```bash
ANSWER=$(jq -n --arg by "$USER" --rawfile tls tls.pem --rawfile signing signing.pem \
  '{name: "test", nickname: "MSS test number", number: "1234679304", tls: $tls, signing: $signing, by: $by}' |
  curl -s -X POST http://127.0.0.1:8083/swish/payers \
    -H 'x-api-secret: <your secret>' -H 'content-type: application/json' -d @-)
echo "$ANSWER"
KEY=$(jq -r .key <<<"$ANSWER")
```

Drop the `x-api-secret` header here and below if you have not set `API_SHARED_SECRET`. On a
loopback address a secret is optional, unless a TLS terminator or proxy forwards to it; anywhere
else swisha refuses to start without one. The key is never optional.

It answers `201` with the number as swisha now holds it, including what Swish said when swisha
tried the new certificate, and the number's `key`. **The key is shown this once**: swisha keeps
only its hash. Every request about this number carries it in `x-payer-key`, which is why the
commands above keep it in `$KEY`:

```json
{
  "added_by": "anna",
  "created_at": "2026-10-09T08:32:58+02:00",
  "disabled": false,
  "key": "swk_C4BEA16DB440175A347124ECE5682F303F9A49B05C231F41678FE338DD12B59E",
  "name": "test",
  "nickname": "MSS test number",
  "number": "1234679304",
  "problem": null,
  "signing_expires": "2027-09-11",
  "signing_expires_in_days": 337,
  "swish_online": true,
  "tls_expires": "2027-09-11",
  "tls_expires_in_days": 337,
  "usable": true
}
```

**9. Send a payout.**

```bash
curl -X POST http://127.0.0.1:8083/swish/payout \
  -H 'x-api-secret: <your secret>' -H "x-payer-key: $KEY" \
  -H 'content-type: application/json' \
  -d '{
    "reference": "INV-1001",
    "payee_alias": "0701234567",
    "payee_ssn": "196408233234",
    "amount": 250.00
  }'
```

```json
{ "payer": "test", "status": "CREATED", "success": true, "swish_ref": "7CBA599425BC499C93D6F0E7310085C3" }
```

The answer is `202 Accepted`: Swish took the instruction, and the outcome is still to come. It
arrives on the event stream, and can always be read back.

**10. Read it back.**

```bash
curl -s http://127.0.0.1:8083/swish/status/test/INV-1001 \
  -H 'x-api-secret: <your secret>' -H "x-payer-key: $KEY"
```

```json
{
  "actions": [],
  "error_category": null,
  "error_code": null,
  "error_message": null,
  "manual_confirmation": null,
  "payer": "test",
  "reference": "INV-1001",
  "replaced_by": null,
  "replaces": null,
  "review": null,
  "status": "PAID",
  "swish_ref": "7CBA599425BC499C93D6F0E7310085C3"
}
```

On the simulator, with no tunnel, that read showed `DEBITED` about 5 seconds after the payout and
`PAID` about 9 seconds after it. The key says which number is meant, so a payout may leave out
`payer`, and the status path may leave out its name: `/swish/status/INV-1001`.

**Where to go next:** [How payouts work](#how-payouts-work) explains what you just saw,
[API](#api) lists every endpoint, and [Going live](#going-live) is the production checklist. To
see a refusal on the simulator, send a payout whose `message` is `RF07`. A redo of it is declined
too, since a redo sends the same message.

## How payouts work

swisha stores what its payout state machine needs and nothing else. Invoices, orders and line
items stay in your own tables, keyed by the same payer and reference.

### The states

| Status | Meaning |
|---|---|
| `CREATED` | Claimed in the database, and sent or about to be. Swish may have taken it without an outcome yet: swisha keeps asking |
| `PENDING` | No answer yet: Swish has the instruction and has not resolved it, or swisha could not tell whether Swish received it. swisha keeps asking |
| `DEBITED` | The money has left your account. Only Swish's `PAID` or `ERROR` can follow |
| `PAID` | The recipient has the money, on Swish's word or a person's [confirmation](#post-swishpayoutpayerreferenceconfirm). **Final** |
| `DECLINED` | Swish refused the payout. No money moved |
| `ERROR` | Swish reported the payout failed. After `DEBITED`, the money is on its way back |
| `NEEDS_REVIEW` | Swish never answered, and swisha has stopped asking. A person decides |

Every payout starts at `CREATED` and moves once an answer arrives:

```text
CREATED ──┬──> DEBITED ──┬──> PAID    the recipient has it
          │              └──> ERROR   the recipient's bank refused it, and the money comes back
          ├──> DECLINED               Swish refused it, no money moved
          ├──> ERROR                  Swish reported it failed
          └──> PENDING                no answer yet

CREATED or PENDING ──> NEEDS_REVIEW   no answer in 48 hours of checks
```

Those are the usual paths. Any other move an answer from Swish brings is allowed too, within the
three rules below: Swish's `PAID` with no `DEBITED` before it, for example, or a failure that later
goes through.

Three rules keep a payout's status honest:

- **Nothing leaves `PAID`, and `DEBITED` only moves on.** Swish reports a successful payout as two
  callbacks seconds apart: `DEBITED` when the money leaves your account, then `PAID` when the
  recipient has it. A late failure is `DEBITED` then `ERROR`: the recipient's bank refused the
  money, during its maintenance for example, and Swish returns it. So `DEBITED` accepts `PAID` or
  `ERROR` and nothing else, and neither move goes back: a delayed `DEBITED` arriving after the
  `ERROR` changes nothing. `PAID` accepts nothing at all.
- **swisha's own "no answer" never replaces Swish's.** `PENDING` and `NEEDS_REVIEW` are what
  swisha records when Swish has not answered, so they only ever follow `CREATED` or `PENDING`. An
  answer Swish gave stands however the timing falls, and is the only status announced.
- **`NEEDS_REVIEW` is not closed.** An answer Swish gives later still settles it.

If you consume the event stream, **a successful payout emits two status updates**, not one. Treat
`DEBITED` as sent and `PAID` as received, rather than treating the first update you see as the
final answer.

### What happens after you send a payout

1. **One submission.** The payout is submitted once, from its payer's number, signed with its
   payer's key. That is the crate's only `POST` to Swish.
2. **The follow-up.** swisha asks Swish for the outcome half a second after submitting, then every
   2 seconds, up to 8 times. When Swish answers `DEBITED`, swisha keeps asking, up to 8 more times,
   until it says `PAID` or `ERROR`. A callback can settle it at any point, and swisha then stops
   asking for what the callback brought.
3. **The checks.** Anything Swish has not given its last word on is checked again: 5 minutes after
   it last changed, then hourly, until 48 hours after the payout was made. That covers a lost
   callback and a bank's maintenance window. Each number is checked on its own, one question a
   second, so no number waits on another's backlog and none reaches Swish as a burst. A check that
   gets no answer, a `429` included, waits for the next one. The checks **only ask**: they never
   submit anything.
4. **A person decides.** When the 48 hours are up, swisha stops asking. A payout Swish never
   answered becomes `NEEDS_REVIEW`; one at `DEBITED` or `ERROR` keeps Swish's word. Each then
   carries a `review` message saying what to check: see
   [When a person has to decide](#when-a-person-has-to-decide). A last check swisha could not run,
   because the number is not loaded, Swish gave no answer or the database failed, waits until it
   can: the checks end only once Swish has been asked and its answer recorded.

Every question to Swish about a payout uses the certificate of the payer that sent it.

**`PAID` arrives with or without the callback.** Measured against the simulator with no callback
reaching swisha: `DEBITED` about 5 seconds after the payout and `PAID` about 9 seconds after it,
both from swisha's own questions. With a callback route, Swish brings each step itself the moment
it happens, which is still worth having: see
[Reverse proxy](#reverse-proxy-and-the-callback-allowlist).

### swisha never retries a payout

There is no automatic retry. A resubmission needs a fresh `payoutInstructionUUID`, which Swish
cannot tie back to the original, so a payout that was in fact already debited would be debited
again. No guard removes that risk; not resubmitting does. Paying again is a person's decision,
made through [redo](#when-a-person-has-to-decide), never swisha's.

### A reference is spent once it is stored

Sending a reference again returns `409`, whatever state the first payout reached, including
`PENDING`, `ERROR`, `DECLINED` and a status this version does not know. `PENDING` is the important
case: it can mean Swish received the instruction and its answer was lost on the way back, so
resubmitting the same reference could debit a second time. Treat `409` as "already handled, go and
look", never as "try again". A payout that should still go out is
[redone](#when-a-person-has-to-decide) under a new reference.

References are case sensitive: `INV-1` and `inv-1` are two references, and so two payouts. Settle
on one form in your own system. The same reference under two payers is two payouts as well.

### Stopping and restarting

**A restart does not cut a payout off.** On `SIGTERM` or Ctrl-C, swisha:

1. stops accepting new connections;
2. ends every open event stream, so a stop is not held up by subscribers. A subscribed client
   reconnects and reads back what it missed from the status endpoint;
3. lets the requests in flight finish, for up to 60 seconds. A request runs to its end even when
   its caller hangs up, and a stop waits for it all the same;
4. exits with code `0`.

A follow-up still asking Swish at that moment is dropped, but its payout is already stored and the
checks settle it. Swish's guide says it retries a callback it could not deliver once, a minute
later; against the simulator it did not. The checks depend on neither.

Give a stop at least those 60 seconds. systemd waits 90 by default, but Docker waits 10 and
Kubernetes 30: see [Running in a container](#running-in-a-container).

## When a person has to decide

swisha never pays again on its own. When Swish refuses a payout, reports it failed, or never
answers within 48 hours of checks, a person decides what happens next: whether the money reached
the recipient, and whether to pay it again. swisha gives that person what they need through the
status endpoint, and two endpoints to act with. Building them into your admin or invoice system is
optional, and **strongly recommended**: without them, every one of these decisions is made outside
swisha, with nothing to stop two people paying the same invoice twice.

> [!WARNING]
> Check your bank statement, and call your bank, before you redo a payout. A redo is a new
> payout: if the first one did reach the recipient after all, swisha cannot take back the second.

What swisha tells a person, by state:

| Status | `review` says | `actions` |
|---|---|---|
| `DECLINED` | nothing: `error_message` says why Swish refused it, and no money moved | `redo` |
| `DECLINED` over the recipient's details | Swish would decline it again: correct the details and send a new payout | none |
| `ERROR` | nothing at first: `error_message` says why it failed | `redo` |
| `ERROR` after `DEBITED` | the money left and should come back: check the statement before paying again | `redo`, and `confirm_paid` once the checks end |
| `ERROR`, checks ended | Swish still reports it failed: confirm it if the recipient got the money anyway | `confirm_paid`, `redo` |
| `DEBITED`, checks ended | the money left, but Swish never confirmed the recipient received it | `confirm_paid` |
| `NEEDS_REVIEW` | Swish never gave an outcome: check the statement or ask the recipient | `confirm_paid`, `redo` |
| `PAID` or `DEBITED`, redone | Swish paid it after it was redone: the recipient may be paid twice | none |
| `PAID` by a person, then `ERROR` or `DECLINED` from Swish | Swish reported it failed after it was confirmed: check whether the money arrived | none |
| a redo of a payout Swish paid after all | the same, from the redo's side | no `redo` |
| anything else, or redone | nothing | none |

`review` is in English or Swedish, as `SWISH_ERROR_LANG` says, and ready to show as it is.

How to build it in, in your own system:

1. **Show the payout.** Read `/swish/status/{payer}/{reference}`, and follow the `updates` stream
   to keep it current. Show `status`, `error_message` and `review` beside the invoice.
2. **Show a button per action.** One for `confirm_paid` and one for `redo`, only while `actions`
   lists them. swisha refuses anything else with `409` and a reason you can show as it is.
3. **Confirm.** Ask the person for what they checked, and send their name as `by` and that as
   `note` to [confirm](#post-swishpayoutpayerreferenceconfirm).
4. **Redo.** Send a new reference, the person's name as `by` and, optionally, a `note` to
   [redo](#post-swishpayoutpayerreferenceredo). When it answers `202`, point the invoice at the
   new reference: the old payout's `replaced_by` and the new one's `replaces` keep the link.
5. **Decide who may press.** swisha records whatever `by` it is sent, and does not know your
   staff. Which roles may confirm or redo is your system's to enforce.

Keep each invoice pointing at the payout that currently carries it, under its payer and
reference. A redo then needs one update on your side:

```sql
UPDATE invoices SET swish_reference = 'KV-10451-2'
 WHERE swish_payer = 'sthlm' AND swish_reference = 'KV-10451';
```

Every confirmation and redo stays in the payout's history with who did it, why and from where:
see [Finding payouts](#finding-payouts).

## API

### Listeners and authentication

swisha listens twice, deliberately separately. The callback has to be reachable from the internet;
the rest must not be.

| Listener | Default address | Setting | Endpoints |
|---|---|---|---|
| Internal | `127.0.0.1:8083` | `SWISH_SERVER_ADDR`, or `SWISH_SERVER_SOCKET` | payout, confirm, redo, status, events, health, the Swish numbers |
| Callback | `127.0.0.1:8084` | `SWISH_CALLBACK_ADDR` | callback only |

Both speak plain HTTP/1.1. The internal API has two layers of authentication:

- **The API secret**, sent as `x-api-secret` when one is configured, is what lets a caller reach
  swisha at all. See [Security](#security) for when one is required. On its own it adds a number
  and reads health, and nothing else.
- **A number's key**, sent as `x-payer-key`, reaches that number and no other. Every other
  endpoint is about one number and needs both. The key comes from adding the number, once, and it
  also says which number a request means.

Send JSON bodies with `content-type: application/json`.

### Endpoints at a glance

| Method and path | Needs | Does |
|---|---|---|
| [`POST /swish/payout`](#post-swishpayout) | secret, key | Sends a payout |
| [`POST /swish/payout/{payer}/{reference}/confirm`](#post-swishpayoutpayerreferenceconfirm) | secret, key | A person marks a payout paid |
| [`POST /swish/payout/{payer}/{reference}/redo`](#post-swishpayoutpayerreferenceredo) | secret, key | A person pays a failed payout again, under a new reference |
| [`GET /swish/status/{payer}/{reference}`](#get-swishstatuspayerreference) | secret, key | One payout as it stands. `/swish/status/{reference}` works too |
| [`GET /events`](#get-events) | secret, key | One number's live event stream |
| [`GET /system/health`](#get-systemhealth) | secret | The database, Swish, and the numbers as counts |
| [`POST /swish/payers`](#post-swishpayers) | secret | Adds a number, and answers its key once |
| [`GET /swish/payers/{name}`](#get-swishpayersname) | secret, key | One number |
| [`PATCH /swish/payers/{name}`](#patch-swishpayersname) | secret, key | A new nickname |
| [`PUT /swish/payers/{name}/certificates`](#put-swishpayersnamecertificates) | secret, key | New certificates |
| [`POST /swish/payers/{name}/disable` and `/enable`](#post-swishpayersnamedisable-and-enable) | secret, key | Stops a number taking payouts, or lets it again |
| [`POST /swish/payers/{name}/key`](#post-swishpayersnamekey) | secret, key | A new key |
| [`POST /swish/callback`](#post-swishcallback) | Swish only | Swish's report of an outcome, on the callback listener |

"Secret" applies only when `API_SHARED_SECRET` is set.

### Errors

Every error from the internal API has one shape, including those raised before a handler runs: a
body that is not the expected JSON, a query or path that cannot be read, a wrong method or an
unknown path.

```json
{ "error": "Payout already in progress for this reference. Swish ref: 7CBA5994..." }
```

The secret and the key are checked first, from the headers alone, so a caller without them learns
nothing else and swisha never reads its body. Where no secret is set, adding a number, which needs
the secret alone, reads the body of anyone who can connect. A key for one number never says
whether another exists.

| Code | When |
|---|---|
| `400` | The request is malformed, or would be refused by Swish: invalid JSON, a disabled `payer`, a reference or message with characters Swish refuses, an amount out of range, a missing or invalid `payee_ssn`, a blank or overlong `by` or `note`, a certificate swisha could not pay with |
| `401` | Missing or wrong `x-api-secret`, or an `x-payer-key` that is missing or holds no number's current key. The message says which |
| `403` | The key belongs to another number than the one the request names |
| `404` | No payout stored under that payer and reference, or no such path |
| `405` | The path exists, but not for that method |
| `409` | The reference is already spent, so **never resubmit**; or swisha refuses a confirm or a redo, and says why; or a number's name or Swish number is already stored, or it paid under another name; or a key was replaced first by another request |
| `415` | The body was not sent as `application/json` |
| `422` | A field is unknown, missing or of the wrong type, or Swish refused the payout's data. See [the refusal](#post-swishpayout) |
| `429` | More payout requests in 60 seconds from one caller for one payer than `SWISH_PAYOUTS_PER_MINUTE` allows, 30 by default; or an event stream past its cap, 64 open on one number or 10,000 in all |
| `431` | A request's head is over 16 KB |
| `500` | An internal fault, such as a payout that could not be signed. The message is fixed, so no cause leaks to the caller. With `status: PENDING` in the body the reference is spent |
| `502` | Swish refused swisha's own request rather than the payout: the certificate, the Swish number, or too many requests |
| `503` | Swish failed or could not be reached, the database could not, or the number's certificates have expired or could not be loaded. With `status: PENDING` in the body the reference is spent; without one, nothing was stored |

#### What to do with each answer

A reference is **spent** once swisha has a payout stored under it: sending it again only gets
`409`. While it is **free**, sending it again is always safe.

| Answer | Reference | What to do |
|---|---|---|
| `202` | spent | Wait for the outcome on the event stream, or read it from `/swish/status/{payer}/{reference}` |
| `400`, `401`, `403`, `415` or `429`, or `422` without a `status` field | free | Fix the request, or wait out the rate limit, then send the same reference again |
| `409` | spent | Look the payout up. Never send it again: if it failed and should still go out, [redo](#post-swishpayoutpayerreferenceredo) it, or send a corrected new payout when the `409` says the recipient's details were declined |
| `status: DECLINED`, with `422` or `502` | spent | Swish stored nothing and no money moved. If it should still go out, [redo](#post-swishpayoutpayerreferenceredo) it under a new reference. Declined over the recipient's details, `ACMT07`, `ACMT17`, `PA06`, `VR02`, `AM02` or `AM06`, it would be declined again: correct them and send a new payout |
| `status: PENDING`, with `502`, `503` or `500` | spent | Swish may hold the payout, and swisha keeps asking it what became of it. Wait for the outcome on the event stream or the status endpoint. Never send the reference again |
| `503` without a `status` field | free | The database is down, or the number's certificates have expired or could not be loaded. Send the same reference once that is fixed. If the database failed mid-claim and could not be read back either, the `error` says so, and the resend answers `409` should the first have been stored |

### POST /swish/payout

Submits a payout from the number whose key it carries. `payer` may name that number too, and a
payout naming another is refused with `403`:

```bash
curl -X POST http://127.0.0.1:8083/swish/payout \
  -H 'x-api-secret: <your secret>' -H 'x-payer-key: <sthlm key>' \
  -H 'content-type: application/json' \
  -d '{
    "payer": "sthlm",
    "reference": "KV-10442",
    "payee_alias": "0701234567",
    "payee_ssn": "196408233234",
    "amount": 1250.50,
    "message": "Kvitto KV-10442, tack!"
  }'
```

#### The request

| Field | Required | Rules |
|---|---|---|
| `payer` | no | The name of the number to pay from. The key in `x-payer-key` already says which, so it may be left out; when given, it must be the key's number |
| `reference` | yes | Your identifier, sent to Swish as `payerPaymentReference`. 1 to 35 characters of `A-Z`, `a-z`, `0-9` and `-`, the characters Swish takes. With the payer, the idempotency key: the same reference under two payers is two payouts. Case sensitive |
| `payee_alias` | yes | The recipient's Swish number, a Swedish one, in any format such as `070-123 45 67`, `+46 (0)70 123 45 67` or `0046701234567`. Normalized to `46XXXXXXXXX`: `46` and 9 or 10 more digits |
| `payee_ssn` | yes | Swedish personnummer, **12 digits** (`YYYYMMDDNNNN`): a date that exists, and a valid Luhn digit. Separators are ignored; a 10-digit number is refused rather than given a guessed century. Swish verifies every payout against it, and refuses one without it, with an impossible date, or a coordination number |
| `amount` | yes | SEK, as a JSON number, at most two decimals. At least 1, at most `SWISH_MAX_PAYOUT`, which defaults to 150,000: the most Swish moves in one payout. Set it lower to cap payouts below that; swisha refuses to start with a ceiling outside 1 to 150,000 |
| `message` | no | Shown to the recipient in their Swish app. Only `A-Z`, `a-z`, `Å Ä Ö å ä ö`, `0-9`, space and `!?(),.-:;`, the characters Swish takes. Cut to its first 50 characters, Swish's limit, before it is stored, so the record holds exactly what Swish received. Defaults to the `SWISH_PAYOUT_MESSAGE` template |

Rules that catch mistakes rather than passing them on:

- **Unknown fields are refused**, not ignored. A caller sending an outdated shape gets a loud
  error instead of a payout built from the fields that happened to match.
- **Whatever Swish would refuse is refused first, with `400`**: a missing or malformed
  personnummer, one whose date does not exist, a reference or message with a character outside
  Swish's set, an amount with a third decimal, a disabled number. Nothing is stored, so the
  reference is still free to use once it is right. Swish's own refusal of these would have spent
  it, measured against the simulator: a message with `é`, `#` or an emoji comes back as `RP02`,
  and a personnummer with month 13, day 32 or a coordination number as `PA06`.
- **An `amount` sent as a string**, such as `"250.00"`, is a field of the wrong type, refused with
  `422`.

#### The answer

`202 Accepted` once Swish has taken the instruction, and the outcome follows on the event stream:

```json
{ "payer": "sthlm", "status": "CREATED", "success": true, "swish_ref": "ADD0393DEF7B43F2B75C018C8BF5B248" }
```

`swish_ref` is the `payoutInstructionUUID` swisha generated. It is what Swish knows the payout by,
and what you quote when asking them about it.

The same reference from the same number again is refused with `409`, whatever became of the
first:

```json
{ "error": "Payout already in progress for this reference. Swish ref: ADD0393DEF7B43F2B75C018C8BF5B248" }
```

A `payer` other than the key's number is refused with `403`, before anything is stored:

```json
{ "error": "This key belongs to another number. Send the key of the number the request is about." }
```

#### When Swish refuses it

The answer then carries the same fields as the status endpoint, plus the `error` every error has.
A payout the payer's bank declines answers `422`. On the simulator, a `message` of `RF07` makes
Swish refuse it this way, which is a handy way to try the refusal path:

```json
{
  "actions": ["redo"],
  "error": "The payout was declined. Your Swish limit may be exceeded, or the account may have insufficient funds. Check with your bank.",
  "error_category": "user_fixable",
  "error_code": "RF07",
  "error_message": "The payout was declined. Your Swish limit may be exceeded, or the account may have insufficient funds. Check with your bank.",
  "manual_confirmation": null,
  "payer": "sthlm",
  "reference": "KV-10443",
  "replaced_by": null,
  "replaces": null,
  "review": null,
  "status": "DECLINED",
  "swish_ref": "F81A58609982420AB6A6104198DDCCE1"
}
```

**Act on `status`, not on the HTTP code.** `DECLINED` means Swish stored nothing and no money
moved, so if it should still go out, [redo](#post-swishpayoutpayerreferenceredo) it under a new
reference. `PENDING` means Swish may hold the instruction, and swisha asks Swish what became of
it, as it does for every payout without an answer.

A number whose certificate belongs to another Swish number is refused by Swish with `PA01`, and
answers `502`, since it is swisha's own request that is wrong rather than the payout. The body is
the same shape, with `"error_code": "PA01"` and `"status": "DECLINED"`. A refusal Swish answers
with `401` or `403` also reads in health as the number not reaching Swish, until Swish next
answers it. The simulator answers `403` to codes in `message` it does not simulate, such as
`VR02`, which health reads the same way until its next check, 30 seconds on.

#### When Swish cannot be reached

When Swish cannot be reached at all, or a connection to it does not open within 5 seconds, the
answer is `503` with the same fields and `status: PENDING`, and its `error` starts "Swish may have
received this payout, so the reference is spent". A callback can come in before a failed send is
answered, and when it brought `PAID`, `DEBITED` or `ERROR`, Swish holds the payout: the answer is
`202` with that `status`.

A `503` without a `status`, from a database failure or an expired certificate, stored nothing, and
the same reference can be sent again. swisha reads a failed claim back before answering, so a
reference an earlier request holds answers `409` instead, and when even that read fails the
`error` says so.

### POST /swish/payout/{payer}/{reference}/confirm

A person's word that a payout was paid, after the bank or the recipient confirmed the money
arrived. `by` and `note` are both required, so a confirmation always says who made it and on what
grounds:

```bash
curl -X POST http://127.0.0.1:8083/swish/payout/sthlm/KV-10450/confirm \
  -H 'x-api-secret: <your secret>' -H 'x-payer-key: <sthlm key>' \
  -H 'content-type: application/json' \
  -d '{ "by": "Anna", "note": "Bank statement 2026-10-08 shows 100 kr to 0701234567" }'
```

It answers `200` with the payout as it now stands:

```json
{
  "actions": [],
  "error_category": null,
  "error_code": null,
  "error_message": null,
  "manual_confirmation": {
    "at": "2026-10-08T22:11:32+02:00",
    "by": "Anna",
    "note": "Bank statement 2026-10-08 shows 100 kr to 0701234567"
  },
  "payer": "sthlm",
  "reference": "KV-10450",
  "replaced_by": null,
  "replaces": null,
  "review": null,
  "status": "PAID",
  "swish_ref": "AAAABBBBCCCCDDDDEEEEFFFF00000002"
}
```

When a payout can be confirmed:

| Status | Confirm |
|---|---|
| `NEEDS_REVIEW` | any time |
| `DEBITED`, `ERROR` | once swisha has asked Swish one last time, about 48 hours after the payout was made: until then Swish may still give its last word |
| `CREATED`, `PENDING` | no: Swish has not answered yet. If it still has not after 48 hours, the payout becomes `NEEDS_REVIEW` |
| `DECLINED` | no: Swish refused it and no money moved |
| `PAID` | no: already paid |
| any, once redone | no: the redo carries it on |

Every refusal is `409` with the reason, such as `"This payout is already paid."`. `by` holds at
most 100 characters and `note` at most 500, neither blank nor with a control or invisible
character, such as a line separator or a bidi override; leaving either out answers `422`. The
confirmation is kept in the payout's history with the caller's IP.

Should Swish report a confirmed payout `ERROR` or `DECLINED` afterwards, the confirmation stands:
swisha cannot tell which of the two is right. It says so instead, with an `error` line in the
service log naming who confirmed it, and a `review` on the payout to check whether the money
arrived.

### POST /swish/payout/{payer}/{reference}/redo

A person's request to pay a payout again under a new reference, because Swish refused it,
reported it failed, or never answered. The new payout sends the same recipient, personnummer,
amount and message as the one it replaces, and goes out exactly as `POST /swish/payout` sends one.
A corrected payout, to another number for example, is a normal new payout instead, and swisha
cannot check the first one for it: make sure with Swish or your bank that the first failed before
sending it.

```bash
curl -X POST http://127.0.0.1:8083/swish/payout/sthlm/KV-10451/redo \
  -H 'x-api-secret: <your secret>' -H 'x-payer-key: <sthlm key>' \
  -H 'content-type: application/json' \
  -d '{ "reference": "KV-10451-2", "by": "Anna", "note": "Bank confirms nothing left the account" }'
```

It answers like a payout, `202` with `replaces` added:

```json
{ "payer": "sthlm", "replaces": "KV-10451", "status": "CREATED", "success": true, "swish_ref": "2A5345E6C6204C7784B1B090B81E4B74" }
```

**swisha asks Swish first.** Before anything is stored, swisha asks Swish about the payout, and
about every earlier payout it redoes when it is itself a redo. Asking only reads, and whatever
Swish says is recorded as the checks would record it. The redo goes out only when Swish's answer
for each of them is one of these:

- it **failed**;
- it **has no record of it**, and swisha's own history shows that send ended without Swish taking
  it: refused, or never answered.

Any other answer stops the redo, and two of them are what keep a payout from being paid twice on a
stale row:

- **Swish paid it.** A payout whose row reads failed, but that Swish paid, is never paid again.
- **Swish has no record of a payout it took.** swisha's history keeps Swish's acceptance, from its
  `201`, a callback, an answer about the instruction, or `RP09` at the send. A "no record" after
  that is a contradiction, not proof the payout never left. Where the history holds neither the
  acceptance nor how the send ended, Swish's "no record" proves nothing either, and the redo is
  refused.

A redo is allowed on `ERROR`, `DECLINED` and `NEEDS_REVIEW` only, and only once per payout,
however many people press at once: measured with eight simultaneous requests, one was sent and
seven were told it had been redone. A confirmation and a redo of one payout pressed at the same
moment never both land either. A decline over the recipient's own details is not redone: sent
again unchanged, it would be declined again. Everything else is `409`, or `400` for a payout above
today's ceiling, or `503` when Swish cannot be asked:

| Asked to redo | Answer |
|---|---|
| Swish reports it, or a payout before it, `PAID` or `DEBITED` | `409`: `"Swish reports this payout as PAID: its money left your account, so nothing was redone."` |
| Swish holds it without an outcome yet | `409`: Swish may still pay it, so nothing was redone |
| Swish has no record of one it took before, or swisha holds no instruction UUID for it | `409`: check the payout with Swish |
| Swish has no record of it, and swisha none of how its send ended | `409`: check your bank statement, and if it was never paid, send it as a new payout |
| Swish cannot be asked, or gives no usable answer | `503`: nothing was redone. Press again once Swish answers |
| `DECLINED` over the recipient's details: `ACMT07`, `ACMT17`, `PA06`, `VR02`, `AM02` or `AM06` | `409`: correct the details and send a new payout under a new reference |
| an amount above `SWISH_MAX_PAYOUT` as it is set now | `400`: nothing was redone |
| `DEBITED` | `409`: the money left your account, so it cannot be redone. If Swish later reports it failed, it can be redone then |
| `PAID` | `409`: `"This payout was paid, so it cannot be redone."` |
| `CREATED`, `PENDING` | `409`: Swish may still pay it, so it cannot be redone now |
| a payout already redone | `409`: `"This payout was already redone as KV-10451-2. If that one failed too, redo KV-10451-2 instead."` |
| a new reference another payout already holds | `409`: the reference is taken, so redo under one no payout has used |

`reference` follows the rules of every reference and must differ from the one it replaces. `by` is
required and `note` optional, with the limits `confirm` has. The redo is rate-limited like a
payout. The payout replaced is closed from then on: its `actions` are empty, its `review` is
`null`, and its `replaced_by` names the redo.

Should Swish report the payout replaced `PAID` or `DEBITED` after all, however swisha hears it,
the recipient may be paid twice, and swisha says so everywhere it can:

- an `error` line in the service log naming both payouts;
- a `review` on the payout and on every redo after it;
- no `redo` on any of them again.

### GET /swish/status/{payer}/{reference}

The current state of one payout, read with its number's key. `/swish/status/{reference}` answers
too, for the key's own number. The answer is shaped like an `updates` event, so a client that
reconnects can feed it through the same handler; only this endpoint also says who confirmed a
payout and why. A payout Swish never answered, 48 hours on:

```json
{
  "actions": ["confirm_paid", "redo"],
  "error_category": "contact_support",
  "error_code": null,
  "error_message": "The outcome is not known: Swish may have received this payout. Check with Swish before paying it again.",
  "manual_confirmation": null,
  "payer": "sthlm",
  "reference": "KV-10451",
  "replaced_by": null,
  "replaces": null,
  "review": "Swish never gave an outcome for this payout, and swisha has stopped asking. Check your bank statement or ask the recipient: confirm it as paid if the money arrived, and pay it again only if it never left your account.",
  "status": "NEEDS_REVIEW",
  "swish_ref": "AAAABBBBCCCCDDDDEEEEFFFF00000001"
}
```

| Field | Holds |
|---|---|
| `status` | Where the payout is: see [The states](#the-states) |
| `swish_ref` | The `payoutInstructionUUID`, what Swish knows the payout by |
| `error_code`, `error_message`, `error_category` | Why it failed, or `null` |
| `actions` | What swisha would accept now: `confirm_paid`, `redo`, both or neither. Show a button for each |
| `review` | What a person should check before acting, or `null` when nothing needs them |
| `manual_confirmation` | Who confirmed it paid, why and when, or `null` |
| `replaces`, `replaced_by` | The reference this payout redoes, and the one that redid it, or `null` |

The three `error_*` fields are `null` unless the payout has failed, and stand or fall together: a
payout Swish refused with `TA01` and then paid anyway is `PAID` with nothing beside it, never
`PAID` next to "check whether it went through before sending it again". `ERROR` is always Swish's
own word that the payout failed. A `NEEDS_REVIEW` never reads "failed" or "try again", whatever
code its send left: its message says the outcome is not known and Swish may hold the payout,
because a person told either may pay it again.

`error_category` is one of `retryable`, `user_fixable` or `contact_support`. `error_message` and
`review` follow `SWISH_ERROR_LANG`, English or Swedish. A code Swish adds later still resolves to a
usable message rather than nothing. Every code swisha knows is in
[Swish error codes](#swish-error-codes).

Every time in an answer is Swedish local time with its offset, such as `+02:00`.

### GET /events

One number's server-sent event stream, opened with its key. Every query parameter is optional:
`channel`, `payer` and `reference`.

```bash
curl -N --unix-socket /run/swisha/swisha.sock -H 'x-payer-key: <sthlm key>' \
  'http://localhost/events?channel=updates'

# or over a port
curl -N 'http://127.0.0.1:8083/events?channel=updates' \
  -H 'x-api-secret: <your secret>' -H 'x-payer-key: <sthlm key>'
```

Each payer publishes on channels of its own, `swisha:{payer}:updates` and
`swisha:{payer}:events`, and a stream carries its key's number alone, so one city's screen can
never be shown another's payouts. A `payer=` naming another number is refused with `403`.

| Channel | Carries | Sensitive |
|---|---|---|
| `updates` | Status changes: the object `/swish/status/{payer}/{reference}` returns, but for who confirmed a payout and why: its `manual_confirmation` holds `at` alone | no |
| `events` | The audit trail, one message per event: adds `amount`, `payee_alias`, the caller's IP, and who confirmed or redid a payout and why | **yes** |

`events` carries personal data and amounts. It is meant for a back office, not for a
customer-facing page. `updates` is the one to forward to a browser.

The SSE event name is the channel, so a client can switch on it directly:

```text
event: updates
data: {"actions":[],"error_category":null,"error_code":null,"error_message":null,"manual_confirmation":null,"payer":"sthlm","reference":"INV-1001","replaced_by":null,"replaces":null,"review":null,"status":"PAID","swish_ref":"7CBA..."}
```

Every 15 seconds without an event, the stream sends an empty comment line, starting `:`, so a
proxy does not close it. A client ignores those.

Service log lines go to `swisha:logs`, with the payer in their context, and the reference and
amount where a line has them. They cover every number, so no number's key streams them:
`channel=logs` is refused with `403`. An operator reads them on the server, in `swisha_logs` or with
`LISTEN`. Any other channel is refused with `400`, rather than opening a stream that would never
carry anything.

**The stream is lossy by design.** It is fed in process, not from the database, with a channel per
number: an event wakes only that number's streams, and a busy number never makes another's fall
behind.

- A stream that falls more than 128 events behind, some 20 payouts, misses messages rather than
  slowing payouts down.
- Every stream ends when swisha stops.
- A stream ends when its key is replaced: at once on the swisha that replaced it, and within 10
  seconds on the others or after `swisha --new-key`.

Anything that must not be missed is read back from `/swish/status/{payer}/{reference}`, which is
also where a reconnecting client catches up.

Each stream holds a connection, and so a file a payout to Swish might need. One number holds at
most 64 streams open and swisha 10,000 over every number. Past either, a new stream is refused with
`429`, saying which. Open one stream per number in your backend and share it among screens.

**The same names work for PostgreSQL `LISTEN`.** When an answer from Swish changes a payout, its
`events` message and then its `updates` message go out in one statement, so a listener in another
process hears the two together. Any role that can connect to the database can listen and notify,
so take a notification as a reason to read the payout, never as its state.

### GET /system/health

Read with the API secret alone, so it counts numbers and never names one:

```json
{
  "db": true,
  "payers": {
    "disabled": 0,
    "expiring": 0,
    "offline": 0,
    "stored": 1,
    "unusable": 0,
    "waiting": 0
  },
  "payouts_paid_twice": 0,
  "started_at": 1791527843,
  "status": "ok",
  "swish_checked_seconds_ago": 3,
  "swish_next_maintenance": {
    "from": "2026-11-03T02:00:00.000+01:00",
    "name": "Planned maintenance at Swish",
    "until": "2026-11-03T05:00:00.000+01:00"
  },
  "swish_online": true,
  "swish_payout_status": "operational",
  "swish_status_checked_seconds_ago": 3,
  "timestamp": "2026-10-09T08:37:27+02:00",
  "version": "1.0.0"
}
```

| Field | Means |
|---|---|
| `status` | `ok`, or `degraded` for any reason in the list below |
| `db` | Whether the database answered, asked live on this request |
| `payers.stored`, `payers.disabled` | Every number stored, and how many are disabled |
| `payers.unusable` | Numbers that take payouts but could not load their certificates |
| `payers.offline` | Numbers that take payouts but did not reach Swish when last asked |
| `payers.expiring` | Numbers that take payouts with a certificate expiring within 30 days |
| `payers.waiting` | Numbers, disabled ones included, with payouts due a check that this swisha cannot ask Swish about, since their certificates are not loaded here. Those payouts wait until they are |
| `payouts_paid_twice` | Payouts Swish reported `PAID` or `DEBITED` after a person redid them: their recipients may be paid twice. See [redo](#post-swishpayoutpayerreferenceredo) |
| `swish_online` | Whether the numbers reach Swish: see below |
| `swish_checked_seconds_ago` | How long ago the most recently asked number was asked |
| `swish_payout_status`, `swish_next_maintenance` | What Swish's own status page says about payouts |
| `swish_status_checked_seconds_ago` | How long ago that page was read |
| `started_at` | When this swisha started, in seconds since 1970 |
| `timestamp`, `version` | Now, in Swedish time, and this swisha's version |

**`status` is `degraded`** when any of these holds:

- the database is down;
- Swish reports payouts as anything but `operational`;
- a number that takes payouts cannot reach Swish, could not load its certificates, or has one that
  expires within 30 days;
- a number has payouts waiting that this swisha cannot ask about.

A disabled number is counted but never degrades it, and neither does a payout paid twice: swisha
cannot know when one has been dealt with, so alert when the count grows. With the secret, health
answers `200` whatever it finds: degraded is information, and nothing here ever stops a payout.
Which numbers are meant is shown by `swisha --list-payers` on the server, or by each number's own
`GET /swish/payers/{name}` with its key.

**How swisha knows a number reaches Swish.** The database is asked live on every request, because
a health check that reports a dead database as healthy is worse than having none. Reaching Swish
is checked with each number's own certificate and cached, since reaching Swish is slow and it
rate-limits:

- each swisha asks Swish about one number a second, whatever the count: first a number Swish did
  not answer, 30 seconds after it failed, then one never asked, then one whose last answer is over
  10 minutes old;
- any answer to a payout or a check counts as reaching Swish, so a number that pays is never
  asked, except `401` and `403`: with those Swish refuses the certificate or the number, which
  reads as not reaching it;
- with 10,000 numbers and no payouts, each is asked at least every 2.8 hours.

`swish_online` sums that up over every number that takes payouts:

- `false` if any of them did not reach Swish when last asked;
- `true` once one has answered and none has failed;
- `null` until one has answered, or while no number takes payouts.

After an outage, health is `ok` again once each number it marked offline has answered again. Each
is asked again 30 seconds after it last failed, one number a second, so that takes about 30
seconds, or about a second per such number when there are more. An answer to a payout counts at
once.

**What Swish says about itself.** Every 5 minutes swisha reads the `Payout` component on
[status.swish.nu](https://status.swish.nu) and the next maintenance announced for it. Offline here
while Swish reports `operational` points at this side; Swish reporting trouble points at theirs. A
status page that cannot be read leaves these fields `null`, never degraded.

### POST /swish/callback

Where Swish reports the outcome. The only public endpoint, and the only one on the callback
listener. Not for your application to call.

Swish calls it **twice** for a successful payout, once for `DEBITED` and again for `PAID`, and
twice for a late failure, `DEBITED` then `ERROR`. swisha is idempotent about it: a repeat is
recorded, a stale one is ignored, and a payout never moves back. The callback's `payerAlias` says
which payer it belongs to. Only `PAID`, `DEBITED`, `DECLINED`, `ERROR` and `PENDING` change a
payout; any other status is recorded in its history and nothing more.

What it answers, and why:

| Code | When |
|---|---|
| `200` | Handled, and also for anything swisha deliberately ignores: a number swisha does not pay from, an unknown or missing reference, a mismatched instruction UUID or `callbackIdentifier`, a status Swish does not report. Retrying those would never help |
| `403` | The source address is not Swish's, in production only |
| `500` | The database could not be read, or the callback could not be recorded. The one case where swisha **wants** Swish to try again |
| `400`, `415`, `422` | The body is not the JSON Swish sends |
| `413` | The body is over 64 KB, far more than any callback Swish sends. It is refused unread |

Swish reads only the status code. How a callback proves it is Swish's is in
[Reverse proxy and the callback allowlist](#reverse-proxy-and-the-callback-allowlist).

## Swish numbers

Every Swish number swisha pays from is stored in its database, as many as you add. Numbers are
added, given new certificates and disabled through the internal API: from your own admin panel, or
from a service that sets numbers up for its customers. Each one gets its own key when it is added,
and every later request about it needs that key. The env file only says where their certificates
are kept, `SWISH_CERT_DIR`.

| Field | Means |
|---|---|
| `name` | What callers send as `payer`. 1 to 32 of `a-z`, `0-9`, `-` and `_`. **Permanent**: with the reference it is the double-payout key, so a number keeps the name it was added under |
| `nickname` | What people call it, such as the city or the customer. Up to 100 characters, and changeable at any time |
| `number` | The Swish number, 10 or 11 digits. Stored once, disabled numbers included: a callback finds its payer by number |
| `tls`, `signing` | The two certificates, each a certificate followed by its own key, sent as text. See [Certificates](#certificates) |
| `by` | Who does it. Every change is logged with it and the address it came from |

### Each number on its own

Each number is its own account at Swish, and swisha runs each one on its own:

- its own HTTPS client, certificates and signing key;
- its own budget on the payout and redo endpoints;
- its own checks: a worker that asks Swish only about that number's payouts, one question a second
  on each swisha, 3,600 an hour;
- its own reach to Swish, checked with its own certificate, for health.

A backlog of unanswered payouts on one number, a Swish that stops answering one certificate, or a
certificate file that will not load holds up that number and no other. Measured: with forty checks
queued on one number, another number's payout was asked about within a tenth of a second, and with
one number's connection to Swish hanging, another's payout settled at the next 5-second tick.

A change on one swisha reaches every other sharing the database within 10 seconds, with nothing
restarted. A disable and a new key take effect on every instance at once, and a number added on
one instance pays at once on the others. Running on more than one host needs `SWISH_CERT_DIR`
shared at the same path on each: a host that cannot read a number's files holds that number aside
and refuses its payouts.

### Each number's key

Adding a number answers with its key, `swk_` and 64 hex characters, once. Send it exactly as it
was given: the hex is uppercase, and a key changed in any way is refused with `401`. swisha keeps
only its SHA-256, so neither the database nor a backup of it holds a key that works, and a lost key
cannot be shown again, only replaced.

The key reaches its own number and nothing else: its payouts, confirmations and redos, its
statuses and events, and its own settings. The API secret alone reaches no number already stored,
so one leaked key, or one bug in your backend that sends a customer's request with another
customer's number, reaches one number at most. Where the keys live decides the rest:

- **In your backend's secret store**, beside each customer's account. A break-in to the backend
  still reaches every key it holds, but a bug or a single leaked key reaches one number.
- **With each customer**, sent with each request and passed through. Your backend then holds no
  key at all.

A key is replaced through [`POST /swish/payers/{name}/key`](#post-swishpayersnamekey), sent with
the key it replaces, or on the server, where no key is needed: see [On the server](#on-the-server).
The old key stops working at once, on every instance. To lock a customer out, disable the number
on the server and give it a new key there: the customer can no longer enable it again.

### POST /swish/payers

Adds a number, with the API secret alone. Both certificates are checked before anything is kept:
each must be a certificate followed by exactly one PKCS#8 key that belongs to it, must not have
expired, and must not have been issued to another Swish number, which Swish would refuse as `PA01`
on every payout. An older certificate that does both jobs goes in as both:

```bash
jq -n --rawfile tls sthlm-tls.pem --rawfile signing sthlm-signing.pem \
  '{name: "sthlm", nickname: "Stockholm refunds", number: "1231234567",
    tls: $tls, signing: $signing, by: "Anna"}' |
curl -s -X POST http://127.0.0.1:8083/swish/payers \
  -H 'x-api-secret: <your secret>' -H 'content-type: application/json' -d @-
```

It answers `201` with the number as swisha now holds it and its `key`, once swisha has tried the
new certificate against Swish. What Swish said shows as `swish_online`. It never refuses an
upload, so a moment's network trouble cannot block one. The same shape, without the key, answers
every endpoint below:

| Field | Holds |
|---|---|
| `key` | The number's key, in this answer only |
| `name`, `nickname`, `number` | As stored |
| `disabled` | Whether it takes new payouts |
| `usable` | Whether its certificates loaded. When not, `problem` says why |
| `tls_expires`, `signing_expires` | Each certificate's last day, with `_in_days` beside it |
| `swish_online` | Whether Swish answered its certificate at the last try without refusing it, as `401` or `403` would, `null` before the first |
| `added_by`, `created_at` | Who added it, and when |

| Answer | When |
|---|---|
| `400` | A field is wrong, or a certificate fails a check, saying which and why. Nothing is kept |
| `409` | The name or the Swish number is already stored, or the number paid out under another name |

Once the number is stored, the answer is `201` with its key even if its files could not be put in
place: it is then `usable: false`, and `problem` says to upload them again with
`PUT /swish/payers/{name}/certificates`. A caller who gives up before the answer still has the
number added in full, but not its key: `swisha --new-key` on the server gives it a new one.

A number that paid out under one name is only ever added under that name: added under a new one,
every reference it already paid would pay again. The `409` never names another number, since the
secret alone must not reveal one; the server's log does.

Every endpoint below is about one number, and needs its key in `x-payer-key`. Another number's
key is refused with `403`.

### GET /swish/payers/{name}

One number, as above. There is no list: the API secret alone shows no number, and an operator
lists them on the server with `swisha --list-payers`.

### PUT /swish/payers/{name}/certificates

A number's new certificates, once Swish has issued them, with `tls`, `signing` and `by`. The checks
are the ones an add makes, against the number already stored. The new files replace the old in one
step, and every swisha sharing the database uses them within 10 seconds.

### PATCH /swish/payers/{name}

A new `nickname`, with `by`. The name and the number never change, so a body carrying either is
refused with `422`.

### POST /swish/payers/{name}/disable and /enable

`disable` stops a number taking new payouts, with `by` and an optional `note`: from that moment,
on every instance, a payout from it is refused with `400`, nothing stored, and the reference stays
free. Its earlier payouts still settle, since their callbacks and checks need its certificate for
as long as Swish may answer. A number is never deleted. `enable` lets it pay again.

### POST /swish/payers/{name}/key

A new key for the number, with `by` and an optional `note`, answered once as `{"payer", "key"}`.
The key the request came with stops working at once, on every instance. An event stream opened
with it ends at once on this instance, and within 10 seconds on the others. Of two replacements
made with one key, one wins. The other is refused with `409`, or with `401` once the winner has
finished.

```bash
curl -s -X POST http://127.0.0.1:8083/swish/payers/sthlm/key \
  -H 'x-api-secret: <your secret>' -H 'x-payer-key: <sthlm key>' \
  -H 'content-type: application/json' -d '{ "by": "Anna", "note": "Pasted in a support ticket" }'
```

### On the server

What reaches every number lives on the server, never in the API. Each command reads the same env
file as the service, and with it the database password, so it needs the access that already means
full control. Run them as the user swisha runs as, which can read that file and the certificates:

```bash
sudo -u swisha env SWISHA_ENV_FILE=/etc/swisha/active.env swisha --new-key sthlm
```

| Command | Does |
|---|---|
| `swisha --new-key NAME` | Gives the number a new key and prints it, alone, on stdout. The old one stops working at once |
| `swisha --disable NAME` | Stops the number taking new payouts, at once on every instance |
| `swisha --enable NAME` | Lets it pay again |
| `swisha --list-payers` | Every number: its nickname and Swish number, whether it is ready, disabled or cannot pay and why, and when each certificate expires. It reads the files and the database and asks Swish nothing |

Each change is logged with the operator's login, as `Payer sthlm disabled by anna on the server`.
`--list-payers` prints:

```text
gbg (Göteborg), 1239999999: ready
  TLS certificate expires 2026-11-08, in 29 days
  signing certificate expires 2026-11-08, in 29 days
test (MSS test number), 1234679304: disabled
  TLS certificate expires 2027-09-11, in 337 days
  signing certificate expires 2027-09-11, in 337 days
```

With no number stored, it prints `No Swish number is stored.`

## Configuration

### How settings are loaded

Every setting comes from the environment, or from a file named by `SWISHA_ENV_FILE`. The
environment wins, so a unit's `Environment=` or `docker run -e` can override a value without the
file being edited.

```bash
SWISHA_ENV_FILE=/etc/swisha/prod.env swisha
```

`SWISHA_ENV_FILE` is a plain filesystem path, with no search and no default location. **Use an
absolute path anywhere the working directory is not obviously yours**: a service, a container, a
cron entry. A relative path resolves against the process's working directory, which under systemd
is `/` unless the unit sets `WorkingDirectory=`, so a unit file naming `dev.env` looks for
`/dev.env` and fails to start.

Failing to start is the point. A file that is named but cannot be read is an error, never a silent
fallback to something else:

```text
Configuration error: Cannot read env file: /etc/swisha/prod.env: No such file or directory (os error 2)
```

A service told to load its configuration from somewhere should not come up with a different
configuration. Leaving `SWISHA_ENV_FILE` unset is the separate, deliberate case: no file is looked
for and every setting comes from the process environment, which is how you would run it under
Docker or Kubernetes.

### Settings

`examples/swisha.dev.example` and `examples/swisha.prod.example` list every setting, with a comment
on each line:

| Setting | Default | Notes |
|---|---|---|
| `DB_HOST` | `localhost:5432` | `host`, `host:port`, `[IPv6]:port`, or a directory holding PostgreSQL's socket |
| `DB_NAME` | **required** | The database |
| `DB_USER` | **required** | The role swisha connects as |
| `DB_PASS` | blank | Blank for peer authentication. swisha warns at startup when it is blank |
| `DB_SSLMODE` | `require` off this host, `prefer` on it | `disable`, `prefer`, `require`, `verify-ca` or `verify-full` |
| `DB_SSLROOTCERT` | none | A private CA's root certificate, for `verify-ca` or `verify-full`. Must exist at startup |
| `DB_POOL_SIZE` | `10` | Connections to PostgreSQL, from 1 to 1,000. See [Running many numbers](#running-many-numbers) |
| `SWISH_SERVER_SOCKET` | none | A Unix socket for the internal API. Wins over `SWISH_SERVER_ADDR` |
| `SWISH_SERVER_ADDR` | `127.0.0.1:8083` | The internal API's address, when no socket is set. Must differ from `SWISH_CALLBACK_ADDR` |
| `SWISH_CALLBACK_ADDR` | `127.0.0.1:8084` | The callback listener, behind your reverse proxy |
| `TRUSTED_PROXY` | `127.0.0.1` | The proxy in front of the callback, as IP addresses separated by commas. Its `X-Forwarded-For` is believed there and nowhere else |
| `SWISH_ENV` | `test` | `production` or `test`. The prod template sets `production` |
| `SWISH_MAX_PAYOUT` | `150000` | The largest payout, from 1 to 150,000 SEK, with at most two decimals |
| `SWISH_PAYOUTS_PER_MINUTE` | `30` | Each number's payouts and redos a minute, per caller, from 1 to 100,000. See [Rate limit](#rate-limit) |
| `SWISH_CALLBACK_URL` | **required** | Where Swish reports outcomes: an `https://` URL of at most 684 characters with no spaces, even in test |
| `SWISH_CERT_DIR` | **required** | The folder that holds every number's certificates. Must exist (swisha never creates it), be closed to other users and be writable by swisha. See [Certificates](#certificates) |
| `API_SHARED_SECRET` | none | At least 16 characters. **Required** when the internal API listens on neither a socket nor loopback |
| `SWISH_PAYOUT_MESSAGE` | `{reference}` | The message for payouts sent without one, `{reference}` substituted |
| `SWISH_ERROR_LANG` | `en` | The language of error messages and reviews: `sv` for Swedish (`se`, `sv-se`, `swedish` and `svenska` work too). Anything else is English |
| `RUST_LOG` | `info` | `error`, `warn`, `info`, `debug` or `trace`, a bare level. Anything else, a filter such as `swisha=debug` included, means `info` |

`SWISH_ENV` is the one field the templates fill in for you: `test` in the dev template,
`production` in the prod one. Blank would mean `test`, which is the wrong default for a file called
prod.env and would leave the callback allowlist off. `test` talks to the MSS simulator, and
`production` to Swish itself: see [Network](#network).

`SWISH_PAYOUT_MESSAGE` is held to the same characters as a message, apart from `{reference}`, and
checked at startup: a template Swish refuses would spend every reference paid without a message of
its own.

The database connection uses TLS as `DB_SSLMODE` says, in libpq's words. Left blank it is `require`
for a database on another host, since the connection carries personnummer, amounts and the
password, and `prefer` on loopback or a socket, where nothing crosses a network. `verify-full`
checks the server against the public roots sqlx bundles; for a private CA, point `DB_SSLROOTCERT`
at its root certificate. `PGSSLMODE` is not consulted, and no `.pgpass` file is read: the password
is `DB_PASS` alone.

`DB_HOST` takes `host`, `host:port`, an IPv6 address bare or in brackets (`[::1]:5432`), or a
directory holding PostgreSQL's socket. `TRUSTED_PROXY` takes IP addresses separated by commas.
Anything else in either stops startup and is named, rather than becoming port 5432 or a proxy that
is quietly not trusted.

### The env file

The file is `KEY=value` lines:

- a blank field means the default rather than an empty string, which is what lets you copy a
  template, fill in the lines marked `required`, and run;
- a line may start with `export `, and blank lines and lines starting with `#` are skipped;
- in an unquoted value, a `#` after a space starts a comment, which is cut: `RUST_LOG=warn # quiet`
  reads as `warn`. Quote the value to keep one: `DB_PASS="pa #ss"`. A quoted value can carry a
  comment after its closing quote too: `SWISH_ERROR_LANG="sv" # en or sv` reads as `sv`;
- single and double quotes both work, and what is between them is taken as it is, with no escapes;
- a key set twice stops startup, naming the line, and so does a line that is not `KEY=value` or
  whose key is not a variable name.

**A key swisha does not read stops startup, and is named.** A misspelled setting would otherwise
keep its default without a word: a lower ceiling meant for `SWISH_MAX_PAYOUT` would quietly stay at
150,000. Keys left blank are ignored, so an older template still loads. The process environment is
not checked this way, since it carries everything else on the machine.

**Settings that no longer exist are refused by name** rather than ignored, so a file written for an
older swisha stops on the setting it relies on: `SWISH_NUMBER`, `SWISH_NAME`, `SWISH_TLS` and
`SWISH_SIGNING` with their numbered forms, `SWISH_CERT`, `SWISH_KEY`, `SWISH_SIGNING_CERT`,
`SWISH_SIGNING_KEY`, `SWISH_CA`, `SWISH_REQUIRE_SSN`, `TABLE_PAYOUTS`, `TABLE_LOGS`, `TABLE_EVENTS`
and `NOTIFY_PREFIX`. The first four name the way numbers are added now: see
[Upgrading](#upgrading).

**The file must be its owner's alone**, owned by the user swisha runs as and `chmod 600`, since it
holds the database password and the API secret. swisha refuses to start while any other user can
open it, its group included.

### Two environments

Keep one file per environment and select it with a symlink, so every value moves together:

```text
/etc/swisha/dev.env
/etc/swisha/prod.env
/etc/swisha/active.env -> prod.env
```

```ini
Environment=SWISHA_ENV_FILE=/etc/swisha/active.env
```

That is the line `examples/swisha.service.example` already carries. Not `EnvironmentFile=`: that
has systemd load the file into the environment itself, which skips swisha's own checks of the file,
its permissions and its keys included.

```bash
ln -sfn dev.env /etc/swisha/active.env && systemctl restart swisha
```

Each environment has its own database and its own certificate folder, so its numbers move with it:
the test number lives only in the test database, and a production number only in the production
one.

## Security

The payout endpoint moves money, so two things guard it. What guards swisha itself depends on
where it listens, and swisha refuses to start in a configuration that leaves it unguarded. Behind
that, every request about a number needs [that number's key](#each-numbers-key), whatever the
listener.

[SECURITY.md](SECURITY.md) lists the properties swisha guarantees, what the operator has to get
right, and how to report a vulnerability. Please do not report one in a public issue.

| Listener | Guard | Secret |
|---|---|---|
| Unix socket | file permissions, enforced by the kernel | not needed |
| Loopback port | the host boundary | not needed, warns. Needed behind a TLS terminator or proxy |
| Any other address | the shared secret | **required, refuses to boot without one**, and warns: the hop is plain HTTP |

### Unix socket

The recommended deployment. Set `SWISH_SERVER_SOCKET` and swisha creates the socket with mode
`0660`, so access is a group membership rather than a string in a config file.

```console
$ ls -l /run/swisha/swisha.sock
srw-rw---- 1 swisha swisha 0 Aug 27 09:03 /run/swisha/swisha.sock
```

There is nothing inside it. A Unix socket is an address, not a file, and the permission bits are
the authentication: the kernel checks them at `connect()` before swisha sees a byte, standing in
for the secret. The path is not a secret and can be published. swisha binds it in a folder only it
can enter and moves it into place once its mode is set, so whatever the umask, nothing connects in
between. A socket left behind by an unclean stop is replaced; anything else at that path is
refused.

Give the calling service that group and it can connect:

```bash
sudo usermod -aG swisha www-data
```

### Shared secret

For deployments where a socket will not do, such as another host or another container. Set
`API_SHARED_SECRET` and send it as `x-api-secret`. It is compared in constant time, must be at
least 16 characters, and is **not** a Swish credential. On its own it adds numbers and reads
health; it reaches no number already stored. You generate it:

```bash
openssl rand -hex 32
```

The internal API speaks plain HTTP. Across hosts the secret, every number's key and each payout's
personnummer cross the network readable, so swisha warns at startup when it listens on anything
but loopback. Keep that hop on a private network, or have swisha listen on loopback behind a TLS
terminator on its own host, such as nginx or stunnel. Set the secret then too: on loopback swisha
takes requests without one, and the terminator forwards anyone's.

### Certificates

Each Swish number needs **two certificates**, created in the Swish Certificate System: one
authenticates the TLS connection to Swish, the other signs every payout. The certificate system
only accepts **4096-bit** keys, so make each certificate request from one:

```bash
openssl req -new -newkey rsa:4096 -nodes -keyout tls.key -out tls.csr -subj "/CN=1231234567"
openssl req -new -newkey rsa:4096 -nodes -keyout signing.key -out signing.csr -subj "/CN=1231234567"
```

Upload each `.csr` in the certificate system and download the certificate it issues. Then join
each certificate with its key into one file:

```bash
cat tls.pem tls.key > sthlm-tls.pem
cat signing.pem signing.key > sthlm-signing.pem
```

Add both with their number through [`POST /swish/payers`](#post-swishpayers), and hand over new
ones the same way through [`PUT /swish/payers/{name}/certificates`](#put-swishpayersnamecertificates).
One file per certificate means a certificate can never be paired with another one's key.

**What swisha checks before it keeps a file**, and again whenever it loads the number, at startup
and after any change to it:

- the file holds a certificate first, and may carry its chain after it; only the first
  certificate is the number's own;
- it holds exactly one private key, which belongs to that certificate. The key is PKCS#8, RSA, from
  2048 to 4096 bits. A PKCS#1 key is refused with the `openssl pkcs8 -topk8` command that converts
  it, and an encrypted one with the command that decrypts it: swisha needs it unencrypted,
  readable by itself alone;
- the certificate has not expired;
- it was not issued to another Swish number, the mix-up Swish refuses as `PA01` on every payout. A
  certificate whose common name is a Swish number must name this one; an older one whose common
  name is not a number is accepted.

A failed check answers `400` and names the file and the fix.

**Where swisha keeps them.** swisha keeps its own copies in `SWISH_CERT_DIR`, one folder per
number, named for its Swish number and made by swisha when the number is added. Each file is named
for its role, the Swish number and the number's name:

```text
/var/lib/swisha/                          SWISH_CERT_DIR, made by hand, 700
├── 1231234567/                           made by swisha when the number is added, 700
│   ├── tls-1231234567-sthlm.pem          certificate and its key, 600
│   └── signing-1231234567-sthlm.pem      certificate and its key, 600
└── 1239876543/
    ├── tls-1239876543-gbg.pem
    └── signing-1239876543-gbg.pem
```

There are no `.key` files: each `.pem` holds a certificate followed by its own key. swisha picks
every path itself, from the number and the name, and checks the shape of both first, so a row
changed in the database by hand never sends a read outside the folder. The API takes the files'
contents, never a path.

New certificates are written to a folder of their own and swapped in whole, so a number never holds
one old file and one new. A write cut short by a crash leaves that folder behind, named with a
leading dot and as closed as the rest. swisha looks for those every 10 minutes and removes one once
it is an hour old: anything newer may be another instance's write still in progress on a shared
folder. A replacement cut short between its two moves leaves the number's own folder aside, and
that goes back at once: the files the number's row still names.

**Who may open the folder.** The folder must be open to its owner alone, the user swisha runs as,
and not to its group: your application joins swisha's group to reach the socket. The env file is
held to the same rule, `chmod 600` and owned by that user, since it carries the database password
and the API secret.

swisha never creates `SWISH_CERT_DIR`, and neither does the shipped unit. Make it once, by hand,
before the first start:

```bash
sudo install -d -m 700 -o swisha -g swisha /var/lib/swisha
```

A folder that is not there stops swisha at startup, naming the path, so a mistyped path fails at
once, instead of swisha starting on an empty folder where every number's files are missing. The
shipped unit lets swisha write to `/var/lib/swisha` and nowhere else on disk, through
`ReadWritePaths=`. A folder kept elsewhere, such as another environment's, is named on that line
too.

The folder holds every number's private keys, so whoever can read it can pay from every number:

- In production, keep it on the host's own disk, never under `/home` or `/tmp`, which the shipped
  unit hides from swisha, or under a web root, which a web server could serve.
- Keep the modes at `700` for folders and `600` for files, as swisha sets them. swisha refuses to
  start on a folder another user can open, its group included, and holds aside a number whose file
  another user can open.
- A backup of it holds every key too. Encrypt it, keep it as closed as the folder, and restore it
  with its owner and modes.
- Shared between hosts, the share is as closed as the folder: mounted for the service account
  alone, with the same user ID on every host.

**A number whose files fail is held aside**, with the reason: it refuses its own payouts with
`503` before anything is stored, health counts it as unusable and reads `degraded`, and every other
number pays as usual. `GET /swish/payers/{name}` and `swisha --list-payers` give the reason. Fix
the file where it is, its mode for example, and swisha tries the number again as soon as the file
changes, within 10 seconds and with nothing restarted.

**Renewing.** swisha never renews a certificate itself: only the Swish Certificate System issues
them. Order new ones there as above and hand them over through `PUT`. Thirty days before either
certificate expires, health turns `degraded`: that is the time to order. One that expires while
swisha runs stops that number's payouts with `503` before anything is stored, so their references
are still free once the new certificates are in.

Swish's serial for the signing certificate is read from the signing file, never the TLS one.
Swish's servers present public certificates, so no CA setting exists.

### Reverse proxy and the callback allowlist

Swish requires HTTPS and swisha serves plain HTTP, so a reverse proxy is part of the deployment
rather than an option. Point it at the callback **and only the callback**.
`examples/nginx.conf.example` is this block as a file you can copy, with a redirect from plain HTTP
added:

```nginx
server {
    listen 443 ssl;
    server_name payouts.example.com;

    ssl_certificate     /etc/letsencrypt/live/payouts.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/payouts.example.com/privkey.pem;

    location = /swish/callback {
        proxy_pass http://127.0.0.1:8084;
        proxy_set_header Host              $host;
        proxy_set_header X-Forwarded-For   $remote_addr;   # overwrite, never append
        proxy_set_header X-Forwarded-Proto https;
    }

    location / { return 404; }
}
```

Three things in that block are load-bearing:

- **`location =`** is an exact match. A prefix match on `/swish/` would publish the payout endpoint
  to the internet.
- **`X-Forwarded-For $remote_addr`** overwrites whatever the caller sent. The usual snippet is
  `$proxy_add_x_forwarded_for`, which *appends*, and swisha reads the first address in the list. A
  caller who puts a Swish address in that header would be believed, which defeats the allowlist
  below. Measured: with `$proxy_add_x_forwarded_for` a forged callback was accepted and marked a
  payout `PAID`; with `$remote_addr` the same request was refused `403`.
- **`location / { return 404; }`** refuses everything else at the proxy, rather than leaving it
  unrouted.

swisha reads `X-Forwarded-For` alone, never `X-Real-IP`, which the block above leaves as the caller
sent it. On swisha's side, **`TRUSTED_PROXY`** must name the proxy, or the forwarded address is
ignored and every genuine callback is refused instead. It defaults to `127.0.0.1`, which is right
when the proxy is on the same host. It applies to the callback listener alone: the internal API
takes each caller's own address, for its rate limit and its audit trail, whatever it forwards. A
forwarded address that is private, such as `10.0.0.5`, is never believed.

**When `SWISH_ENV=production`, callbacks are accepted only from Swish's eight published
addresses**, listed under [Network](#network). Outside production the address is logged but not
enforced, so the MSS simulator and local tests work without pretending to be Swish.

The allowlist is the outer layer, not the only one. A callback also has to:

- name a number swisha pays from;
- quote the stored `payoutInstructionUUID`;
- echo, in a `callbackIdentifier` header, the value swisha sent with that instruction.

The instruction UUID is no secret, since every answer carries it as `swish_ref`. The
`callbackIdentifier` is: 122 random bits, never in an answer, and to be kept from anyone who does
not run swisha. A forged callback that gets past the address check cannot settle a payout without
guessing it. And a faked failure still cannot lead to a second payout, because a redo asks Swish
first. The address check trusts the proxy's host: anything there can reach the callback listener as
the proxy does and claim any address, and then the `callbackIdentifier` is what still holds.

Every value a callback brings is quoted, escaped and cut to 64 characters in swisha's log, so no
callback can forge a log line or flood one.

### Rate limit

The payout and redo endpoints allow `SWISH_PAYOUTS_PER_MINUTE` requests per caller and number per
60 seconds, 30 by default, and answer `429` beyond that, so one busy city cannot use up the others'
budget. Raise it for bulk payouts: at 30, one number paying 1,000 people takes 34 minutes.

- The limit is per process, so running several instances behind a load balancer multiplies it.
- A caller is the address it connects from, which no forwarded header changes.
- Over a Unix socket every caller shares one bucket per payer, since the kernel has already
  decided who may connect.

### Connections

Each connection holds a file, and a payout that cannot open one to Swish fails with its reference
spent. So neither listener keeps open whatever connects to it:

| Limit | Value |
|---|---|
| Connections the internal API holds at once | 12,000: every event stream swisha allows, and 2,000 beside them. More wait in the kernel's queue until one closes |
| Connections the callback listener holds at once | 1,024 |
| Time to send a request's headers, the first or the next | 30 seconds, then the connection is closed |
| Time to send a body, after the headers | 30 seconds, then the request is refused and the connection closed |
| A request's head | 16 KB, above that `431`. 500 unfinished heads held 13 MB, where they could hold 210 MB before |
| A callback's body | 64 KB, above that refused unread |

Where a secret is set, a body is read only once the secret and the key have passed. In production a
callback from outside Swish's addresses is refused before its body is read.

The caps assume the shipped unit's `LimitNOFILE=65536`. Under a lower limit, fewer idle
connections than the cap can still take every file: before these limits, 450 that never sent a
byte made a payout fail to reach Swish under a limit of 400.

## Database

### PostgreSQL 18 only

swisha supports PostgreSQL 18, and only PostgreSQL. A payout service needs concurrent writers, and
a single-writer engine would cap a host at one instance. Every guard against paying twice is a
PostgreSQL statement, and the whole test suite runs against PostgreSQL 18. swisha's storage code
does sit behind a Rust trait, `PayoutStore`, with its rules written down as executable checks in
`store::conformance`, but that is how the code is organized, not an invitation to swap the
database: no other engine is supported.

### The tables

One swisha pays from every stored number using one set of four tables:

| Table | Holds |
|---|---|
| `swisha_payouts` | One row per payout, under its payer and reference: the recipient, amount, message, status, and the instruction UUID |
| `swisha_events` | Each payout's history, one row per event: see [History events](#history-events) |
| `swisha_payers` | The Swish numbers, with the hash of each one's key |
| `swisha_logs` | The service log: changes to numbers, confirmations, redos, warnings and errors |

`swisha --print-schema` prints them, with comments. Two separate deployments, such as staging and
production on one server, use two databases.

Every connection swisha opens runs with a 10-second statement timeout, and a request waits at most
5 seconds for a free connection from the pool.

### What the payouts table protects

The payouts table is what keeps a reference from being paid twice. Lose it, restore a backup older
than a payout, or fail over to a replica that had not yet received one, and swisha no longer knows
it sent that payout: the same reference sent again reaches Swish as a new payout, under a new
`payoutInstructionUUID`.

- **After a restore**, check the bank statement for what was paid since the backup before sending
  any of those references again.
- **Back up so that a restore loses as little as possible**: continuous archiving with
  point-in-time recovery rather than a nightly dump alone.
- **Each Swish number pays from one database.** Two databases paying from the same number cannot
  see each other's references.

### What swisha checks at startup

At startup swisha checks that every column it uses is there, and every unique rule its guards rely
on: one payout per reference, one redo per payout, and one name, number and key per Swish number.
`CREATE TABLE IF NOT EXISTS` leaves a table from another version as it is, so a mismatch stops
startup and names the table, rather than failing every payout on a missing column.

### Server settings

Two server settings cut the WAL swisha writes, measured at 30 payouts a second, and neither touches
durability:

- `wal_compression = lz4` compresses the full-page images that follow each checkpoint: 23% less WAL
  per payout, 33% with `zstd`.
- `checkpoint_timeout = 30min`, with `max_wal_size = 4GB` to hold it, writes those images less
  often: 17% less beside the default 5 minutes.

Both belong to the server, in `postgresql.conf`; the end of `examples/setup.sql.example` lists them.

### What you may prune

swisha never deletes a row:

- **`swisha_payouts`: keep every row.** It is the double-payout guard.
- **`swisha_events`: keep it.** Each payout's status, its error, who confirmed it and whether Swish
  ever held it are read from its history.
- **`swisha_logs`: yours to trim.** swisha writes the service log and never reads it back.

### Finding payouts

Every payout is stored under its payer's name and its reference, and that pair finds it, however
many numbers share the tables. The name is the one the number was added under, which never changes:
see [Swish numbers](#swish-numbers).

Through the API, with the number's key:

```bash
curl -s http://127.0.0.1:8083/swish/status/sthlm/KV-10442 \
  -H 'x-api-secret: <your secret>' -H 'x-payer-key: <sthlm key>'
curl -N 'http://127.0.0.1:8083/events?channel=updates' \
  -H 'x-api-secret: <your secret>' -H 'x-payer-key: <sthlm key>'
```

In the database, with `psql`. Each query below was run against a live simulator database:

```sql
SET timezone = 'Europe/Stockholm';  -- show times as Swedish time

-- One payout
SELECT payer, reference, status, amount, swish_ref, created_at
  FROM swisha_payouts WHERE payer = 'sthlm' AND reference = 'KV-10442';

-- Its history, oldest first
SELECT event, status, error_code, created_at
  FROM swisha_events WHERE payer = 'sthlm' AND reference = 'KV-10442' ORDER BY id;

-- One city's payouts since a moment
SELECT reference, status, amount FROM swisha_payouts
 WHERE payer = 'goteborg' AND created_at >= '2026-10-08 00:00 Europe/Stockholm'
 ORDER BY created_at;

-- Paid out per city: the money has left at DEBITED
SELECT payer, count(*) AS payouts, sum(amount) AS sek FROM swisha_payouts
 WHERE status IN ('DEBITED', 'PAID') GROUP BY payer ORDER BY payer;

-- Payouts waiting for a person: failed, never answered, or left at DEBITED by the checks
SELECT p.payer, p.reference, p.status, p.updated_at FROM swisha_payouts p
 WHERE (p.status IN ('ERROR', 'DECLINED', 'NEEDS_REVIEW') OR (p.status = 'DEBITED' AND p.checks_ended))
   AND NOT EXISTS (SELECT 1 FROM swisha_payouts r WHERE r.payer = p.payer AND r.replaces = p.reference)
 ORDER BY p.updated_at;

-- Payouts Swish paid after they were redone: the recipient may be paid twice. Read from the redos,
-- as health counts them: 7 ms at a million payouts, where starting from the paid ones took 150
SELECT p.payer, p.reference, p.status, r.reference AS redo, r.status AS redo_status
  FROM swisha_payouts r JOIN swisha_payouts p ON p.payer = r.payer AND p.reference = r.replaces
 WHERE r.replaces IS NOT NULL AND p.status IN ('PAID', 'DEBITED');

-- What people did, and why
SELECT payer, reference, event, actor, note, created_at FROM swisha_events
 WHERE actor IS NOT NULL ORDER BY id;

-- Redone payouts and their redos
SELECT payer, replaces AS redone, reference AS redo, status FROM swisha_payouts
 WHERE replaces IS NOT NULL;

-- The numbers, who added them, and which are disabled
SELECT name, nickname, number, added_by, created_at, disabled_at FROM swisha_payers
 ORDER BY name;

-- Every change to a number, and who made it: written with the change, so neither stands alone
SELECT message, context::jsonb->>'by' AS by, ip, timestamp FROM swisha_logs
 WHERE message LIKE 'Payer %' ORDER BY id;
```

The history of a payout on the simulator, with its callbacks reaching swisha through a tunnel: the
send, Swish taking it, then Swish's two callbacks, `DEBITED` and then `PAID`. swisha asked Swish too
while it waited, and stopped once the callbacks had told it, so nothing is recorded twice.

```text
   event   | status  | error_code |          created_at
-----------+---------+------------+-------------------------------
 INITIATED |         |            | 2026-10-10 17:21:04.121677+02
 ACCEPTED  |         |            | 2026-10-10 17:21:04.179293+02
 CALLBACK  | DEBITED |            | 2026-10-10 17:21:08.506544+02
 CALLBACK  | PAID    |            | 2026-10-10 17:21:12.563637+02
```

A busy database does not cost a history its first rows. A payout's `INITIATED` is stored with its
claim, so nothing is sent without it. A row written on its own, such as `ACCEPTED`, is tried again
when its write fails, as one that waited too long for a connection does: for up to 30 seconds, and
before the payout is answered. Only a stop between Swish's answer and the write can lose one, and a
[redo](#post-swishpayoutpayerreferenceredo) never relies on it.

Two payouts a person settled, one confirmed and one redone:

```text
 reference |     event      | actor |                         note
-----------+----------------+-------+------------------------------------------------------
 KV-10450  | CONFIRMED_PAID | Anna  | Bank statement 2026-10-08 shows 100 kr to 0701234567
 KV-10451  | REDONE         | Anna  | Bank confirms nothing left the account
```

The service log keeps the payer inside each line's JSON context rather than in a column of its
own:

```sql
SELECT level, message, timestamp FROM swisha_logs
 WHERE context::jsonb->>'payer' = 'sthlm' ORDER BY id;
```

**A table per city.** To give a city what feels like tables of its own, make a view per city. A
view can be granted to that city's staff alone, who then cannot read the shared table behind it. It
names its columns, leaving out the recipient's personnummer and the `callbackIdentifier`, which is
what proves a callback is Swish's:

```sql
CREATE VIEW goteborg_payouts AS
SELECT payer, reference, payer_alias, payee_alias, amount, message, swish_ref, status, replaces,
       checks_ended, created_at, updated_at
  FROM swisha_payouts WHERE payer = 'goteborg';
GRANT CONNECT ON DATABASE swisha TO goteborg_staff;
GRANT USAGE ON SCHEMA public TO goteborg_staff;
GRANT SELECT ON goteborg_payouts TO goteborg_staff;
```

Connecting reaches further than the view. Any role that can connect to swisha's database can
`LISTEN` on every number's channels and on the service log, which carry recipients, amounts and
addresses, and `NOTIFY` on any channel. So serve each city through your own backend where you can.
swisha never listens, so a forged notification moves no money, but a backend that listens should
take one as a reason to read `/swish/status/{payer}/{reference}`, never as the payout's state.

Keep your own side of each payout, such as the receipt and what it paid for, in your own tables
under the same payer and reference. That pair is what ties your record to the Swish payout.

## Running it

### Going live

A checklist for production, in order, for a Linux host with systemd and nginx. Each step points to
the section with the detail.

1. **Certificates.** For each Swish number, order a TLS and a signing certificate in the Swish
   Certificate System, made from 4096-bit keys, and join each with its key into one file. See
   [Certificates](#certificates).

2. **The service account, the binary and the certificate folder.** The folder is where swisha
   keeps each number's certificates, owned by this account and closed to everyone else. Make it by
   hand: neither swisha nor the unit does. See [Certificates](#certificates).

   ```bash
   sudo useradd --system --no-create-home --shell /usr/sbin/nologin swisha
   sudo install -m 0755 target/release/swisha /usr/local/bin/swisha
   sudo install -d -m 700 -o swisha -g swisha /var/lib/swisha
   ```

3. **The database.** Change the password in `examples/setup.sql.example`, then create the role,
   the database and the tables. The file also closes the database to every other role on the
   server, which could otherwise listen to its payout events. For a database on another host, set
   `DB_SSLMODE=verify-full`.

   ```bash
   sudo -u postgres psql < examples/setup.sql.example
   swisha --print-schema | psql -h localhost -U swisha -d swisha
   ```

4. **The env file.** Fill in the database fields, `SWISH_CALLBACK_URL` with your public callback
   address, `SWISH_SERVER_SOCKET=/run/swisha/swisha.sock` and `SWISH_CERT_DIR=/var/lib/swisha`.
   `SWISH_ENV` is already `production`. See [Configuration](#configuration).

   ```bash
   sudo mkdir -p /etc/swisha
   sudo cp examples/swisha.prod.example /etc/swisha/prod.env
   sudoedit /etc/swisha/prod.env
   sudo chown swisha: /etc/swisha/prod.env && sudo chmod 600 /etc/swisha/prod.env
   sudo ln -sfn prod.env /etc/swisha/active.env
   ```

5. **The callback route.** Publish `/swish/callback`, and nothing else, over HTTPS. Set your domain
   and certificate paths in the file first. Without this route swisha still learns each outcome by
   asking, seconds later; with it, Swish brings each step the moment it happens. See
   [Reverse proxy](#reverse-proxy-and-the-callback-allowlist).

   ```bash
   sudo cp examples/nginx.conf.example /etc/nginx/sites-available/swisha
   sudo ln -s /etc/nginx/sites-available/swisha /etc/nginx/sites-enabled/
   sudo nginx -t && sudo systemctl reload nginx
   ```

6. **The service.** The log should name the env file it loaded and list the callback allowlist, 8
   entries: see [Command line](#command-line).

   ```bash
   sudo cp examples/swisha.service.example /etc/systemd/system/swisha.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now swisha
   sudo journalctl -u swisha -n 20
   ```

7. **Your application's access.** The socket is open to the `swisha` group. Add the user your
   application runs as, then restart the application so the group applies:

   ```bash
   sudo usermod -aG swisha www-data
   ```

8. **The numbers.** Add each one with its two certificate files, from your admin panel or as below.
   Each answer should show `"usable": true` and `"swish_online": true`, and carries the number's
   `key`: store it where your backend keeps secrets, since it is never shown again.
   `"swish_online": false` means Swish could not be reached, or refused the certificate or the
   number, so no payout from it would go through. See [Swish numbers](#swish-numbers).

   ```bash
   sudo jq -n --rawfile tls sthlm-tls.pem --rawfile signing sthlm-signing.pem \
     '{name: "sthlm", nickname: "Stockholm", number: "1231234567", tls: $tls, signing: $signing, by: "Anna"}' |
   sudo curl -s --unix-socket /run/swisha/swisha.sock -X POST http://localhost/swish/payers \
     -H 'content-type: application/json' -d @-
   ```

9. **Check it.** Look for `"status": "ok"` and `"swish_online": true` in health, and every number
   `ready` with more than 30 days left on each certificate in the server's list. Then make one small
   payout to a number you control, and watch it reach `PAID`.

   ```bash
   sudo curl -s --unix-socket /run/swisha/swisha.sock http://localhost/system/health
   sudo -u swisha env SWISHA_ENV_FILE=/etc/swisha/active.env swisha --list-payers
   ```

10. **Watch it.** Set up the alerts in [Monitoring](#monitoring).

### Running in a container

swisha ships no image. This `Dockerfile`, saved in the repository's root, builds one:

```dockerfile
FROM rust:1.99-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/swisha /usr/local/bin/swisha
ENTRYPOINT ["/usr/local/bin/swisha"]
```

```bash
docker build -t your-swisha-image .
```

A `.dockerignore` holding the line `target` keeps a local build folder out of what Docker reads.

The image needs the system's CA certificates, `ca-certificates` above: swisha checks Swish's
servers against them, and without them no number loads and each says
`Failed to read the system's trusted certificates`.

What changes in a container:

- **Settings.** Mount the env file read-only and point `SWISHA_ENV_FILE` at it, so swisha still
  checks its permissions and its keys. Do not hand the shipped templates to `docker --env-file`:
  Docker takes each value as written, so a line's trailing `# comment` becomes part of the value.
  Settings passed as plain environment variables work too, without those checks.
- **Listening.** Inside a container, `127.0.0.1` is the container itself. To reach the internal API
  from another container, listen on `0.0.0.0:8083`: that is not loopback, so `API_SHARED_SECRET` is
  required, and the hop is plain HTTP, so keep it on a private network. A Unix socket on a shared
  volume needs no secret. Listen on `0.0.0.0:8084` for the callback too, and set `TRUSTED_PROXY` to
  the proxy's address as swisha sees it: for nginx on the host and the `-p` line below, the Docker
  bridge's gateway, `172.17.0.1` by default.
- **The certificate folder.** A volume mounted at `SWISH_CERT_DIR`, owned by the user the container
  runs as, mode `700`. Run the container as that user, such as
  `--user "$(id -u swisha):$(id -g swisha)"`. A bind mount of a path the Docker daemon cannot see
  is made as an empty directory instead, which swisha then reports as open to other users, mode
  `0755`: on a Mac, keep both files under a folder Docker shares, such as your home folder.
- **Open files and memory.** `--ulimit nofile=65536:65536` and `-e MALLOC_ARENA_MAX=2`, as the
  shipped unit sets: see [Running many numbers](#running-many-numbers).
- **Stopping.** swisha lets requests in flight finish for up to 60 seconds, but `docker stop` kills
  a container after 10 and Kubernetes after 30. Give it more: `--stop-timeout 75`,
  `stop_grace_period: 75s` in Compose, or `terminationGracePeriodSeconds: 75` in Kubernetes.
- **The database** stays outside: `examples/docker-compose.yml.example` is a development database
  only.

```bash
docker run -d --name swisha \
  --user "$(id -u swisha):$(id -g swisha)" \
  -e SWISHA_ENV_FILE=/etc/swisha/prod.env \
  -v /etc/swisha/prod.env:/etc/swisha/prod.env:ro \
  -v /var/lib/swisha:/var/lib/swisha \
  -e MALLOC_ARENA_MAX=2 --ulimit nofile=65536:65536 --stop-timeout 75 \
  -p 127.0.0.1:8084:8084 \
  your-swisha-image
```

### Running many numbers

One swisha pays from every number in its database, in one process: nothing starts per number, and
the 10,000th is added as the second was. Measured on Linux with 10,000 numbers, each with Swish's
4096-bit keys, and 10 million payouts and 31 million events stored. swisha, with the shipped unit's
`MALLOC_ARENA_MAX=2`, shared 6 cores with PostgreSQL 18 and a stand-in for Swish, and a round trip
to the database took 0.55 ms:

| What | Measured |
|---|---|
| Start, with 10,000 numbers to load | 1.2 seconds |
| Memory, idle | about 156 MB |
| Memory, 1,000 numbers paying 425 payouts a second | about 280 MB |
| Memory, 30 minutes at 444 payouts a second | 277 MB after 2 minutes, 293 MB at 30 |
| Memory, 5,000 event streams open | about 385 MB |
| Probes of Swish | 1 a second per swisha, whatever the count |
| Adding a number | 32 ms |
| Payouts, 1,000 numbers at 300 a second | p50 287 ms and p99 507 ms, against a Swish answering in 50 to 500 ms |
| Payouts, as many as one swisha takes | about 400 a second, and 540 with `DB_POOL_SIZE=32` |

What to give each swisha:

- **Open files.** Each number paying keeps a connection to Swish, and each event stream holds one:
  1,859 open files with 1,000 numbers paying, and 7,077 with 5,000 streams open. Under systemd's
  default of 1,024, 1,000 numbers paying at once ran out, and payouts failed. The shipped unit sets
  `LimitNOFILE=65536`; under Docker, pass `--ulimit nofile=65536:65536`.
- **Database connections.** Each payout runs 13 statements when Swish's callbacks bring its
  outcome, and 11 when swisha learns it by asking; 4 of them are `NOTIFY`. Each statement holds a
  connection for at least its round trip to the database, so the pool sets the pace rather than the
  cores: here a payout held one for about 25 ms in all, and `DB_POOL_SIZE`, 10 by default, carried
  about 400 payouts a second while swisha used about two of the six cores. 32 carried about 540. A
  longer round trip holds each connection longer, so raise the pool with it. Every swisha's pool
  counts against PostgreSQL's `max_connections`, 100 by default.
- **Memory.** glibc keeps what a thread frees for that thread. The shipped unit sets
  `MALLOC_ARENA_MAX=2`, which cut the growth under load by a third for under 1% more CPU; under
  Docker, pass `-e MALLOC_ARENA_MAX=2`.
- **A caching resolver.** swisha looks Swish's address up through the system. Behind
  `systemd-resolved`, payouts went on with its upstream DNS unreachable, for as long as Swish's
  record lives, 15 minutes; with no local cache, each new connection to Swish failed after 5
  seconds.
- **The payout rate.** `SWISH_PAYOUTS_PER_MINUTE`, per number and caller on each swisha. See
  [Rate limit](#rate-limit).

Signing takes CPU, so swisha signs at most twice as many payouts at once as the host has cores; a
burst waits its turn rather than taking a thread each.

Several swisha over one database share the work. With three, 10,000 references each sent five times
at once were paid once each, a change on one took effect on the others at once, and one restarted
under load finished its requests in half a second and served again 2.6 seconds later. Follow-ups
are paced per swisha, so three of them with a backlog ask Swish about one number up to three times a
second.

### Monitoring

What to watch, once swisha runs:

- **Health.** Poll `/system/health` and alert on `"status": "degraded"`. Its `payers` counts say
  which kind of trouble, and `swisha --list-payers` which number.
- **Payouts paid twice.** Alert whenever `payouts_paid_twice` grows. Health never turns `degraded`
  for it, since swisha cannot know when one has been dealt with.
- **Error lines.** Both of these go to the journal and to `swisha_logs` at `error` level, and each
  needs a person:
  - `Payout sthlm/KV-10451 was redone as KV-10451-2, and Swish now reports it PAID: the recipient may be paid twice`
  - `Payout sthlm/KV-10450 was confirmed paid by Anna, and Swish now reports it ERROR: the recipient may not have the money`
- **Payouts waiting for a person.** The query of that name in
  [Finding payouts](#finding-payouts), or each payout's `actions` and `review`.
- **Certificates.** `payers.expiring` turns health `degraded` 30 days before a certificate expires.
  Order the new one then.

```sql
SELECT level, message, timestamp FROM swisha_logs WHERE level = 'error' ORDER BY id DESC LIMIT 20;
```

### Troubleshooting

**swisha does not start.** It prints the reason and exits with code `1`:

| It says | Why | Fix |
|---|---|---|
| `Configuration error: Cannot read env file: …: No such file or directory` | `SWISHA_ENV_FILE` names a file that is not there, often a relative path under systemd | Use the file's absolute path |
| `… is open to other users (mode 0644), and it holds a key or a password` | The env file is readable by others | `chmod 600` it, owned by the user swisha runs as |
| `… in … is not a setting swisha reads` | A misspelled or unknown key in the env file | Fix the spelling against the template |
| `Missing required setting …`, or `SWISH_CALLBACK_URL is required` | `DB_NAME`, `DB_USER`, `SWISH_CERT_DIR` or `SWISH_CALLBACK_URL` is blank | Fill it in |
| `… is set more than once` | A key appears twice in the env file | Keep one |
| `Cannot open SWISH_CERT_DIR: …` | The certificate folder is not there | Make it: `install -d -m 700 -o swisha -g swisha /var/lib/swisha` |
| `… is open to other users (mode 0755), and it holds private keys` | The certificate folder is open to others | `chmod 700` it |
| `swisha cannot write to SWISH_CERT_DIR …` | Another user owns it, or the unit's `ReadWritePaths=` leaves it out | Give it to the user swisha runs as; name it in `ReadWritePaths=` |
| `SWISH_CALLBACK_URL must be an https:// URL …` | The callback URL is not HTTPS, has a space, or is too long | Use the `https://` address your proxy or tunnel serves, ending in `/swish/callback` |
| `API_SHARED_SECRET is required when the payout endpoint listens on …` | The internal API listens on a non-loopback address without a secret | Set a secret, listen on loopback, or use `SWISH_SERVER_SOCKET` |
| `API_SHARED_SECRET must be at least 16 characters` | The secret is too short | `openssl rand -hex 32` |
| `SWISH_SERVER_ADDR and SWISH_CALLBACK_ADDR must differ` | Both listeners on one address | Give each its own port |
| `Storage error: Cannot connect to PostgreSQL: …` | Wrong `DB_*` settings, the server is down, or TLS is required and not offered | Check `DB_HOST`, `DB_USER`, `DB_PASS` and `DB_SSLMODE` |
| `Storage error: The table … does not match the schema this swisha uses` | The tables are from another version | Run the schema again: see [Upgrading](#upgrading) |
| `Socket directory does not exist: …` | The folder `SWISH_SERVER_SOCKET` names is missing | Under the shipped unit, keep the socket in `/run/swisha`, which the unit makes |
| `Failed to bind …: path must be shorter than SUN_LEN` | The socket's path is longer than the system allows, about 100 characters | Use a short path, such as `/run/swisha/swisha.sock` |
| `Failed to bind to …: Address already in use` | Another process has the port | Stop it, or change `SWISH_SERVER_ADDR` or `SWISH_CALLBACK_ADDR` |

**A number cannot pay.** `swisha --list-payers` shows `cannot pay:` and the reason, and its
`GET /swish/payers/{name}` shows `usable: false` and `problem`:

| Reason | Fix |
|---|---|
| A file is open to other users | `chmod 600` the file. swisha tries the number again within 10 seconds, with no restart |
| The certificate expired | Order new ones, and upload them with `PUT /swish/payers/{name}/certificates` |
| `the certificate belongs to Swish number …` | The file holds another number's certificate: upload the right one |
| The files could not be put in place when it was added | Upload them again with `PUT /swish/payers/{name}/certificates` |
| `Failed to read the system's trusted certificates` | The host or image has no CA certificates: install `ca-certificates`, then restart swisha |

**Requests are refused.**

| Code | It says | Why | Fix |
|---|---|---|---|
| `401` | `Unauthorized: x-payer-key must hold the current key of the number this request is about.` | The key is missing, changed, or was replaced | Send the key exactly as given; replace a lost one with `swisha --new-key` |
| `401` | `Unauthorized.` | `x-api-secret` is missing or wrong | Send the secret from the env file |
| `403` | `This key belongs to another number.` | The key and the `payer` or path disagree | Send that number's own key |
| `400` | `payer … is disabled and takes no new payouts.` | The number is disabled | Enable it, if it should pay |
| `503` | `The certificates of payer '…' could not be loaded`, or `… expired on …` | The number's files are held aside | See "A number cannot pay" above |
| `429` | `Too many requests.` | The rate limit | Wait a minute, or raise `SWISH_PAYOUTS_PER_MINUTE` |
| `502` | `PA01` in `error_code` | Swish refused the certificate for this number | Check the number's certificates were issued to it |

**Callbacks never arrive.** swisha still settles every payout by asking, but:

- in production, a log line `rejected request from unknown IP: …` naming your proxy's address
  means `TRUSTED_PROXY` does not name the proxy, or the proxy sets no `X-Forwarded-For`: see
  [Reverse proxy](#reverse-proxy-and-the-callback-allowlist);
- a local cloudflared tunnel answering `404` is a `~/.cloudflared/config.yml` getting in the way:
  start the tunnel with `--config` pointing at an empty file.

**Payouts fail under load with files running out.** The open-file limit is too low: see
[Running many numbers](#running-many-numbers).

### Upgrading

Run the schema again:

```bash
swisha --print-schema | psql -h localhost -U swisha -d swisha
```

It adds the columns an older swisha's tables lack and keeps every row, and `psql` reports a
`NOTICE` for each table already there and each old index already gone. Each step looks before it
changes anything, so a run on an up-to-date database takes no lock and payouts go on. A step that
waits more than 5 seconds for its lock gives up with `lock timeout`, and running the schema again
finishes it. Only the run that adds `checks_ended` writes to rows: it marks the payouts whose checks
an older swisha had finished. Any later run leaves every row alone.

#### From 0.4

Run the schema again. On a database 0.4.0 made, it swaps the checks' index for one on
`(payer, checked_at)`: 1.1 seconds at 10 million payouts, during which payouts cannot be written.
Copy `examples/swisha.service.example` again too: it now sets `MALLOC_ARENA_MAX=2`. Used as a
library, four things change:

- `PayoutStore` gains `claim_opening`, with a default implementation;
- `events::record` answers a `Recorded`, to announce or to send on alone;
- `swish::submit::submit_payout` takes what `sign_payout` signed;
- an event stream's `Subscription` carries `Arc<StreamEvent>`.

#### From before 0.4

The first run of the schema on a database from before 0.4 builds two indexes on `swisha_payouts`:
about 12 seconds at 10 million payouts, during which payouts cannot be written. It also swaps the
one-redo rule's index for one that holds redos alone: 0.3 seconds at 10 million, built before the
old one goes, so the rule holds throughout. Every run also drops the `attempts` column and an index
on events, which nothing reads. Older versions still write that column, so stop every copy of
swisha first.

Two things are left that the schema cannot do for you:

- **The env file is its owner's alone:** owned by the user swisha runs as, `chmod 600`. swisha
  refuses one its group can read, since your application joins that group to reach the socket.
- **The database is closed to other roles.** As a superuser, once:

  ```sql
  REVOKE CONNECT, TEMPORARY ON DATABASE swisha FROM PUBLIC;
  GRANT CONNECT, TEMPORARY ON DATABASE swisha TO swisha;
  ```

#### From 0.3

The numbers move from the env file into the database, under the names they paid as:

1. Run the schema again, as above. It adds `swisha_payers`.
2. Make the certificate folder by hand, owned by the user swisha runs as and closed to others, and
   set `SWISH_CERT_DIR` to it. Under the shipped unit that is `/var/lib/swisha`:
   `sudo install -d -m 700 -o swisha -g swisha /var/lib/swisha`. Install this version's
   `examples/swisha.service.example` too: it is what lets swisha write there.
3. Remove the payer blocks from the env file. swisha refuses to start while one is set, and names
   it.
4. Start swisha, and add each number under its old `SWISH_NAME`, from the same two files. As root,
   since those files are readable by swisha alone. Each line it prints is a name and its key, or the
   reason it was refused:

   ```bash
   while read -r name number; do
     jq -n --arg name "$name" --arg number "$number" \
       --rawfile tls "/etc/swisha/certs/$name-tls.pem" \
       --rawfile signing "/etc/swisha/certs/$name-signing.pem" \
       '{name: $name, nickname: $name, number: $number, tls: $tls, signing: $signing, by: "upgrade"}' |
     curl -s --unix-socket /run/swisha/swisha.sock -X POST http://localhost/swish/payers \
       -H 'content-type: application/json' -d @- |
     jq -r --arg name "$name" '"\($name) \(.key // .error)"'
   done <<'BLOCKS'
   sthlm 1231234567
   goteborg 1239876543
   BLOCKS
   ```

5. Give your backend each number's key, and send it as `x-payer-key` on every request about that
   number. **swisha refuses every such request without it**, payouts and status reads included.

A number added under any name but the one it paid as is refused with `409`, so no past reference
can pay again under a new name; the server's log names the right one. Until a number is added it
has no key, so nothing can be sent from it, and nothing is stored. Its earlier payouts wait,
unasked, and their checks resume once it is back.

#### From 0.2

0.2 also wrote `ERROR` when it could not tell whether Swish had received a payout, where 0.3 writes
`PENDING`. A redo of such a payout asks Swish about it first, as every redo does.

## Using it as a library

Omit the `http` feature to get the payout engine, the storage code and the Swish client with no web
framework attached:

```toml
[dependencies]
swisha = { git = "https://github.com/zztaeha/swisha", tag = "v1.0.0", default-features = false }
serde_json = "1"  # for the signing example below
```

swisha is not on crates.io, so this is a git dependency. Pin a tag rather than tracking a branch:
an unpinned dependency that moves money is one `cargo update` away from a payout path you have not
read.

| Feature | Default | Adds |
|---|---|---|
| `http` | yes | The HTTP API and the `swisha` binary. Brings in axum |

The library stores payouts in PostgreSQL 18, as the service does: it is the only database
supported.

The part of the protocol that is easiest to get wrong is building and signing the instruction:

```rust
use swisha::swish::payload::{ExecutePayoutArgs, PayoutPayload};
use swisha::swish::sign::{rsa_key_pair, sign_payload};
use swisha::swish::{cert::read_cert_serial, random_payout_uuid};

let signing = std::fs::read_to_string("/var/lib/swisha/1231234567/signing-1231234567-sthlm.pem")?;
let (key, serial) = (rsa_key_pair(&signing)?, read_cert_serial(&signing)?);

// One UUID per payout, forever: a second one is a second instruction, and a second debit.
let uuid = random_payout_uuid();
let args = ExecutePayoutArgs {
    reference: "INV-1042",
    payout_uuid: &uuid,
    payee_alias: "46701234567",
    payee_ssn: Some("196408233234"),
    amount: 250.00,
    message: "Faktura 1042",
};
let payload = PayoutPayload::new(&args, "1231234567", &serial, "2026-10-07T12:00:00Z".into());
let wire = serde_json::to_string(&payload)?; // sign exactly the bytes that are sent
let signature = sign_payload(&wire, &key)?;
```

swisha's calls return `swisha::error::Result`, whose error is `swisha::error::BoxError`, a
`Box<dyn Error + Send + Sync>`. Wrap the example in a function returning that, such as
`fn main() -> swisha::error::Result<()>`: with a plain `Box<dyn Error>`, its `?` does not compile.

Every public item is documented, with examples that compile as tests and run wherever they need no
database, file or network. Open the API documentation with `cargo doc --no-deps --open`; the crate
page has a table of where to look for each task.

## Development

Every test needs a database. There is no in-memory fallback on purpose: the double-payout guard is
an `ON CONFLICT DO UPDATE ... RETURNING`, and testing it against anything but the real engine would
prove nothing. `examples/docker-compose.yml.example` starts PostgreSQL 18 for it on
`127.0.0.1:5433`:

```bash
docker compose -f examples/docker-compose.yml.example up -d

export SWISHA_TEST_DATABASE_URL=postgres://swisha:swisha@localhost:5433/swisha_test
cargo test
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps
```

The docs are held to the build: an undocumented public item fails `cargo check`, a broken
intra-doc link fails `cargo doc`, and under clippy so does a fallible function without an
`# Errors` section, a panicking one without `# Panics`, or an identifier missing its backticks.
`cargo test` compiles every example in the docs, and runs each one that needs no database, file or
network.

Each test creates its own tables under a unique prefix, which the library allows in code though the
service does not, so the suite runs in parallel against one server. Leftovers from an interrupted
run are swept at the start of the next one.

With `SWISHA_TEST_DATABASE_URL` unset the suite refuses to run rather than skipping. A payments
suite reporting `0 failed` while its guards never ran is worse than one that will not start.

With the test bundle from [Quick start](#quick-start) and `SWISH_ENV=test`, payouts go to the MSS
simulator, where no money moves.

A few tests need something outside the repository. Without it they return early and pass:

| Set | Runs |
|---|---|
| `SWISH_TEST_CERT_DIR` | the certificate tests against Swish's real test bundle, the folder holding `Swish_Merchant_TestCertificate_1234679304.pem` |
| `SWISHA_TEST_IMPOSTOR`, `SWISHA_TEST_IMPOSTOR_CA` | with `SWISH_TEST_CERT_DIR`, the check that swisha refuses a server impersonating Swish. The comment at the top of `tests/tls.rs` shows how |
| `SWISHA_TEST_LIVE_STATUS=1` | a read of Swish's live status page |

```bash
SWISH_TEST_CERT_DIR=~/Downloads/client_cert cargo test --test cert
```

## Reference

### Swish error codes

What swisha shows for each code Swish sends, in `error_code`, `error_message` and
`error_category`. The numeric codes are HTTP statuses, used when Swish's refusal names no code. A
code not listed reads `The Swish payout failed.`, `contact_support`. Swedish messages follow
`SWISH_ERROR_LANG`.

| Code | Category | Message |
|---|---|---|
| `400` | `contact_support` | The payout request was malformed. Contact support. |
| `401` | `contact_support` | Certificate authentication failed, or the Swish number in the certificate is not enrolled. Contact support. |
| `403` | `contact_support` | The sending Swish number does not match the merchant's Swish number. Contact support. |
| `415` | `contact_support` | The Content-Type header must be application/json. Contact support. |
| `422` | `user_fixable` | The payout data contains errors. Check the details and try again. |
| `429` | `retryable` | Too many attempts. Wait a moment and try again. |
| `500` | `retryable` | Unknown error at Swish. Try again shortly. |
| `ACMT03` | `contact_support` | Your Swish number is not enrolled for payouts. Contact support. |
| `ACMT07` | `user_fixable` | The recipient is not enrolled in Swish. Check the Swish number. |
| `ACMT17` | `user_fixable` | The Swish number is not valid. Check that the number is correct. |
| `AM02` | `user_fixable` | The amount is above the highest allowed limit. |
| `AM06` | `user_fixable` | The amount is below the lowest allowed limit. |
| `CD01` | `user_fixable` | The amount exceeds a limit on the receiving account. The recipient should check with their bank. |
| `FF08` | `contact_support` | The payment reference is invalid. Contact support. |
| `FF10` | `retryable` | The bank's system could not process the payout. Try again. |
| `PA01` | `contact_support` | A parameter in the request is invalid. Contact support. |
| `PA02` | `contact_support` | The amount is missing or is not a valid number. Contact support. |
| `PA06` | `user_fixable` | The personal identity number has the wrong format. Check the details. |
| `RF07` | `user_fixable` | The payout was declined. Your Swish limit may be exceeded, or the account may have insufficient funds. Check with your bank. |
| `RP01` | `contact_support` | The merchant's Swish number is missing. Contact support. |
| `RP02` | `contact_support` | The message is incorrectly formatted. Contact support. |
| `RP03` | `contact_support` | The callback URL is missing or does not use HTTPS. Contact support. |
| `RP09` | `retryable` | The given instructionUUID is not available. Try again. |
| `RR04` | `contact_support` | The payout was declined for regulatory reasons. Contact the bank. |
| `TA01` | `retryable` | Temporary technical error at Swish. Check whether the payout went through before sending it again. |
| `TM01` | `retryable` | Swish timed out before the payout was started. Confirm it did not go through before sending it again. |
| `VR02` | `user_fixable` | The Swish number does not match the given personal identity number. Check the details. |

`retryable` is a hint for the person deciding whether to redo a payout, never something swisha
acts on itself. A payout Swish declined over the recipient's details, `ACMT07`, `ACMT17`, `PA06`,
`VR02`, `AM02` or `AM06`, is not redone, since it would be declined again. A `NEEDS_REVIEW` payout
always reads "The outcome is not known: Swish may have received this payout. Check with Swish
before paying it again.", whatever code its send left.

### History events

Each row of a payout's history in `swisha_events`, and on the `events` channel, names what
happened in its `event` column:

| Event | Written when |
|---|---|
| `INITIATED` | The payout is claimed and about to be sent. Stored with the claim itself |
| `ACCEPTED` | Swish took the instruction: its `201`, `RP09` at the send, or an answer that it holds the instruction without an outcome yet |
| `CALLBACK` | Swish called back. `status` holds what it said, with any `error_code` |
| `PAID`, `DEBITED`, `ERROR`, `DECLINED` | swisha asked Swish, and this is what it answered |
| `DECLINED` | Swish refused the send outright, so no money moved. `error_code` says why |
| `PENDING` | The send got no usable answer, so Swish may hold it |
| `NEEDS_REVIEW` | The checks ended with no answer from Swish |
| `CHECKS_ENDED` | The checks ended with the payout at `DEBITED` or `ERROR`, Swish's last word |
| `CONFIRMED_PAID` | A person confirmed it paid: `actor` and `note` say who and why |
| `REDONE` | A person redid it under a new reference: `actor` and `note` say who and why |

`ip` holds the address the request came from, where one did.

### Timeouts and limits

| What | Value |
|---|---|
| The follow-up after a payout | half a second, then every 2 seconds, up to 8 questions; up to 8 more after `DEBITED` |
| The checks | 5 minutes after a payout last changed, then hourly, until 48 hours after it was made |
| Questions to Swish about one number's checks | one a second, on each swisha |
| How often swisha looks for due checks | every 5 seconds |
| A question to Swish about a payout | 4 seconds |
| A payout sent to Swish | 55 seconds in all, and 5 to open the connection. Redirects are never followed |
| Probes of Swish, for health | one number a second. One that got no answer is asked again after 30 seconds, one that answered after 10 minutes |
| Swish's status page | every 5 minutes, 10 seconds to answer |
| Reading the numbers again, for changes made elsewhere | every 10 seconds |
| A certificate write cut short | removed once an hour old, looked for every 10 minutes |
| Health turns `degraded` before a certificate expires | 30 days |
| A row written on its own in a payout's history, retried | up to 30 seconds |
| A stop, waiting for requests in flight | 60 seconds |
| A database statement | 10 seconds |
| Waiting for a database connection | 5 seconds |
| Database connections per swisha | 10 by default, `DB_POOL_SIZE` |
| A schema step waiting for its lock | 5 seconds |
| Payouts signed at once | twice the host's cores |
| Payouts and redos per caller and number | 30 a minute by default, `SWISH_PAYOUTS_PER_MINUTE` |
| Event streams | 64 per number, 10,000 per swisha; 128 events held for a slow reader; a keep-alive every 15 seconds |
| Connections | 12,000 on the internal API, 1,024 on the callback |
| Sending a request's headers, and then its body | 30 seconds each |
| A request's head, and a callback's body | 16 KB, and 64 KB |
| A reference, a message, a name, a nickname, `by`, `note` | 35, 50, 32, 100, 100 and 500 characters |
| An amount | 1 to 150,000 SEK, `SWISH_MAX_PAYOUT` |

### Network

What swisha connects to, and what connects to it:

| Direction | Address | What for |
|---|---|---|
| Out | `https://cpc.getswish.net` with `SWISH_ENV=production`, `https://mss.cpc.getswish.net` otherwise | Payouts, questions about them, and probes, over mutual TLS with each number's certificate |
| Out | `https://status.swish.nu` | Swish's status page, every 5 minutes |
| Out | PostgreSQL, at `DB_HOST` | Everything swisha stores |
| In | The internal API: `SWISH_SERVER_SOCKET` or `SWISH_SERVER_ADDR` | Your own backend only |
| In | The callback listener, `SWISH_CALLBACK_ADDR`, behind your proxy | Swish's callbacks |

The paths swisha calls on Swish are `POST /swish-cpcapi/api/v1/payouts`,
`GET /swish-cpcapi/api/v1/payouts/{instruction UUID}`, and, for a probe,
`GET /swish-cpcapi/api/v1/payouts/ping`.

Swish's callback addresses, the only ones accepted when `SWISH_ENV=production`, and worth allowing
at your firewall in front of the proxy:

```text
89.46.83.171
193.53.81.202
193.53.81.203
193.53.81.204
193.53.81.205
213.132.115.86
213.132.115.90
213.132.115.94
```

### Command line

```text
usage: swisha                   serve, configured by SWISHA_ENV_FILE or the environment
       swisha --print-schema    print the PostgreSQL tables swisha uses
       swisha --new-key NAME    give a number a new key and print it; the old one stops working
       swisha --disable NAME    stop a number taking new payouts
       swisha --enable NAME     let a disabled number take payouts again
       swisha --list-payers     list every number, whether it can pay, and its certificates
```

`--print-schema` needs no configuration or database. The other commands read the same env file as
the service: see [On the server](#on-the-server). A command prints its result alone on stdout, and
its log lines on stderr.

| Exit code | Means |
|---|---|
| `0` | Stopped cleanly, after `SIGTERM` or Ctrl-C, or a command succeeded |
| `1` | It could not start: the configuration, the database, a listener, the socket. The reason is printed. A command that failed exits `1` too |
| `2` | An unknown argument. The usage above is printed |

A healthy start logs these lines, among others:

```text
configuration loaded from /etc/swisha/active.env
Swish numbers stored: 12, ready to pay: 12
swisha payout and health listening on unix:/run/swisha/swisha.sock
swisha callback listening on 127.0.0.1:8084
callback IP whitelist (8 entries): 193.53.81.202, 193.53.81.203, …
```

With `SWISH_ENV=test` the last line is a warning instead, that the callback allowlist is
disabled.

### What is in `examples/`

Everything here is a working file rather than a sketch. The unit file parses, the nginx config
passes `nginx -t` on nginx 1.22 and newer, and the SQL runs against PostgreSQL 18.

| File | What it is |
|---|---|
| `swisha.dev.example` | Config template for the MSS simulator. No money moves |
| `swisha.prod.example` | Config template for production |
| `swisha.service.example` | systemd unit |
| `nginx.conf.example` | The reverse proxy, publishing the callback and nothing else |
| `docker-compose.yml.example` | PostgreSQL 18 for local development and the test suite |
| `setup.sql.example` | The role and database, before `--print-schema` makes the tables |

### Swish's documentation

- [Swish CPC API documentation](https://developer.swish.nu/)
- [Swish payouts API reference](https://developer.swish.nu/api/payouts/v1)
- [Swish test certificates and environment](https://developer.swish.nu/documentation/environments)

## License

MIT
