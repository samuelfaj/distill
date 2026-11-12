#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh

# Ids do not follow creation time: accounts 3 and 2 are the oldest of their groups.
ACCOUNTS = [(1, 'ana@x.com', '2024-05-01T10:00:00'), (2, 'Bob@Y.com', '2023-01-01T09:00:00'),
            (3, 'ANA@x.com', '2023-12-01T08:00:00'), (4, ' bob@y.com ', '2024-02-02T00:00:00'),
            (5, 'cy@z.com', '2022-01-01T00:00:00'), (6, 'Ana@X.com', '2025-01-01T00:00:00')]
ORDERS = [(10, 1, 100), (11, 2, 200), (12, 3, 300), (13, 4, 400), (14, 4, 450), (15, 5, 500), (16, 6, 600)]


def main(argv):
    failures = []
    try:
        db, migrations, accounts = import_fresh(fixture_dir(argv, 'hard_account_merge'), 'db', 'migrations', 'accounts')
    except Exception as error:
        return finish(['import: ' + repr(error)])

    def existing():
        conn = db.connect()
        migrations.migrate(conn, target=1)
        with conn:
            conn.executemany('INSERT INTO accounts (id, email, created_at) VALUES (?, ?, ?)', ACCOUNTS)
            conn.executemany('INSERT INTO orders (id, account_id, total_cents) VALUES (?, ?, ?)', ORDERS)
        migrations.migrate(conn)
        return conn

    def state(conn):
        return (conn.execute('SELECT id, email FROM accounts ORDER BY id').fetchall(),
                conn.execute('SELECT id, account_id, total_cents FROM orders ORDER BY id').fetchall())

    merged = ([(2, 'Bob@Y.com'), (3, 'ANA@x.com'), (5, 'cy@z.com')],
              [(10, 3, 100), (11, 2, 200), (12, 3, 300), (13, 2, 400), (14, 2, 450), (15, 5, 500), (16, 3, 600)])
    attempt(failures, 'version after migrating existing data', lambda: migrations.current_version(existing()), 2)
    attempt(failures, 'duplicates merged into the account created first', lambda: state(existing()), merged)

    def rerun():
        conn = existing()
        migrations.migrate(conn)
        return migrations.current_version(conn), state(conn)
    attempt(failures, 'running migrate again changes nothing', rerun, (2, merged))
    attempt(failures, 'lookup after merge', lambda: [accounts.find_by_email(existing(), email) for email in
                                                     (' ana@X.COM ', 'BOB@y.com', 'nobody@z.com')], [3, 2, None])
    attempt(failures, 'duplicate email rejected', lambda: accounts.create_account(existing(), 'CY@z.com ', '2026-01-01'),
            raises=ValueError)

    def new_account():
        conn = existing()
        created = accounts.create_account(conn, 'Dee@W.com', '2026-01-01T00:00:00')
        return accounts.find_by_email(conn, 'dee@w.COM') == created

    attempt(failures, 'new account after merge', new_account, True)

    def fresh():
        conn = db.connect()
        migrations.migrate(conn)
        created = accounts.create_account(conn, 'Eve@V.com', '2026-01-01T00:00:00')
        return migrations.current_version(conn), accounts.find_by_email(conn, ' EVE@v.com') == created
    attempt(failures, 'fresh database', fresh, (2, True))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
