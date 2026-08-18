CREATE TABLE factory_settings (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    auto_mode INTEGER NOT NULL CHECK (auto_mode IN (0, 1)),
    updated_at_ms INTEGER NOT NULL
) STRICT;

INSERT INTO factory_settings (singleton, auto_mode, updated_at_ms)
VALUES (1, 1, 0);
