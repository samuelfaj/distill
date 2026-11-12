from framework import register

UNIT = 2


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'folders', 'total': request.get('qty', 1) * UNIT}
    return 200, body


register('folders.get', _get)
