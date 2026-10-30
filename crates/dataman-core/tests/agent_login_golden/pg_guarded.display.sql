BEGIN;
-- the login (password sent as a SCRAM verifier, never in clear)
CREATE ROLE "dataman_agent_a1b2c3d4" LOGIN PASSWORD 'SCRAM-SHA-256$4096:…'
  NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS;
COMMENT ON ROLE "dataman_agent_a1b2c3d4" IS 'dataman-managed v1 conn=a1b2c3d4e5f6';

-- where it may connect
GRANT CONNECT ON DATABASE "shop" TO "dataman_agent_a1b2c3d4";
GRANT USAGE ON SCHEMA "public" TO "dataman_agent_a1b2c3d4";

-- tables the agent may read and write (2)
GRANT SELECT, INSERT, UPDATE, DELETE
  ON "public"."orders"
  TO "dataman_agent_a1b2c3d4";

-- tables with hidden or masked columns: only the others
GRANT
  SELECT ("created_at", "id", "name"),
  INSERT ("created_at", "id", "name"),
  UPDATE ("created_at", "id", "name"),
  DELETE
  ON "public"."users"
  TO "dataman_agent_a1b2c3d4";

-- masked columns: views the agent reads instead (1)
CREATE SCHEMA "dataman_agent_a1b2c3d4_views";
COMMENT ON SCHEMA "dataman_agent_a1b2c3d4_views" IS 'dataman-managed v1';
CREATE VIEW "dataman_agent_a1b2c3d4_views"."users" WITH (security_barrier) AS
SELECT
  "id",
  "name",
  "created_at",
  CASE
    WHEN "email"::text IS NULL THEN NULL
    WHEN strpos("email"::text, '@') > 0
      THEN left("email"::text, 1) || '•••@' || split_part("email"::text, '@', 2)
    WHEN length("email"::text) >= 8 THEN '•••• ' || right("email"::text, 4)
    ELSE '••••'
  END AS "email"
FROM "public"."users";
COMMENT ON VIEW "dataman_agent_a1b2c3d4_views"."users" IS 'dataman-managed v1 2b9173d7e722';

-- where it may connect
GRANT USAGE
  ON SCHEMA "dataman_agent_a1b2c3d4_views"
  TO "dataman_agent_a1b2c3d4";

-- the views
GRANT SELECT
  ON "dataman_agent_a1b2c3d4_views"."users"
  TO "dataman_agent_a1b2c3d4";

-- the sequences its inserts take ids from
GRANT USAGE ON SEQUENCE "public"."orders_id_seq" TO "dataman_agent_a1b2c3d4";

-- defaults (Dataman still enforces its own 30 s)
ALTER ROLE "dataman_agent_a1b2c3d4" IN DATABASE "shop" SET idle_in_transaction_session_timeout = '60s';
ALTER ROLE "dataman_agent_a1b2c3d4" IN DATABASE "shop" SET statement_timeout = '30s';
ALTER ROLE "dataman_agent_a1b2c3d4" CONNECTION LIMIT 4;

-- hidden: public.audit_log → no grant at all
COMMIT;

-- needs a login that can grant on them, such as their owner (not run):
-- GRANT SELECT, INSERT, UPDATE, DELETE ON "public"."ledger" TO "dataman_agent_a1b2c3d4";
