import unittest

import db
import migrations
from accounts import create_account, find_by_email
from orders import add_order, orders_for


class AccountTests(unittest.TestCase):
    def setUp(self):
        self.conn = db.connect()
        migrations.migrate(self.conn)

    def test_lookup_ignores_case_and_spaces(self):
        account = create_account(self.conn, 'Ana@Example.com', '2026-01-01T00:00:00')
        self.assertEqual(find_by_email(self.conn, ' ana@example.COM '), account)

    def test_orders(self):
        account = create_account(self.conn, 'bo@example.com', '2026-01-01T00:00:00')
        order = add_order(self.conn, account, 500)
        self.assertEqual(orders_for(self.conn, account), [order])
