import sqlite3

from migrations import email_key


def create_account(conn, email, created_at):
    try:
        with conn:
            cursor = conn.execute('INSERT INTO accounts (email, created_at, email_key) VALUES (?, ?, ?)',
                                  (email, created_at, email_key(email)))
    except sqlite3.IntegrityError as error:
        raise ValueError(f'an account already uses {email!r}') from error
    return cursor.lastrowid


def find_by_email(conn, email):
    row = conn.execute('SELECT id FROM accounts WHERE email_key = ?', (email_key(email),)).fetchone()
    return row[0] if row else None
