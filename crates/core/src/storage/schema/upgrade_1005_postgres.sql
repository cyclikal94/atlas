-- 1004 -> 1005: BE-Q25 recognisable sessions and devices. Adds the coarse client label of each new
-- session (three nullable columns, closed values) and `device_profiles`, the per-device name and
-- last client label. Additive only: no existing row is read, rewritten, exported or dropped, and
-- existing sessions keep NULL labels (there is no source to backfill them from). The CHECKs let
-- NULL through, so every valid 1004 row satisfies them; adding a nullable column without a default
-- rewrites no row. Strict DDL (no IF [NOT] EXISTS): a 1004 database that already has these objects
-- is not a 1004 database and must fail loudly. ALTER TABLE takes an ACCESS EXCLUSIVE lock on
-- sessions for a moment; other replicas already wait on migrate()'s advisory lock.
ALTER TABLE sessions
    ADD COLUMN client_channel text,
    ADD COLUMN client_browser text,
    ADD COLUMN client_platform text,
    ADD CONSTRAINT sessions_client_channel_check CHECK ((client_channel = ANY (ARRAY['browser'::text, 'native'::text]))),
    ADD CONSTRAINT sessions_client_browser_check CHECK ((client_browser = ANY (ARRAY['chrome'::text, 'edge'::text, 'firefox'::text, 'safari'::text, 'opera'::text, 'samsung_internet'::text, 'unknown'::text]))),
    ADD CONSTRAINT sessions_client_platform_check CHECK ((client_platform = ANY (ARRAY['windows'::text, 'macos'::text, 'ios'::text, 'android'::text, 'linux'::text, 'chromeos'::text, 'unknown'::text])));

CREATE TABLE device_profiles (
    account_id text NOT NULL,
    device_id text NOT NULL,
    name text,
    client_channel text,
    client_browser text,
    client_platform text,
    updated_at bigint NOT NULL,
    CONSTRAINT device_profiles_name_check CHECK (((length(name) >= 1) AND (length(name) <= 64))),
    CONSTRAINT device_profiles_client_channel_check CHECK ((client_channel = ANY (ARRAY['browser'::text, 'native'::text]))),
    CONSTRAINT device_profiles_client_browser_check CHECK ((client_browser = ANY (ARRAY['chrome'::text, 'edge'::text, 'firefox'::text, 'safari'::text, 'opera'::text, 'samsung_internet'::text, 'unknown'::text]))),
    CONSTRAINT device_profiles_client_platform_check CHECK ((client_platform = ANY (ARRAY['windows'::text, 'macos'::text, 'ios'::text, 'android'::text, 'linux'::text, 'chromeos'::text, 'unknown'::text])))
);

ALTER TABLE ONLY device_profiles
    ADD CONSTRAINT device_profiles_pkey PRIMARY KEY (account_id, device_id);

ALTER TABLE ONLY device_profiles
    ADD CONSTRAINT device_profiles_account_id_fkey FOREIGN KEY (account_id) REFERENCES accounts(id);
