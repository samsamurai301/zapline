-- Accounts for `src/routes/auth.rs`. Emails are stored lowercased.
CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    email TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    -- Bumped when the password changes, which ends every other session.
    -- Set once the owner proves the address (a used reset link). Admin
    -- access needs it.
    email_verified INTEGER NOT NULL DEFAULT 0,
    session_version INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Password-reset links: only a SHA-256 of each token is stored.
CREATE TABLE password_resets (
    token_hash TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    expires_at BIGINT NOT NULL
);
