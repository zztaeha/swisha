-- The schema swisha uses, printed by swisha --print-schema. One swisha pays from every stored
-- number using these four tables, and checks at startup that they match. Running it again on a
-- database an older swisha made adds what that database lacks and keeps its rows.
--
-- swisha stores only what its payout state machine needs. Business context such as invoices,
-- orders or line items belongs in your own tables, keyed by the same payer and reference.

CREATE TABLE IF NOT EXISTS swisha_payouts (
    -- The name of the number that pays, from swisha_payers. With the reference, the idempotency
    -- key for the double-payout guard: the same reference under two payers is two payouts.
    payer        VARCHAR(32)   NOT NULL,
    -- Your identifier for this payout, sent to Swish as payerPaymentReference.
    reference    VARCHAR(35)   NOT NULL,
    -- The Swish number it was paid from: the audit trail, and how adding a number spots one that
    -- already paid under another name.
    payer_alias  VARCHAR(20)   NOT NULL,
    payee_alias  VARCHAR(20)   NOT NULL,
    payee_ssn    VARCHAR(12),
    amount       NUMERIC(10,2) NOT NULL,
    -- What the recipient reads: at most 50 characters, Swish's limit, cut before it is stored.
    message      VARCHAR(50)   NOT NULL,
    -- payoutInstructionUUID: 32 uppercase hex characters. VARCHAR, not CHAR: CHAR is
    -- blank-padded, so a shorter value reads back with trailing spaces and never compares
    -- equal to what was written.
    swish_ref    VARCHAR(32),
    -- Sent with the instruction and echoed by Swish on its callback, as proof it is theirs.
    callback_identifier VARCHAR(32),
    -- CREATED, PENDING, DEBITED, PAID, DECLINED, ERROR, NEEDS_REVIEW
    status       VARCHAR(20)   NOT NULL,
    -- The reference of the payout this one redoes, under the same payer.
    replaces     VARCHAR(35),
    -- When swisha last asked Swish about it. The checks end once the last one, 48 hours after
    -- created_at, is recorded.
    checked_at   TIMESTAMPTZ,
    created_at   TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ   NOT NULL DEFAULT NOW(),
    PRIMARY KEY (payer, reference)
);

-- Each change below is made only where it is missing, and waits at most 5 seconds for its lock: an
-- ALTER TABLE locks the table before it looks, so a re-run would otherwise queue every payout.
DO $$
BEGIN
    PERFORM set_config('lock_timeout', '5s', true);
    IF NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'swisha_payouts'::regclass
                    AND attname = 'checked_at' AND NOT attisdropped) THEN
        ALTER TABLE swisha_payouts ADD COLUMN checked_at TIMESTAMPTZ;
    END IF;
    -- Older versions counted their checks here, and nothing ever read the count.
    IF EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'swisha_payouts'::regclass
                AND attname = 'attempts' AND NOT attisdropped) THEN
        ALTER TABLE swisha_payouts DROP COLUMN attempts;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'swisha_payouts'::regclass
                    AND attname = 'replaces' AND NOT attisdropped) THEN
        ALTER TABLE swisha_payouts ADD COLUMN replaces VARCHAR(35);
    END IF;
END $$;
-- Set once the last check of a payout Swish left at ERROR or DEBITED is recorded. Only the run that
-- adds it marks what an older swisha finished: later, a last check cut off mid-answer looks the same.
DO $$
BEGIN
    PERFORM set_config('lock_timeout', '5s', true);
    IF NOT EXISTS (SELECT 1 FROM pg_attribute
                    WHERE attrelid = 'swisha_payouts'::regclass AND attname = 'checks_ended'
                      AND NOT attisdropped) THEN
        ALTER TABLE swisha_payouts ADD COLUMN checks_ended BOOLEAN NOT NULL DEFAULT FALSE;
        UPDATE swisha_payouts SET checks_ended = TRUE
         WHERE status IN ('ERROR', 'DEBITED') AND checked_at >= created_at + interval '48 hours';
    END IF;
END $$;

-- A payout is redone once: a second redo of the same payout cannot be stored. Only redos are in
-- it, a few among millions; the index it replaces held every payout, and goes once this one exists.
DO $$
BEGIN
    PERFORM set_config('lock_timeout', '5s', true);
    IF to_regclass('idx_swisha_payouts_redo') IS NULL THEN
        CREATE UNIQUE INDEX idx_swisha_payouts_redo
            ON swisha_payouts (payer, replaces) WHERE replaces IS NOT NULL;
    END IF;
    DROP INDEX IF EXISTS idx_swisha_payouts_replaces;
