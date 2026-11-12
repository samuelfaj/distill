"""Schema migrations. migrate() applies the pending ones in order, one transaction each."""


def _v1(conn):
    conn.execute('CREATE TABLE accounts (id INTEGER PRIMARY KEY, email TEXT NOT NULL, created_at TEXT NOT NULL)')
    conn.execute('CREATE TABLE orders (id INTEGER PRIMARY KEY, '
                 'account_id INTEGER NOT NULL REFERENCES accounts(id), total_cents INTEGER NOT NULL)')


MIGRATIONS = [_v1]



def current_version(conn):
    conn.execute('CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)')
    return conn.execute('SELECT MAX(version) FROM schema_version').fetchone()[0] or 0


def migrate(conn, target=None):
    """Apply pending migrations up to target (default: the latest) and return the version."""
    target = len(MIGRATIONS) if target is None else target
    for number in range(current_version(conn) + 1, target + 1):
        with conn:
            MIGRATIONS[number - 1](conn)
            conn.execute('INSERT INTO schema_version (version) VALUES (?)', (number,))
    return current_version(conn)
