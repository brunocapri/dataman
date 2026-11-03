Dataman for Windows
===================

dataman.exe      the app
dataman-mcp.exe  the MCP server agents start (a console program: stdin/stdout)

Where Dataman keeps things
  Settings, connections, history   %APPDATA%\dataman
  Logs and crash reports           %LOCALAPPDATA%\dataman\logs
  Passwords                        Windows Credential Manager (generic credentials "dataman:...")
  libpq files it reads             %APPDATA%\postgresql\pgpass.conf, .pg_service.conf
  SSH                              %USERPROFILE%\.ssh\config, known_hosts

Agents (MCP)
  Claude Code:     claude mcp add dataman -- "C:\path\to\dataman-mcp.exe"
  Claude Desktop:  %APPDATA%\Claude\claude_desktop_config.json
                   { "mcpServers": { "dataman": { "command": "C:\\path\\to\\dataman-mcp.exe" } } }
  Cursor:          %USERPROFILE%\.cursor\mcp.json, the same "mcpServers" entry
  Settings > Agents in the app shows the exact command for this install.

While the app runs, dataman-mcp hands each call to it (writes are approved there); without it,
the server works on its own with the same rules.

Updates: Dataman checks for new versions. Installed with the installer, it updates itself:
it downloads the new installer, checks its signature, and installs it when you restart it.
The portable .zip only says a new version is out: unzip the new one over this one.