END $$;

-- The checks read only payouts still checked, statuses UNCONFIRMED in src/domain/status.rs, held
-- equal by a test, in order of last check: a claim read 14 buffers, not 4,030, at 201,000 of them.
DROP INDEX IF EXISTS idx_swisha_payouts_stalled;
DROP INDEX IF EXISTS idx_swisha_payouts_checks;
DO $$
BEGIN
    PERFORM set_config('lock_timeout', '5s', true);
    IF to_regclass('idx_swisha_payouts_due_at') IS NULL THEN
        CREATE INDEX idx_swisha_payouts_due_at
            ON swisha_payouts (payer, checked_at) WHERE status IN ('CREATED', 'PENDING', 'ERROR', 'DEBITED') AND NOT checks_ended;
    END IF;
END $$;
DROP INDEX IF EXISTS idx_swisha_payouts_due;

-- An add asks whether its number ever paid out under another name. Without this, that read every
-- payout: over a second and 2 GB from disk per add at 10 million.
DO $$
BEGIN
    PERFORM set_config('lock_timeout', '5s', true);
    IF to_regclass('idx_swisha_payouts_alias') IS NULL THEN
        CREATE INDEX idx_swisha_payouts_alias ON swisha_payouts (payer_alias, payer);
    END IF;
END $$;

-- One row per payout event: the submission, Swish taking it, each answer from Swish, and what a
-- person did.
CREATE TABLE IF NOT EXISTS swisha_events (
    id            BIGSERIAL     PRIMARY KEY,
    payer         VARCHAR(32)   NOT NULL,
    reference     VARCHAR(35)   NOT NULL,
    swish_ref     VARCHAR(32),
    event         VARCHAR(50)   NOT NULL,
    status        VARCHAR(20),
    amount        NUMERIC(10,2),
    payee_alias   VARCHAR(20),
    error_code    VARCHAR(20),
    error_message TEXT,
    ip            VARCHAR(45),
    -- Who did it and why, when a person did it: a confirmation or a redo.
    actor         VARCHAR(100),
    note          TEXT,
    created_at    TIMESTAMPTZ   NOT NULL DEFAULT NOW()
);

DO $$
BEGIN
    PERFORM set_config('lock_timeout', '5s', true);
    IF NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'swisha_events'::regclass
                    AND attname = 'actor' AND NOT attisdropped) THEN
        ALTER TABLE swisha_events ADD COLUMN actor VARCHAR(100);
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = 'swisha_events'::regclass
                    AND attname = 'note' AND NOT attisdropped) THEN
        ALTER TABLE swisha_events ADD COLUMN note TEXT;
    END IF;
    IF to_regclass('idx_swisha_events_payer_reference') IS NULL THEN
        CREATE INDEX idx_swisha_events_payer_reference ON swisha_events (payer, reference);
    END IF;
END $$;
-- No query reads events by time alone, and it slowed every insert. Dropped where an older swisha
-- made it.
DROP INDEX IF EXISTS idx_swisha_events_created_at;

-- The Swish numbers swisha pays from. Each one's two certificates are files in SWISH_CERT_DIR,
-- never in the database.
CREATE TABLE IF NOT EXISTS swisha_payers (
    -- What callers send as payer. Permanent: with the reference, the double-payout key.
    name        VARCHAR(32)  PRIMARY KEY,
    -- What people call it. May change at any time.
    nickname    VARCHAR(100) NOT NULL,
    -- Once only, disabled numbers included: a callback finds its payer by number.
    number      VARCHAR(20)  NOT NULL UNIQUE,
    added_by    VARCHAR(100) NOT NULL,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    -- Every change, new certificates included. A running swisha reloads a number when it moves.
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    -- Set while it takes no new payouts. Its earlier payouts still settle.
    disabled_at TIMESTAMPTZ,
    -- SHA-256 of the number's key, in hex. The key is shown once, when made, and never kept.
    -- VARCHAR, since a CHAR compared with the text a driver sends cannot use the index.
    key_hash    VARCHAR(64)  NOT NULL UNIQUE
);

-- General service log.
CREATE TABLE IF NOT EXISTS swisha_logs (
    id        BIGSERIAL   PRIMARY KEY,
    level     VARCHAR(20) NOT NULL,
    message   TEXT        NOT NULL,
    context   TEXT,
    ip        VARCHAR(45),
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
