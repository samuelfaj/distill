from defaults import LIMITS
from paging import paginate


def list_items(store, page=1):
    return paginate(sorted(store['items']), page, LIMITS['page_size'])


def list_users(store, page=1):
    return paginate(sorted(store['users']), page, LIMITS['page_size'])
