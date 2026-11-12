from framework import register

UNIT = 18


def _get(request):
    if 'id' not in request:
        return {'status': 404, 'body': {'error': 'missing id'}}
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'vehicles', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body, 'headers': {'Cache-Control': 'max-age=18'}}


register('vehicles.get', _get)
register('vehicles.fetch', _get)
