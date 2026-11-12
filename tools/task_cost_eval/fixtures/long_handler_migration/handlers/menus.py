from framework import register

UNIT = 39


def _get(request):
    if 'id' not in request:
        return {'status': 404, 'body': {'error': 'missing id'}}
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'menus', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body}


register('menus.get', _get)
