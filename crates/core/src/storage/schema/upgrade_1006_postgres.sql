-- 1005 -> 1006: BE-PEOPLE-SUMMARY original-creator attribution. Adds `person_attributions`, the
-- account that created each person, kept separately from `resources.owner_id` because link
-- acceptance, a merge into a linked identity and `reference_account` all leave a person owned by
-- an account that did not create it. Additive only: no existing row is read, rewritten or dropped,
-- and the step inserts no attribution. No pre-1006 record proves who created an existing person
-- (ownership may have been transferred, unlink removes the link trace, and receipts hold digests
-- only), so every existing person reads as unknown rather than being given an invented creator.
-- Strict DDL (no IF [NOT] EXISTS): a 1005 database that already has this table is not a 1005
-- database and must fail loudly. Creating an empty table locks nothing that exists.
CREATE TABLE person_attributions (
    person_id text NOT NULL,
    added_by text NOT NULL
);

ALTER TABLE ONLY person_attributions
    ADD CONSTRAINT person_attributions_pkey PRIMARY KEY (person_id);

ALTER TABLE ONLY person_attributions
    ADD CONSTRAINT person_attributions_added_by_fkey FOREIGN KEY (added_by) REFERENCES accounts(id);

ALTER TABLE ONLY person_attributions
    ADD CONSTRAINT person_attributions_person_id_fkey FOREIGN KEY (person_id) REFERENCES resources(id);
