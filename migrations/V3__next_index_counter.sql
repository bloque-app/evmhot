-- P0 register address-collision fix (docs/fix-p0-register-address-collision.md,
-- Option 1): new registrations allocate a sequential derivation index from a
-- persisted counter instead of hashing the account id.
--
-- Seed the counter above the current max index so new sequential allocations
-- never collide with legacy hash-derived indices. Existing accounts are left
-- untouched. INSERT OR IGNORE keeps this idempotent if a 'next_index' row
-- somehow already exists.
INSERT OR IGNORE INTO state (key, value)
SELECT 'next_index', CAST(COALESCE(MAX(derivation_index) + 1, 0) AS TEXT)
FROM accounts;
