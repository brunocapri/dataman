-- the login, locked until the end (password sent as its hash, never in clear)
CREATE USER 'dataman_agent_a1b2c3d4'@'%'
  IDENTIFIED WITH caching_sha2_password AS '$A$005$…'
  WITH MAX_USER_CONNECTIONS 4
  PASSWORD REQUIRE CURRENT
  ACCOUNT LOCK
  ATTRIBUTE '{"dataman":"dataman-managed v1","conn":"a1b2c3d4e5f6"}';

-- masked columns: views the agent reads instead (1)
CREATE DATABASE `dataman_agent_a1b2c3d4_views`;
CREATE SQL SECURITY DEFINER VIEW `dataman_agent_a1b2c3d4_views`.`shop__users` AS
SELECT
  `id`,
  `name`,
  `created_at`,
  CASE
    WHEN CAST(`email` AS CHAR) IS NULL THEN NULL
    WHEN CHAR_LENGTH(CAST(`email` AS CHAR)) > 4
      THEN CONCAT(REPEAT('•', CHAR_LENGTH(CAST(`email` AS CHAR)) - 4), RIGHT(CAST(`email` AS CHAR), 4))
    ELSE REPEAT('•', CHAR_LENGTH(CAST(`email` AS CHAR)))
  END AS `email`
FROM `shop`.`users`;

-- the views
GRANT SELECT
  ON `dataman_agent_a1b2c3d4_views`.`shop__users`
  TO 'dataman_agent_a1b2c3d4'@'%';

-- tables the agent may use (2)
GRANT SELECT, INSERT, UPDATE, DELETE
  ON `shop`.`orders`
  TO 'dataman_agent_a1b2c3d4'@'%';

-- tables with hidden or masked columns: only the others
GRANT
  SELECT (`created_at`, `id`, `name`),
  INSERT (`created_at`, `id`, `name`),
  UPDATE (`created_at`, `id`, `name`),
  DELETE
  ON `shop`.`users`
  TO 'dataman_agent_a1b2c3d4'@'%';

-- the views
ALTER USER 'dataman_agent_a1b2c3d4'@'%'
  ATTRIBUTE '{"views":{"shop__users":"4cf487ca5f53"}}';

-- hidden: shop.audit_log → no grant at all

-- ready: unlocked last
ALTER USER 'dataman_agent_a1b2c3d4'@'%' ACCOUNT UNLOCK;

-- needs a login that can grant on them, such as their owner (not run):
-- GRANT SELECT, INSERT, UPDATE, DELETE ON `shop`.`ledger` TO 'dataman_agent_a1b2c3d4'@'%';
