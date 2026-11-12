def create_account(conn, email, created_at):
    with conn:
        cursor = conn.execute('INSERT INTO accounts (email, created_at) VALUES (?, ?)', (email, created_at))
    return cursor.lastrowid


def find_by_email(conn, email):
    row = conn.execute('SELECT id FROM accounts WHERE email = ?', (email,)).fetchone()
    return row[0] if row else None
