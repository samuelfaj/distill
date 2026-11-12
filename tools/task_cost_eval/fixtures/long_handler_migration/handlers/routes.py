from framework import register

UNIT = 31


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'routes', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body}


register('routes.get', _get)
register('routes.fetch', _get)
