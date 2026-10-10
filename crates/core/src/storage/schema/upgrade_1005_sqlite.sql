-- 1004 -> 1005: BE-Q25 recognisable sessions and devices. Adds the coarse client label of each new
-- session (three nullable columns, closed values) and `device_profiles`, the per-device name and
-- last client label. Additive only: no existing row is read, rewritten, exported or dropped, and
-- existing sessions keep NULL labels (there is no source to backfill them from). The CHECKs let
-- NULL through, so every valid 1004 row satisfies them. Strict DDL (no IF [NOT] EXISTS): a 1004
-- database that already has these objects is not a 1004 database and must fail loudly.
ALTER TABLE sessions ADD COLUMN client_channel TEXT CHECK (client_channel IN ('browser','native'));
ALTER TABLE sessions ADD COLUMN client_browser TEXT CHECK (client_browser IN ('chrome','edge','firefox','safari','opera','samsung_internet','unknown'));
ALTER TABLE sessions ADD COLUMN client_platform TEXT CHECK (client_platform IN ('windows','macos','ios','android','linux','chromeos','unknown'));
CREATE TABLE device_profiles (
 account_id TEXT NOT NULL REFERENCES accounts(id), device_id TEXT NOT NULL,
 name TEXT CHECK (length(name) BETWEEN 1 AND 64),
 client_channel TEXT CHECK (client_channel IN ('browser','native')),
 client_browser TEXT CHECK (client_browser IN ('chrome','edge','firefox','safari','opera','samsung_internet','unknown')),
 client_platform TEXT CHECK (client_platform IN ('windows','macos','ios','android','linux','chromeos','unknown')),
 updated_at BIGINT NOT NULL, PRIMARY KEY(account_id,device_id)
);
