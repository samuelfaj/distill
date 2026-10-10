import unittest
from query import encode_query, redact_pairs


class QueryTests(unittest.TestCase):
    def test_encoding(self):
        self.assertEqual(encode_query([('z','a b'),('a','+')]), 'a=%2B&z=a%20b')

    def test_redaction(self):
        self.assertEqual(redact_pairs([('Token','secret'),('other','public')], ['token']),
                         [('Token','[REDACTED]'),('other','public')])
