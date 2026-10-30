-- the login, locked until the end (password sent as its hash, never in clear)
CREATE USER 'dataman_agent_a1b2c3d4'@'%'
  IDENTIFIED BY PASSWORD '*…'
  WITH MAX_USER_CONNECTIONS 4 MAX_STATEMENT_TIME 30
  ACCOUNT LOCK;

-- masked columns: views the agent reads instead (1)
CREATE DATABASE `dataman_agent_a1b2c3d4_views` COMMENT 'dataman-managed v1 conn=a1b2c3d4e5f6';
CREATE SQL SECURITY DEFINER VIEW `dataman_agent_a1b2c3d4_views`.`shop__users` AS
SELECT
  `id`,
  `name`,
  `created_at`,
  CASE
    WHEN CAST(`email` AS CHAR) IS NULL THEN NULL
    WHEN LOCATE('@', CAST(`email` AS CHAR)) > 0
      THEN CONCAT(LEFT(CAST(`email` AS CHAR), 1), '•••@', SUBSTRING_INDEX(CAST(`email` AS CHAR), '@', -1))
    WHEN CHAR_LENGTH(CAST(`email` AS CHAR)) >= 8
      THEN CONCAT('•••• ', RIGHT(CAST(`email` AS CHAR), 4))
    ELSE '••••'
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
CREATE TABLE IF NOT EXISTS `dataman_agent_a1b2c3d4_views`.`dataman_views` (name VARCHAR(64) NOT NULL PRIMARY KEY, hash VARCHAR(64) NOT NULL);
REPLACE INTO `dataman_agent_a1b2c3d4_views`.`dataman_views` (name, hash) VALUES ('shop__users', 'f82a2e7ec667');

-- hidden: shop.audit_log → no grant at all

-- ready: unlocked last
ALTER USER 'dataman_agent_a1b2c3d4'@'%' ACCOUNT UNLOCK;

-- needs a login that can grant on them, such as their owner (not run):
-- GRANT SELECT, INSERT, UPDATE, DELETE ON `shop`.`ledger` TO 'dataman_agent_a1b2c3d4'@'%';
