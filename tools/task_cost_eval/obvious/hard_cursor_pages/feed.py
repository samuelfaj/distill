def _ordered(posts):
    """Newest first; posts with the same timestamp by id, highest first."""
    return sorted(posts, key=lambda post: (post['ts'], post['id']), reverse=True)



def page(posts, cursor=None, limit=20):
    """One page of the feed: {'items': [...], 'next_cursor': str or None}."""
    ordered = _ordered(posts)
    if cursor is not None:
        try:
            last_ts = int(cursor)
        except ValueError:
            raise ValueError(f'malformed cursor: {cursor!r}')
        ordered = [post for post in ordered if post['ts'] < last_ts]
    items = ordered[:limit]
    more = len(ordered) > limit
    return {'items': items, 'next_cursor': str(items[-1]['ts']) if more else None}
