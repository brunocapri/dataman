BEGIN TRANSACTION;
-- the login (password sent as its hash, never in clear)
CREATE LOGIN [dataman_agent_a1b2c3d4] WITH
  PASSWORD = 0x0200… HASHED,
  CHECK_POLICY = OFF,
  CHECK_EXPIRATION = OFF,
  DEFAULT_DATABASE = [shop];
ALTER LOGIN [dataman_agent_a1b2c3d4] DISABLE;
CREATE USER [dataman_agent_a1b2c3d4] FOR LOGIN [dataman_agent_a1b2c3d4] WITH
  DEFAULT_SCHEMA = [dbo];
EXEC sys.sp_addextendedproperty
  @name = N'dataman',
  @value = N'dataman-managed v1 conn=a1b2c3d4e5f6',
  @level0type = N'USER',
  @level0name = N'dataman_agent_a1b2c3d4';

-- hidden or masked columns: views the agent reads instead (1)
CREATE SCHEMA [dataman_agent_a1b2c3d4_views] AUTHORIZATION [dbo];
EXEC sys.sp_addextendedproperty
  @name = N'dataman',
  @value = N'dataman-managed v1',
  @level0type = N'SCHEMA',
  @level0name = N'dataman_agent_a1b2c3d4_views';
CREATE VIEW [dataman_agent_a1b2c3d4_views].[users] AS
SELECT
  [id],
  [name],
  [created_at],
  CASE
    WHEN CAST([email] AS NVARCHAR(4000)) IS NULL THEN NULL
    WHEN CHARINDEX(N'@', CAST([email] AS NVARCHAR(4000))) > 0
      THEN LEFT(CAST([email] AS NVARCHAR(4000)), 1) + N'•••@' + SUBSTRING(CAST([email] AS NVARCHAR(4000)), CHARINDEX(N'@', CAST([email] AS NVARCHAR(4000))) + 1, 4000)
    WHEN LEN(CAST([email] AS NVARCHAR(4000))) >= 8
      THEN N'•••• ' + RIGHT(CAST([email] AS NVARCHAR(4000)), 4)
    ELSE N'••••'
  END AS [email]
FROM [dbo].[users];
EXEC sys.sp_addextendedproperty
  @name = N'dataman',
  @value = N'dataman-managed v1 8df16d71c3a2',
  @level0type = N'SCHEMA',
  @level0name = N'dataman_agent_a1b2c3d4_views',
  @level1type = N'VIEW',
  @level1name = N'users';

-- the views
GRANT SELECT
  ON OBJECT::[dataman_agent_a1b2c3d4_views].[users]
  TO [dataman_agent_a1b2c3d4];

-- tables the agent may use (2)
GRANT SELECT, INSERT, UPDATE, DELETE
  ON OBJECT::[dbo].[orders]
  TO [dataman_agent_a1b2c3d4];

-- writes on tables read through a view: only the visible columns
GRANT SELECT ([created_at], [id], [name]), UPDATE ([created_at], [id], [name])
  ON OBJECT::[dbo].[users]
  TO [dataman_agent_a1b2c3d4];
GRANT DELETE ON OBJECT::[dbo].[users] TO [dataman_agent_a1b2c3d4];

-- hidden: dbo.audit_log → no grant at all

-- ready: enabled last
ALTER LOGIN [dataman_agent_a1b2c3d4] ENABLE;
COMMIT;

-- needs a login that can grant on them, such as their owner (not run):
-- GRANT SELECT, INSERT, UPDATE, DELETE ON OBJECT::[dbo].[ledger] TO [dataman_agent_a1b2c3d4];
