This is a tool that creates backups of PostgreSQL databases and automatically deletes them as disk space becomes full. It's intended to be run as a cronjob.

# Setup

1. Create `/usr/local/share/pgbackuproll` and give ownership to the `postgres` user.
2. Create a cronjob that runs `pgbackuproll` as `postgres`.
