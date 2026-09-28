-- 1003 -> 1004: BE-B6 independent per-occurrence timers. Replaces the one-running-timer-per-account
-- unique index with one scoped to the person's occurrence stream (`progress_id`), and adds the
-- index behind the account-wide history listing. Index-only: no row of any table is read,
-- rewritten, exported or dropped. The old index allowed at most one running row per account,
-- which implies at most one per (account_id, progress_id), so the new unique index cannot fail on
-- any valid 1003 data. Strict DDL (no IF [NOT] EXISTS): a 1003 database that lacks the old index
-- is corrupt and must fail loudly rather than be papered over.
CREATE UNIQUE INDEX timer_active_account_occurrence ON timer_sessions(account_id,progress_id) WHERE stopped_at IS NULL AND cancelled=0;
CREATE INDEX timer_account_stopped ON timer_sessions(account_id,stopped_at,id) WHERE stopped_at IS NOT NULL AND cancelled=0;
DROP INDEX timer_active_account;
