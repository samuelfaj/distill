def add_item(cart, name, qty):
    cart[name] = cart.get(name, 0) + qty
    return cart
