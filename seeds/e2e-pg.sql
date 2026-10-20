-- Run by `xtask e2e` before the stages, after phase2.sql: removes what an interrupted check
-- leaves behind in the bench database, so each run starts from the seeds.
DO $$
DECLARE s text;
BEGIN
    FOR s IN SELECT nspname FROM pg_namespace WHERE nspname LIKE 'mcp\_check\_%' OR nspname LIKE 'e2e\_%' LOOP
        EXECUTE format('DROP SCHEMA %I CASCADE', s);
    END LOOP;
END
$$;
DROP TABLE IF EXISTS public.insert_t, public.newuser_t CASCADE;
DROP SCHEMA IF EXISTS dataman_activity_demo_check CASCADE;
DROP TABLE IF EXISTS public.e2e_ddl, public.e2e_parquet CASCADE;
DROP TABLE IF EXISTS dataman_scratch.products_edit;
