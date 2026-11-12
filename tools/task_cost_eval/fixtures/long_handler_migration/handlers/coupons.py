from framework import register

UNIT = 10


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'coupons', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body}


register('coupons.get', _get)
register('coupons.fetch', _get)
