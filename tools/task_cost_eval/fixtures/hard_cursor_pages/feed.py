def _ordered(posts):
    """Newest first; posts with the same timestamp by id, highest first."""
    return sorted(posts, key=lambda post: (post['ts'], post['id']), reverse=True)



def page(posts, cursor=None, limit=20):
    """One page of the feed: {'items': [...], 'next_cursor': str or None}."""
    ordered = _ordered(posts)
    start = int(cursor) if cursor else 0
    items = ordered[start:start + limit]
    more = start + limit < len(ordered)
    return {'items': items, 'next_cursor': str(start + limit) if more else None}
