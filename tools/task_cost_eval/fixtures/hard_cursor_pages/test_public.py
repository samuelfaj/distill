import unittest

from feed import page


def posts(*stamps):
    return [{'id': index, 'ts': ts} for index, ts in enumerate(stamps, 1)]


class FeedTests(unittest.TestCase):
    def test_walks_every_post_once(self):
        data = posts(*range(100, 125))
        seen, cursor = [], None
        while True:
            result = page(data, cursor, limit=10)
            seen += [post['id'] for post in result['items']]
            cursor = result['next_cursor']
            if cursor is None:
                break
        self.assertEqual(seen, list(range(25, 0, -1)))

    def test_new_posts_do_not_repeat_old_ones(self):
        data = posts(*range(100, 110))
        first = page(data, limit=4)
        data.append({'id': 99, 'ts': 500})
        second = page(data, first['next_cursor'], limit=4)
        self.assertEqual([post['id'] for post in second['items']], [6, 5, 4, 3])
