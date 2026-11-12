from framework import register

UNIT = 37


def _get(request):
    item_id = request.get('id', 0)
    body = {'id': item_id, 'kind': 'stocks', 'total': request.get('qty', 1) * UNIT}
    return {'status': 200, 'body': body, 'headers': {'Cache-Control': 'max-age=37'}}


register('stocks.get', _get)
