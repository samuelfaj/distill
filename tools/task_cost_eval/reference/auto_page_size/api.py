from defaults import LIMITS
from paging import paginate


def _size(page_size):
    size = LIMITS['page_size'] if page_size is None else page_size
    if size < 1:
        raise ValueError('page_size starts at 1')
    return min(size, LIMITS['max_page_size'])


def list_items(store, page=1, page_size=None):
    return paginate(sorted(store['items']), page, _size(page_size))


def list_users(store, page=1, page_size=None):
    return paginate(sorted(store['users']), page, _size(page_size))
