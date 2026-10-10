import unittest
from api import list_documents, query


class LibraryTests(unittest.TestCase):
    def test_empty_legacy(self):
        self.assertEqual(list_documents([]), [])

    def test_query(self):
        docs = [{'id':'d','title':'Python tools','body':'tools','tags':['dev'],'rating':3,'archived':False}]
        self.assertEqual(query(docs, 'tools')['hits'][0]['score'], 3)
        self.assertEqual(query(docs, 'missing'), {'total':0,'hits':[]})
