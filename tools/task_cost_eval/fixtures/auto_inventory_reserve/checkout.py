from inventory import OutOfStock


def place_order(inventory, order_id, lines):
    try:
        inventory.reserve(order_id, lines)
    except OutOfStock as error:
        return {'order_id': order_id, 'status': 'rejected', 'sku': str(error)}
    return {'order_id': order_id, 'status': 'reserved'}
