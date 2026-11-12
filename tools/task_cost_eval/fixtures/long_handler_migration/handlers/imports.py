from framework import register

UNIT = 34


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'imports', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body}


register('imports.get', _get)
register('imports.fetch', _get)
