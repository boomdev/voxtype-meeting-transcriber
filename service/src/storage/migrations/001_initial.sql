CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    state TEXT NOT NULL CHECK (state IN ('running', 'completed', 'interrupted')),
    started_at TEXT NOT NULL,
    ended_at TEXT,
    microphone_id TEXT,
    microphone_description TEXT,
    output_id TEXT,
    output_description TEXT,
    monitor_id TEXT,
    monitor_description TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE transcription_runs (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    provider TEXT NOT NULL CHECK (provider IN ('voxtype', 'openai', 'whisper_cpp')),
    model TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE audio_chunks (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    source TEXT NOT NULL CHECK (source IN ('mic', 'system')),
    sequence INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT NOT NULL,
    file_path TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (session_id, source, sequence)
);

CREATE TABLE transcription_jobs (
    id TEXT PRIMARY KEY,
    audio_chunk_id TEXT NOT NULL REFERENCES audio_chunks(id),
    run_id TEXT NOT NULL REFERENCES transcription_runs(id),
    provider TEXT NOT NULL CHECK (provider IN ('voxtype', 'openai', 'whisper_cpp')),
    model TEXT,
    state TEXT NOT NULL CHECK (state IN ('pending', 'processing', 'completed')),
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    next_retry_at TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    completed_at TEXT
);

CREATE INDEX idx_jobs_due ON transcription_jobs (state, next_retry_at);

CREATE TABLE transcript_events (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    audio_chunk_id TEXT NOT NULL REFERENCES audio_chunks(id),
    job_id TEXT NOT NULL UNIQUE REFERENCES transcription_jobs(id),
    source TEXT NOT NULL CHECK (source IN ('mic', 'system')),
    sequence INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT NOT NULL,
    text TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    is_canonical INTEGER NOT NULL DEFAULT 1 CHECK (is_canonical IN (0, 1)),
    created_at TEXT NOT NULL
);

CREATE UNIQUE INDEX idx_canonical_event_per_chunk
    ON transcript_events (audio_chunk_id)
    WHERE is_canonical = 1;
