import sqlite3


def connect(path=':memory:'):
    conn = sqlite3.connect(path)
    conn.execute('PRAGMA foreign_keys = ON')
    return conn
