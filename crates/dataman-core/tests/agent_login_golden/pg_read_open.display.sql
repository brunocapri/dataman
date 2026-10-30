BEGIN;
-- the login (password sent as a SCRAM verifier, never in clear)
CREATE ROLE "dataman_agent_a1b2c3d4" LOGIN PASSWORD 'SCRAM-SHA-256$4096:…'
  NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS;
COMMENT ON ROLE "dataman_agent_a1b2c3d4" IS 'dataman-managed v1 conn=a1b2c3d4e5f6';

-- where it may connect
GRANT CONNECT ON DATABASE "shop" TO "dataman_agent_a1b2c3d4";
GRANT USAGE ON SCHEMA "public" TO "dataman_agent_a1b2c3d4";

-- tables the agent may read (3)
GRANT SELECT ON "public"."audit_log" TO "dataman_agent_a1b2c3d4";
GRANT SELECT ON "public"."orders" TO "dataman_agent_a1b2c3d4";
GRANT SELECT ON "public"."users" TO "dataman_agent_a1b2c3d4";

-- server activity
GRANT "pg_read_all_stats" TO "dataman_agent_a1b2c3d4";

-- defaults (Dataman still enforces its own 5 min)
ALTER ROLE "dataman_agent_a1b2c3d4" IN DATABASE "shop" SET default_transaction_read_only = 'on';
ALTER ROLE "dataman_agent_a1b2c3d4" IN DATABASE "shop" SET idle_in_transaction_session_timeout = '60s';
ALTER ROLE "dataman_agent_a1b2c3d4" IN DATABASE "shop" SET statement_timeout = '300s';
ALTER ROLE "dataman_agent_a1b2c3d4" CONNECTION LIMIT 4;
COMMIT;

-- needs a login that can grant on them, such as their owner (not run):
-- GRANT SELECT ON "public"."ledger" TO "dataman_agent_a1b2c3d4";
