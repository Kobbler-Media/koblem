-- Browser push endpoints, one row per installed PWA.
--
-- The SSE stream and the tab-open Notification only reach someone with the page open. This is the
-- half that reaches a phone with the browser closed.

CREATE TABLE push_subscriptions (
    -- The push service URL the browser minted. It IS the identity: a re-subscribe from the same
    -- installation returns the same endpoint, so an upsert on it is what makes "enable
    -- notifications" idempotent rather than accumulating a row per page load.
    endpoint     TEXT PRIMARY KEY,

    -- The subscription's own P-256 public key and auth secret, base64url. RFC8291 payload
    -- encryption is per-subscription; without both the server can only send an empty push.
    p256dh       TEXT NOT NULL,
    auth         TEXT NOT NULL,

    -- `Principal::label()` — so the row is attributable when several people share the instance.
    created_by   TEXT NOT NULL,

    -- 🔴 A push service answering 404/410 means the subscription is permanently gone (the browser
    -- was uninstalled, the user cleared site data) and the row is DELETED. Everything else is
    -- transient — a 5xx, a timeout, a phone off a network — and only bumps `failures`, because
    -- deleting on those would silently unsubscribe someone whose push service had a bad hour.
    failures     INTEGER NOT NULL DEFAULT 0,

    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at TIMESTAMPTZ
);
