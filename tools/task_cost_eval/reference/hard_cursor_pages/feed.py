import base64
import binascii
import json

def _ordered(posts):
    """Newest first; posts with the same timestamp by id, highest first."""
    return sorted(posts, key=lambda post: (post['ts'], post['id']), reverse=True)



def _encode(post):
    return base64.urlsafe_b64encode(json.dumps([post['ts'], post['id']]).encode()).decode()


def _decode(cursor):
    try:
        ts, post_id = json.loads(base64.urlsafe_b64decode(cursor.encode()))
    except (binascii.Error, ValueError, TypeError, UnicodeError) as error:
        raise ValueError(f'malformed cursor: {cursor!r}') from error
    if type(ts) not in (int, float) or type(post_id) is not int:
        raise ValueError(f'malformed cursor: {cursor!r}')
    return ts, post_id


def page(posts, cursor=None, limit=20):
    """One page of the feed: {'items': [...], 'next_cursor': str or None}.

    The cursor is the position of the last post returned, so the next page continues
    after it whatever was added or removed in between.
    """
    ordered = _ordered(posts)
    if cursor is not None:
        after = _decode(cursor)
        ordered = [post for post in ordered if (post['ts'], post['id']) < after]
    items = ordered[:limit]
    more = len(ordered) > limit
    return {'items': items, 'next_cursor': _encode(items[-1]) if more else None}
