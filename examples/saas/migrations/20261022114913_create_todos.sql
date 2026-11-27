-- Each todo belongs to one user; deleting the user deletes their todos.
CREATE TABLE todos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    done BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE INDEX todos_by_user ON todos (user_id);
