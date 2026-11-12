def add_order(conn, account_id, total_cents):
    with conn:
        return conn.execute('INSERT INTO orders (account_id, total_cents) VALUES (?, ?)',
                            (account_id, total_cents)).lastrowid


def orders_for(conn, account_id):
    rows = conn.execute('SELECT id FROM orders WHERE account_id = ? ORDER BY id', (account_id,)).fetchall()
    return [row[0] for row in rows]
